use crate::cypher::{
    execute_cypher, executor::ExecContext, parse_cache::ParseCache, plan_cache::PlanCache,
    record::NamedRecord, ExecCaches,
};
use crate::edge;
use crate::fts;
use crate::index;
use crate::node;
use crate::types::{
    validate_properties, Direction, GraphError, Node, NodeId, Properties, Result, Value,
};
use std::sync::atomic::{AtomicU64, Ordering};

/// Shared read operations — implemented identically on both transaction types.
macro_rules! impl_read_ops {
    ($ty:ident, $read_only:expr) => {
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
            pub fn query(&self, cypher: &str) -> Result<Vec<NamedRecord>> {
                self.query_with_params(cypher, None)
            }

            /// Execute a Cypher query with optional parameter substitution.
            pub fn query_with_params(
                &self,
                cypher: &str,
                params: Option<&std::collections::HashMap<String, Value>>,
            ) -> Result<Vec<NamedRecord>> {
                let ctx = ExecContext {
                    max_result_rows: self.max_result_rows,
                    max_traversal_depth: self.max_traversal_depth,
                    max_traversal_work: self.max_traversal_work,
                    require_read_only: $read_only,
                    ..Default::default()
                };
                execute_cypher(&self.tx, cypher, params, ctx, self.exec_caches())
            }

            /// Execute a Cypher query with optional parameters and procedure registry.
            pub fn query_with_procedures(
                &self,
                cypher: &str,
                params: Option<&std::collections::HashMap<String, Value>>,
                procedures: &crate::cypher::procedure::ProcedureRegistry,
            ) -> Result<Vec<NamedRecord>> {
                let ctx = ExecContext {
                    max_result_rows: self.max_result_rows,
                    max_traversal_depth: self.max_traversal_depth,
                    max_traversal_work: self.max_traversal_work,
                    procedures: procedures.clone(),
                    require_read_only: $read_only,
                    ..Default::default()
                };
                execute_cypher(&self.tx, cypher, params, ctx, self.exec_caches())
            }
        }
    };
}

/// A read-only transaction. Provides snapshot isolation via SQLite's WAL.
///
/// Not nameable from outside the crate — `mod transaction` is private and the
/// type is not re-exported. Callers reach its methods through `Deref` on
/// [`ReadTxGuard`].
pub struct ReadTransaction<'a> {
    tx: rusqlite::Transaction<'a>,
    max_result_rows: usize,
    max_traversal_depth: u32,
    max_traversal_work: u64,
    parse_cache: &'a ParseCache,
    plan_cache: &'a PlanCache,
    schema_epoch: &'a AtomicU64,
}

impl<'a> ReadTransaction<'a> {
    pub(crate) fn new(
        tx: rusqlite::Transaction<'a>,
        max_result_rows: usize,
        max_traversal_depth: u32,
        max_traversal_work: u64,
        parse_cache: &'a ParseCache,
        plan_cache: &'a PlanCache,
        schema_epoch: &'a AtomicU64,
    ) -> Self {
        Self {
            tx,
            max_result_rows,
            max_traversal_depth,
            max_traversal_work,
            parse_cache,
            plan_cache,
            schema_epoch,
        }
    }

    fn exec_caches(&self) -> ExecCaches<'_> {
        ExecCaches {
            parse: Some(self.parse_cache),
            plan: Some(self.plan_cache),
            schema_epoch: Some(self.schema_epoch),
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

impl_read_ops!(ReadTransaction, true);

/// A read-write transaction. Acquires the write lock via BEGIN IMMEDIATE.
///
/// Not nameable from outside the crate — `mod transaction` is private and the
/// type is not re-exported. Callers reach its methods through `Deref` on
/// [`WriteTxGuard`].
pub struct WriteTransaction<'a> {
    tx: rusqlite::Transaction<'a>,
    max_property_value_bytes: usize,
    max_name_bytes: usize,
    max_result_rows: usize,
    max_traversal_depth: u32,
    max_traversal_work: u64,
    parse_cache: &'a ParseCache,
    plan_cache: &'a PlanCache,
    schema_epoch: &'a AtomicU64,
}

impl<'a> WriteTransaction<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tx: rusqlite::Transaction<'a>,
        max_property_value_bytes: usize,
        max_name_bytes: usize,
        max_result_rows: usize,
        max_traversal_depth: u32,
        max_traversal_work: u64,
        parse_cache: &'a ParseCache,
        plan_cache: &'a PlanCache,
        schema_epoch: &'a AtomicU64,
    ) -> Self {
        Self {
            tx,
            max_property_value_bytes,
            max_name_bytes,
            max_result_rows,
            max_traversal_depth,
            max_traversal_work,
            parse_cache,
            plan_cache,
            schema_epoch,
        }
    }

    fn exec_caches(&self) -> ExecCaches<'_> {
        ExecCaches {
            parse: Some(self.parse_cache),
            plan: Some(self.plan_cache),
            schema_epoch: Some(self.schema_epoch),
        }
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
        fts::update_fts_for_node(&self.tx, id, primary_label, None, &properties)?;
        Ok(id)
    }

    /// Delete a node and all its edges.
    pub fn delete_node(&self, id: NodeId) -> Result<()> {
        let n = node::get_node(&self.tx, id)?;
        let primary_label = n.labels.first().map(|s| s.as_str()).unwrap_or("");
        index::remove_indexes_for_node(&self.tx, id, primary_label, &n.properties)?;
        fts::remove_fts_for_node(&self.tx, id, primary_label, &n.properties)?;
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
        fts::update_fts_for_node(
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
        fts::update_fts_for_node(
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
        index::create_index(&self.tx, label, property)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Drop a secondary index.
    pub fn drop_index(&self, label: &str, property: &str) -> Result<()> {
        index::drop_index(&self.tx, label, property)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Create a fulltext index on `(label, property)`. See
    /// `Database::create_fulltext_index` for semantics.
    pub fn create_fulltext_index(&self, label: &str, property: &str) -> Result<()> {
        fts::create_fulltext_index(&self.tx, label, property)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Create a case-insensitive fulltext index. See
    /// `Database::create_fulltext_index_ci` for semantics.
    pub fn create_fulltext_index_ci(&self, label: &str, property: &str) -> Result<()> {
        fts::create_fulltext_index_ci(&self.tx, label, property)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Create a word-tokenized fulltext index. See
    /// `Database::create_fulltext_index_word` for semantics.
    pub fn create_fulltext_index_word(&self, label: &str, property: &str) -> Result<()> {
        fts::create_fulltext_index_word(&self.tx, label, property)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Multi-property variant. See
    /// `Database::create_fulltext_index_word_multi` for semantics.
    pub fn create_fulltext_index_word_multi(
        &self,
        label: &str,
        properties: &[String],
    ) -> Result<()> {
        fts::create_fulltext_index_word_multi(&self.tx, label, properties)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Drop a fulltext index on `(label, property)`. See
    /// `Database::drop_fulltext_index` for semantics.
    pub fn drop_fulltext_index(&self, label: &str, property: &str) -> Result<()> {
        fts::drop_fulltext_index(&self.tx, label, property)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
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

impl_read_ops!(WriteTransaction, false);

// ----------------------------------------------------------------------------
// WriteTxGuard / ReadTxGuard — RAII wrappers that auto-roll-back on drop.
//
// Wrap the crate-internal `WriteTransaction` / `ReadTransaction`. Forward the
// full typed-Tx API via `Deref`/`DerefMut` so callers can keep using
// `tx.create_node(...)`, `tx.query(...)`, etc. — the inner Tx types are
// `pub(crate)` (unnameable from outside), but their `pub` methods remain
// reachable through deref. Inherent `commit(self)` / `rollback(self)` consume
// the guard; otherwise `Drop` emits a `tracing::warn!` and lets rusqlite's own
// `Transaction` Drop perform the actual rollback.
// ----------------------------------------------------------------------------

/// RAII guard for a write transaction. Returned by [`crate::Database::write_tx`].
///
/// Drops trigger a rollback unless [`commit`](Self::commit) is called.
/// Methods on the wrapped transaction (`create_node`, `query`, etc.) are
/// accessible directly via `Deref`.
pub struct WriteTxGuard<'a> {
    inner: Option<WriteTransaction<'a>>,
}

/// RAII guard for a read transaction. Returned by [`crate::Database::read_tx`].
///
/// Drops release the snapshot. Methods on the wrapped transaction (`query`,
/// `get_node`, etc.) are accessible directly via `Deref`.
pub struct ReadTxGuard<'a> {
    inner: Option<ReadTransaction<'a>>,
}

impl<'a> WriteTxGuard<'a> {
    pub(crate) fn new(tx: WriteTransaction<'a>) -> Self {
        Self { inner: Some(tx) }
    }

    /// Commit the wrapped write transaction. Disarms the drop-rollback.
    pub fn commit(mut self) -> Result<()> {
        self.inner
            .take()
            .expect("WriteTxGuard already finalized")
            .commit()
    }

    /// Roll back the wrapped write transaction explicitly. Disarms the
    /// drop-rollback (which would otherwise have rolled back anyway).
    pub fn rollback(mut self) -> Result<()> {
        self.inner
            .take()
            .expect("WriteTxGuard already finalized")
            .rollback()
    }
}

impl<'a> ReadTxGuard<'a> {
    pub(crate) fn new(tx: ReadTransaction<'a>) -> Self {
        Self { inner: Some(tx) }
    }

    /// Release the snapshot held by the wrapped read transaction.
    pub fn commit(mut self) -> Result<()> {
        self.inner
            .take()
            .expect("ReadTxGuard already finalized")
            .commit()
    }

    /// Explicitly end the wrapped read transaction. Read-only txns have no
    /// writes to revert, so this is equivalent to dropping the guard.
    pub fn rollback(mut self) -> Result<()> {
        self.inner
            .take()
            .expect("ReadTxGuard already finalized")
            .rollback()
    }
}

impl<'a> std::ops::Deref for WriteTxGuard<'a> {
    type Target = WriteTransaction<'a>;
    fn deref(&self) -> &WriteTransaction<'a> {
        // Inner is only `None` after `commit`/`rollback`, both of which consume
        // `self` — so any reachable `&WriteTxGuard` has `Some(inner)`.
        self.inner
            .as_ref()
            .expect("WriteTxGuard accessed after commit or rollback")
    }
}

impl<'a> std::ops::DerefMut for WriteTxGuard<'a> {
    fn deref_mut(&mut self) -> &mut WriteTransaction<'a> {
        self.inner
            .as_mut()
            .expect("WriteTxGuard accessed after commit or rollback")
    }
}

impl<'a> std::ops::Deref for ReadTxGuard<'a> {
    type Target = ReadTransaction<'a>;
    fn deref(&self) -> &ReadTransaction<'a> {
        self.inner
            .as_ref()
            .expect("ReadTxGuard accessed after commit or rollback")
    }
}

impl<'a> std::ops::DerefMut for ReadTxGuard<'a> {
    fn deref_mut(&mut self) -> &mut ReadTransaction<'a> {
        self.inner
            .as_mut()
            .expect("ReadTxGuard accessed after commit or rollback")
    }
}

impl Drop for WriteTxGuard<'_> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            tracing::warn!(
                "WriteTxGuard dropped without commit or rollback; transaction will be rolled back. \
                 Call `tx.commit()` to persist writes, or `tx.rollback()` to silence this warning."
            );
        }
    }
}

impl Drop for ReadTxGuard<'_> {
    fn drop(&mut self) {
        // Read txns have no writes to lose; dropping silently releases the
        // snapshot. No warning to emit.
    }
}

#[cfg(test)]
mod tx_guard_tests {
    use crate::{Database, Value};

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

    #[test]
    fn write_tx_create_and_drop_fulltext_index() {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.create_fulltext_index("Doc", "body").unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = db.write_tx().unwrap();
            tx.query("CREATE (n:Doc {body: 'hello world'})").unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = db.write_tx().unwrap();
            tx.drop_fulltext_index("Doc", "body").unwrap();
            tx.commit().unwrap();
        }
    }

    #[test]
    fn write_tx_create_and_drop_fulltext_index_ci() {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.create_fulltext_index_ci("Doc", "body").unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = db.write_tx().unwrap();
            tx.query("CREATE (n:Doc {body: 'Hello World'})").unwrap();
            tx.commit().unwrap();
        }
        {
            // Case-insensitive: lowercase substring matches mixed-case content.
            let tx = db.read_tx().unwrap();
            let rows = tx
                .query("MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n.body AS body")
                .unwrap();
            assert_eq!(rows.len(), 1);
        }
        {
            let tx = db.write_tx().unwrap();
            tx.drop_fulltext_index("Doc", "body").unwrap();
            tx.commit().unwrap();
        }
    }

    #[test]
    fn write_tx_create_fulltext_index_word() {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.create_fulltext_index_word("Doc", "body").unwrap();
            tx.commit().unwrap();
        }
        // Verify via introspection — should report `fulltext_word` kind.
        let rows = db
            .execute("CALL db.indexes() YIELD label, property, kind RETURN kind")
            .unwrap();
        let kinds: Vec<String> = rows
            .iter()
            .map(|r| match r.get("kind").unwrap() {
                Value::String(s) => s.clone(),
                v => panic!("kind was {v:?}"),
            })
            .collect();
        assert_eq!(kinds, vec!["fulltext_word".to_string()]);
    }

    #[test]
    fn write_tx_create_fulltext_index_word_multi() {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.create_fulltext_index_word_multi(
                "Article",
                &["title".to_string(), "body".to_string()],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let rows = db
            .execute(
                "CALL db.indexes() YIELD label, property, kind RETURN property ORDER BY property",
            )
            .unwrap();
        let props: Vec<String> = rows
            .iter()
            .map(|r| match r.get("property").unwrap() {
                Value::String(s) => s.clone(),
                v => panic!("property was {v:?}"),
            })
            .collect();
        assert_eq!(props, vec!["body".to_string(), "title".to_string()]);
    }
}
