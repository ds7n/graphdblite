use crate::cypher::{
    ast::Statement,
    cost,
    executor::{self, ExecContext},
    parser, planner,
    record::Record,
};
use crate::edge;
use crate::index;
use crate::node;
use crate::types::{
    validate_properties, Direction, GraphError, Node, NodeId, Properties, Result, Value,
};

/// Shared read operations — implemented identically on both transaction types.
macro_rules! impl_read_ops {
    ($ty:ident) => {
        impl<'a> $ty<'a> {
            /// Get a node by ID.
            pub fn get_node(&self, id: NodeId) -> Result<Node> {
                node::get_node(&self.tx, id)
            }

            /// Check if a node exists.
            pub fn node_exists(&self, id: NodeId) -> Result<bool> {
                node::node_exists(&self.tx, id)
            }

            /// Get neighbor node IDs.
            pub fn get_neighbors(
                &self,
                id: NodeId,
                label: &str,
                direction: Direction,
            ) -> Result<Vec<NodeId>> {
                edge::get_neighbors(&self.tx, id, label, direction)
            }

            /// Get edge properties.
            pub fn get_edge_properties(
                &self,
                src: NodeId,
                dst: NodeId,
                label: &str,
            ) -> Result<Properties> {
                edge::get_edge_properties(&self.tx, src, dst, label)
            }

            /// Find nodes by label (full scan).
            pub fn find_nodes_by_label(&self, label: &str) -> Result<Vec<Node>> {
                node::find_nodes_by_label(&self.tx, label)
            }

            /// Lookup nodes via secondary index.
            pub fn index_lookup(
                &self,
                label: &str,
                property: &str,
                value: &Value,
            ) -> Result<Vec<NodeId>> {
                index::index_lookup(&self.tx, label, property, value)
            }

            /// Variable-length path traversal (BFS).
            pub fn traverse(
                &self,
                start: NodeId,
                label: &str,
                direction: Direction,
                min_hops: u32,
                max_hops: u32,
            ) -> Result<Vec<NodeId>> {
                edge::traverse(&self.tx, start, label, direction, min_hops, max_hops)
            }

            /// Variable-length path traversal (BFS) with depth tracking.
            ///
            /// Returns `(node_id, depth)` pairs ordered by depth (closest first).
            pub fn traverse_with_depth(
                &self,
                start: NodeId,
                label: &str,
                direction: Direction,
                min_hops: u32,
                max_hops: u32,
            ) -> Result<Vec<(NodeId, u32)>> {
                edge::traverse_with_depth(&self.tx, start, label, direction, min_hops, max_hops)
            }

            /// Execute a Cypher query string and return result records.
            pub fn query(&self, cypher: &str) -> Result<Vec<Record>> {
                self.query_with_params(cypher, None)
            }

            /// Execute a Cypher query with optional parameter substitution.
            pub fn query_with_params(
                &self,
                cypher: &str,
                params: Option<&std::collections::HashMap<String, Value>>,
            ) -> Result<Vec<Record>> {
                let mut stmt = parser::parse(cypher)?;
                if let Some(p) = params {
                    stmt = parser::resolve_params(&stmt, p)?;
                }
                let plan = planner::plan(&self.tx, &stmt)?;
                if matches!(stmt, Statement::Explain(_)) {
                    return Ok(cost::format_explain(&self.tx, &plan));
                }
                let ctx = ExecContext {
                    max_result_rows: self.max_result_rows,
                    ..Default::default()
                };
                executor::execute_with_ctx(&self.tx, &plan, &ctx)
            }

            /// Execute a Cypher query with optional parameters and procedure registry.
            pub fn query_with_procedures(
                &self,
                cypher: &str,
                params: Option<&std::collections::HashMap<String, Value>>,
                procedures: &crate::cypher::procedure::ProcedureRegistry,
            ) -> Result<Vec<Record>> {
                let mut stmt = parser::parse(cypher)?;
                if let Some(p) = params {
                    stmt = parser::resolve_params(&stmt, p)?;
                }
                let plan = planner::plan_with_procedures(&self.tx, &stmt, procedures, params)?;
                if matches!(stmt, Statement::Explain(_)) {
                    return Ok(cost::format_explain(&self.tx, &plan));
                }
                let ctx = ExecContext {
                    max_result_rows: self.max_result_rows,
                    procedures: procedures.clone(),
                };
                executor::execute_with_ctx(&self.tx, &plan, &ctx)
            }
        }
    };
}

/// A read-only transaction. Provides snapshot isolation via SQLite's WAL.
pub struct ReadTransaction<'a> {
    tx: rusqlite::Transaction<'a>,
    max_result_rows: usize,
}

impl<'a> ReadTransaction<'a> {
    pub(crate) fn new(tx: rusqlite::Transaction<'a>, max_result_rows: usize) -> Self {
        Self {
            tx,
            max_result_rows,
        }
    }

    /// Commit the read transaction (releases snapshot).
    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}

impl_read_ops!(ReadTransaction);

/// A read-write transaction. Acquires the write lock via BEGIN IMMEDIATE.
pub struct WriteTransaction<'a> {
    tx: rusqlite::Transaction<'a>,
    max_property_value_bytes: usize,
    max_name_bytes: usize,
    max_result_rows: usize,
}

impl<'a> WriteTransaction<'a> {
    pub(crate) fn new(
        tx: rusqlite::Transaction<'a>,
        max_property_value_bytes: usize,
        max_name_bytes: usize,
        max_result_rows: usize,
    ) -> Self {
        Self {
            tx,
            max_property_value_bytes,
            max_name_bytes,
            max_result_rows,
        }
    }

    /// Access the underlying SQLite connection for direct operations.
    pub fn connection(&self) -> &rusqlite::Connection {
        &self.tx
    }

    // --- Write operations ---

    /// Create a new node with a single label.
    pub fn create_node(&self, label: &str, properties: Properties) -> Result<NodeId> {
        let labels = if label.is_empty() {
            vec![]
        } else {
            vec![label.to_string()]
        };
        self.create_node_with_labels(&labels, properties)
    }

    /// Create a new node with multiple labels.
    pub fn create_node_with_labels(
        &self,
        labels: &[String],
        properties: Properties,
    ) -> Result<NodeId> {
        let primary_label = labels.first().map(|s| s.as_str()).unwrap_or("");
        validate_properties(
            primary_label,
            &properties,
            self.max_name_bytes,
            self.max_property_value_bytes,
        )?;
        let id = node::create_node(&self.tx, labels, properties.clone())?;
        index::update_indexes_for_node(&self.tx, id, primary_label, None, &properties)?;
        Ok(id)
    }

    /// Delete a node and all its edges.
    pub fn delete_node(&self, id: NodeId) -> Result<()> {
        let n = node::get_node(&self.tx, id)?;
        let primary_label = n.labels.first().map(|s| s.as_str()).unwrap_or("");
        index::remove_indexes_for_node(&self.tx, id, primary_label, &n.properties)?;
        node::delete_node(&self.tx, id)
    }

    /// Set a property on a node.
    pub fn set_node_property(&self, id: NodeId, key: &str, value: Value) -> Result<()> {
        let props = std::collections::HashMap::from([(key.to_string(), value.clone())]);
        validate_properties(
            "_",
            &props,
            self.max_name_bytes,
            self.max_property_value_bytes,
        )?;
        let old = node::get_node(&self.tx, id)?;
        node::set_node_property(&self.tx, id, key, value.clone())?;
        let mut new_props = old.properties.clone();
        new_props.insert(key.to_string(), value);
        index::update_indexes_for_node(
            &self.tx,
            id,
            old.labels.first().map(|s| s.as_str()).unwrap_or(""),
            Some(&old.properties),
            &new_props,
        )?;
        Ok(())
    }

    /// Remove a property from a node.
    pub fn remove_node_property(&self, id: NodeId, key: &str) -> Result<()> {
        let old = node::get_node(&self.tx, id)?;
        node::remove_node_property(&self.tx, id, key)?;
        let mut new_props = old.properties.clone();
        new_props.remove(key);
        index::update_indexes_for_node(
            &self.tx,
            id,
            old.labels.first().map(|s| s.as_str()).unwrap_or(""),
            Some(&old.properties),
            &new_props,
        )?;
        Ok(())
    }

    /// Create an edge.
    pub fn create_edge(
        &self,
        src: NodeId,
        dst: NodeId,
        label: &str,
        properties: Properties,
    ) -> Result<()> {
        validate_properties(
            label,
            &properties,
            self.max_name_bytes,
            self.max_property_value_bytes,
        )?;
        // Verify both endpoints exist.
        if !node::node_exists(&self.tx, src)? {
            return Err(GraphError::NodeNotFound(src));
        }
        if !node::node_exists(&self.tx, dst)? {
            return Err(GraphError::NodeNotFound(dst));
        }
        edge::create_edge(&self.tx, src, dst, label, properties)
    }

    /// Delete an edge.
    pub fn delete_edge(&self, src: NodeId, dst: NodeId, label: &str) -> Result<()> {
        edge::delete_edge(&self.tx, src, dst, label)
    }

    /// Create a secondary index.
    pub fn create_index(&self, label: &str, property: &str) -> Result<()> {
        index::create_index(&self.tx, label, property)
    }

    /// Drop a secondary index.
    pub fn drop_index(&self, label: &str, property: &str) -> Result<()> {
        index::drop_index(&self.tx, label, property)
    }

    /// Commit the transaction.
    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }

    /// Rollback the transaction (also happens on drop).
    pub fn rollback(self) -> Result<()> {
        self.tx.rollback()?;
        Ok(())
    }
}

impl_read_ops!(WriteTransaction);
