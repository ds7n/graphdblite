use crate::cypher::{execute_cypher, executor::ExecContext, record::Record};
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
                edge::traverse(&self.tx, start, label, direction, min_hops, max_hops, None)
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
                edge::traverse_with_depth(
                    &self.tx, start, label, direction, min_hops, max_hops, None,
                )
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
                let ctx = ExecContext {
                    max_result_rows: self.max_result_rows,
                    max_traversal_depth: self.max_traversal_depth,
                    max_traversal_work: self.max_traversal_work,
                    ..Default::default()
                };
                execute_cypher(&self.tx, cypher, params, ctx)
            }

            /// Execute a Cypher query with optional parameters and procedure registry.
            pub fn query_with_procedures(
                &self,
                cypher: &str,
                params: Option<&std::collections::HashMap<String, Value>>,
                procedures: &crate::cypher::procedure::ProcedureRegistry,
            ) -> Result<Vec<Record>> {
                let ctx = ExecContext {
                    max_result_rows: self.max_result_rows,
                    max_traversal_depth: self.max_traversal_depth,
                    max_traversal_work: self.max_traversal_work,
                    procedures: procedures.clone(),
                };
                execute_cypher(&self.tx, cypher, params, ctx)
            }
        }
    };
}

/// A read-only transaction. Provides snapshot isolation via SQLite's WAL.
pub struct ReadTransaction<'a> {
    tx: rusqlite::Transaction<'a>,
    max_result_rows: usize,
    max_traversal_depth: u32,
    max_traversal_work: u64,
}

impl<'a> ReadTransaction<'a> {
    pub(crate) fn new(
        tx: rusqlite::Transaction<'a>,
        max_result_rows: usize,
        max_traversal_depth: u32,
        max_traversal_work: u64,
    ) -> Self {
        Self {
            tx,
            max_result_rows,
            max_traversal_depth,
            max_traversal_work,
        }
    }

    /// Commit the read transaction (releases snapshot).
    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }

    /// Roll back the read transaction (also happens on drop).
    /// Read-only txns have no writes to revert; this just releases the snapshot.
    pub fn rollback(self) -> Result<()> {
        self.tx.rollback()?;
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
    max_traversal_depth: u32,
    max_traversal_work: u64,
}

impl<'a> WriteTransaction<'a> {
    pub(crate) fn new(
        tx: rusqlite::Transaction<'a>,
        max_property_value_bytes: usize,
        max_name_bytes: usize,
        max_result_rows: usize,
        max_traversal_depth: u32,
        max_traversal_work: u64,
    ) -> Self {
        Self {
            tx,
            max_property_value_bytes,
            max_name_bytes,
            max_result_rows,
            max_traversal_depth,
            max_traversal_work,
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
            return Err(GraphError::NodeNotFound {
                id: src,
                hint: None,
            });
        }
        if !node::node_exists(&self.tx, dst)? {
            return Err(GraphError::NodeNotFound {
                id: dst,
                hint: None,
            });
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

// ----------------------------------------------------------------------------
// TxGuard — RAII wrapper that auto-rolls-back on drop.
//
// Wraps a typed `ReadTransaction` or `WriteTransaction`. Forwards the full
// typed-Tx API via `Deref`/`DerefMut` so existing callers do not change. The
// guard's inherent `commit(self)` / `rollback(self)` consume the guard and the
// inner Tx; if neither is called, `Drop` emits a `tracing::warn!` and lets
// rusqlite's own `Transaction` Drop perform the actual rollback.
// ----------------------------------------------------------------------------

/// RAII transaction guard. Returned by `Database::read_tx` / `write_tx`.
pub struct TxGuard<T> {
    inner: Option<T>,
}

impl<T> TxGuard<T> {
    pub(crate) fn new(tx: T) -> Self {
        Self { inner: Some(tx) }
    }
}

impl<T> std::ops::Deref for TxGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Inner is only `None` after `commit`/`rollback`, both of which consume
        // `self` — so any reachable `&TxGuard` has `Some(inner)`.
        self.inner
            .as_ref()
            .expect("TxGuard accessed after commit or rollback")
    }
}

impl<T> std::ops::DerefMut for TxGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.inner
            .as_mut()
            .expect("TxGuard accessed after commit or rollback")
    }
}

impl<'a> TxGuard<WriteTransaction<'a>> {
    /// Commit the wrapped write transaction. Disarms the drop-rollback.
    pub fn commit(mut self) -> Result<()> {
        self.inner
            .take()
            .expect("TxGuard already finalized")
            .commit()
    }

    /// Roll back the wrapped write transaction explicitly. Disarms the
    /// drop-rollback (which would otherwise have rolled back anyway).
    pub fn rollback(mut self) -> Result<()> {
        self.inner
            .take()
            .expect("TxGuard already finalized")
            .rollback()
    }
}

impl<'a> TxGuard<ReadTransaction<'a>> {
    /// Commit (release snapshot for) the wrapped read transaction.
    pub fn commit(mut self) -> Result<()> {
        self.inner
            .take()
            .expect("TxGuard already finalized")
            .commit()
    }

    /// Explicitly end the wrapped read transaction. Read-only txns have no
    /// writes to revert, so this is equivalent to dropping the guard.
    pub fn rollback(mut self) -> Result<()> {
        self.inner
            .take()
            .expect("TxGuard already finalized")
            .rollback()
    }
}

impl<T> Drop for TxGuard<T> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            tracing::warn!(
                "TxGuard dropped without commit or rollback; transaction will be rolled back. \
                 Call `tx.commit()` to persist writes, or `tx.rollback()` to silence this warning."
            );
            // Inner `WriteTransaction`/`ReadTransaction` drops here; rusqlite's
            // own `Transaction` `Drop` performs the actual ROLLBACK.
        }
    }
}

#[cfg(test)]
mod tx_guard_tests {
    use crate::Database;

    #[test]
    fn explicit_commit_persists() {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.query("CREATE (:Person {name: 'Alice'})").unwrap();
            tx.commit().unwrap();
        }
        let tx = db.read_tx().unwrap();
        let rows = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(rows.len(), 1);
        tx.commit().unwrap();
    }

    #[test]
    fn forgot_commit_rolls_back() {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.query("CREATE (:Person {name: 'Bob'})").unwrap();
            // dropped without commit
        }
        let tx = db.read_tx().unwrap();
        let rows = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(rows.len(), 0);
        tx.commit().unwrap();
    }

    #[test]
    fn explicit_rollback_discards() {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.query("CREATE (:Person {name: 'Carol'})").unwrap();
            tx.rollback().unwrap();
        }
        let tx = db.read_tx().unwrap();
        let rows = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(rows.len(), 0);
        tx.commit().unwrap();
    }

    #[test]
    fn panic_mid_txn_rolls_back() {
        let mut db = Database::open_memory().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let tx = db.write_tx().unwrap();
            tx.query("CREATE (:Person {name: 'Dan'})").unwrap();
            panic!("simulated failure");
        }));
        assert!(result.is_err());
        // After panic-driven unwind, the guard was dropped → rollback.
        let tx = db.read_tx().unwrap();
        let rows = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(rows.len(), 0);
        tx.commit().unwrap();
    }

    #[test]
    fn deref_exposes_typed_methods() {
        // Sanity: method resolution through Deref still finds typed-Tx
        // CRUD helpers and `query_with_params`.
        let mut db = Database::open_memory().unwrap();
        let tx = db.write_tx().unwrap();
        let id = tx
            .create_node("Person", std::collections::HashMap::new())
            .unwrap();
        let n = tx.get_node(id).unwrap();
        assert_eq!(n.id, id);
        tx.commit().unwrap();
    }

    #[test]
    fn typed_guard_blocks_when_stateful_txn_active() {
        // Mixing stateful `begin_*` with typed `*_tx` guards is rejected at
        // runtime to keep the SQLite layer from silently failing on a nested
        // BEGIN IMMEDIATE.
        let mut db = Database::open_memory().unwrap();
        db.begin_write().unwrap();
        assert!(db.write_tx().is_err());
        assert!(db.read_tx().is_err());
        db.rollback().unwrap();
    }
}
