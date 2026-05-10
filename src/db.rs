use std::fmt;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use std::sync::atomic::{AtomicU64, Ordering};

use crate::cypher::{execute_cypher, executor::ExecContext, record::NamedRecord, ExecCaches};
use crate::schema;
use crate::transaction::{ReadTransaction, ReadTxGuard, WriteTransaction, WriteTxGuard};
use crate::types::{GraphError, Result, Value};

/// SQLite `application_id` value identifying this file as a graphdblite
/// database. Stored as a big-endian 32-bit integer at offset 68 of the
/// database header (per the SQLite file format spec). ASCII bytes are
/// `G`, `D`, `B`, `L` — chosen to match the uppercase convention of
/// existing entries in SQLite's `magic.txt` registry (`GPKG`, `GP10`,
/// `MPBX`, …). See `docs/sqlite-application-id.md` for the upstream
/// registration plan.
const GRAPHDBLITE_APPLICATION_ID: i32 = 0x4744_424C;

/// On-disk schema version. Bumped only when an incompatible storage-layout
/// change requires migration. Read via `PRAGMA user_version`. A graphdblite
/// build refuses to open a file whose `user_version` is greater than this
/// constant — it was written by a newer release that may have introduced
/// layout changes the older code can't safely interpret.
const GRAPHDBLITE_SCHEMA_VERSION: i32 = 1;

/// State of the stateful transaction lifecycle on a `Database` handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxState {
    None,
    Read,
    Write,
}

/// SQLite synchronous PRAGMA mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum SyncMode {
    Off,
    Normal,
    Full,
    Extra,
}

impl fmt::Display for SyncMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncMode::Off => write!(f, "OFF"),
            SyncMode::Normal => write!(f, "NORMAL"),
            SyncMode::Full => write!(f, "FULL"),
            SyncMode::Extra => write!(f, "EXTRA"),
        }
    }
}

/// Configuration for opening a database.
pub struct Config {
    /// SQLite busy timeout in milliseconds. Default: 5000.
    pub busy_timeout_ms: u32,
    /// SQLite synchronous mode. Default: Normal (WAL-safe).
    pub synchronous: SyncMode,
    /// Maximum hop count for variable-length / fixed-length pattern traversal.
    /// Default: 64. Set to 0 to disable. Var-length traversal cost grows
    /// roughly as `O(branching_factor ^ max_hops)`, so a too-large value can
    /// OOM the host on dense graphs.
    pub max_traversal_depth: u32,
    /// Maximum total edge-visit budget (DFS "fuel") for a single var-length
    /// traversal. Default: 10,000,000. Set to 0 to disable. This is the
    /// runtime sibling to `max_traversal_depth`: even within the hop cap,
    /// dense graphs can produce factorial path enumeration that pegs CPU
    /// long before result limits would stop it. Exceeding this returns
    /// `GraphError::SizeLimit`.
    pub max_traversal_work: u64,
    /// Maximum byte length for a single property value. Default: 1 MiB.
    pub max_property_value_bytes: usize,
    /// Maximum byte length for label and property key names. Default: 256.
    pub max_name_bytes: usize,
    /// Maximum number of result rows before the executor aborts. Default: 100,000.
    /// Set to 0 to disable the limit.
    pub max_result_rows: usize,
    /// SQLite page cache size in KiB (negative = KiB, positive = pages).
    /// Default: -32000 (32 MiB). Larger values improve graph traversal.
    pub cache_size: i32,
    /// SQLite mmap_size in bytes. Default: 268435456 (256 MiB).
    /// Memory-mapped I/O improves read-heavy workloads. Set to 0 to disable.
    ///
    /// **Note**: the default reserves 256 MiB of address space per open
    /// `Database` handle. On 32-bit targets, sandboxed environments with
    /// strict address-space limits (e.g. seccomp/RLIMIT_AS), or hosts running
    /// many concurrent handles, consider lowering or disabling this.
    pub mmap_size: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            busy_timeout_ms: 5000,
            synchronous: SyncMode::Normal,
            max_traversal_depth: 64,
            max_traversal_work: 10_000_000,
            max_property_value_bytes: 1024 * 1024,
            max_name_bytes: 256,
            max_result_rows: 100_000,
            cache_size: -32000,
            mmap_size: 268_435_456,
        }
    }
}

/// An embedded graph database backed by SQLite.
///
/// Every `Database` instance owns its own SQLite connection. No global state,
/// no shared mutexes. Two handles in the same process are fully independent.
pub struct Database {
    conn: Connection,
    /// State of the active stateful transaction (set by `begin_read`/`begin_write`).
    /// The typed `read_tx`/`write_tx` API does NOT touch this field —
    /// it manages its own scoped `rusqlite::Transaction`.
    tx_state: TxState,
    /// Maximum byte length for a single property value.
    pub max_property_value_bytes: usize,
    /// Maximum byte length for label and property key names.
    pub max_name_bytes: usize,
    /// Maximum number of result rows before the executor aborts.
    pub max_result_rows: usize,
    /// Maximum hop count for variable-length / fixed-length pattern traversal.
    /// Enforced at plan validation time. 0 = unlimited.
    pub max_traversal_depth: u32,
    /// Maximum total edge-visit budget for a single var-length traversal.
    /// Enforced inside `traverse_paths`. 0 = unlimited.
    pub max_traversal_work: u64,
    /// Per-`Database` cache of parsed Cypher ASTs. Populated lazily on
    /// `execute*`/`query*` calls; bounded FIFO eviction. See
    /// `cypher::parse_cache` for design rationale.
    parse_cache: crate::cypher::parse_cache::ParseCache,
    /// Per-`Database` cache of planned (`LogicalOp`) queries. Keyed on
    /// `(cypher, schema_epoch)` so DDL invalidates entries lazily on
    /// next lookup. See `cypher::plan_cache` for design rationale.
    plan_cache: crate::cypher::plan_cache::PlanCache,
    /// Bumped on every `CREATE INDEX` / `DROP INDEX` so cached plans built
    /// before the DDL (which may have made different index decisions) are
    /// not reused.
    schema_epoch: AtomicU64,
}

impl Database {
    /// Open a database at the given file path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_config(path, Config::default())
    }

    /// Open a database with custom configuration.
    pub fn open_with_config<P: AsRef<Path>>(path: P, config: Config) -> Result<Self> {
        let p = path.as_ref();
        // Atomically pre-create the database file with owner-only permissions
        // before SQLite opens it. Without this, SQLITE_OPEN_CREATE would create
        // the file with the process umask (often 0o644), leaving a TOCTOU
        // window where the file is world-readable until our `set_permissions`
        // call ran. `create_new(true)` uses O_CREAT|O_EXCL so this is a no-op
        // (and harmless EEXIST) when the file already exists.
        #[cfg(unix)]
        {
            use std::fs::OpenOptions;
            use std::os::unix::fs::OpenOptionsExt;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(p)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(crate::types::GraphError::Storage {
                        source: rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                            Some(format!("failed to pre-create database file: {e}")),
                        ),
                        hint: None,
                    });
                }
            }
        }
        let conn = Connection::open_with_flags(
            p,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                // NO_MUTEX is safe: Database requires &mut self for all operations,
                // so concurrent access from multiple threads is prevented at compile time.
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::init(conn, &config)
    }

    /// Open an in-memory database (for tests).
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, &Config::default())
    }

    fn init(conn: Connection, config: &Config) -> Result<Self> {
        conn.execute_batch(&format!(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout={};
             PRAGMA synchronous={};
             PRAGMA foreign_keys=OFF;  -- intentional: graph edges are managed in application code, not via FK constraints
             PRAGMA cache_size={};
             PRAGMA mmap_size={};",
            config.busy_timeout_ms, config.synchronous, config.cache_size, config.mmap_size,
        ))?;
        check_and_stamp_identity(&conn)?;
        schema::init_schema(&conn)?;
        Ok(Self {
            conn,
            tx_state: TxState::None,
            max_property_value_bytes: config.max_property_value_bytes,
            max_name_bytes: config.max_name_bytes,
            max_result_rows: config.max_result_rows,
            max_traversal_depth: config.max_traversal_depth,
            max_traversal_work: config.max_traversal_work,
            parse_cache: crate::cypher::parse_cache::ParseCache::new(),
            plan_cache: crate::cypher::plan_cache::PlanCache::new(),
            schema_epoch: AtomicU64::new(0),
        })
    }

    /// Internal access to the underlying SQLite connection.
    ///
    /// Crate-internal: used by `tck_support` for storage-table introspection.
    /// Not part of the public API — bindings use the stateful `execute`/
    /// `begin_*`/`commit` methods instead.
    #[cfg(any(feature = "tck-support", feature = "fuzzing", test))]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Begin a read-only transaction (snapshot isolation via WAL).
    ///
    /// Returns a [`ReadTxGuard`] — an RAII guard that releases the snapshot
    /// on drop. Methods on the wrapped transaction (`query`,
    /// `query_with_params`, `get_node`, …) are accessible directly via
    /// `Deref`. Call `tx.commit()` or `tx.rollback()` to finalize explicitly.
    pub fn read_tx(&mut self) -> Result<ReadTxGuard<'_>> {
        self.ensure_no_active_tx("read_tx")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        Ok(ReadTxGuard::new(ReadTransaction::new(
            tx,
            self.max_result_rows,
            self.max_traversal_depth,
            self.max_traversal_work,
            &self.parse_cache,
            &self.plan_cache,
            &self.schema_epoch,
        )))
    }

    /// Begin a read-write transaction (BEGIN IMMEDIATE).
    ///
    /// Returns a [`WriteTxGuard`] — an RAII guard that auto-rolls-back on
    /// drop with a `tracing::warn!`. Methods on the wrapped transaction
    /// (`create_node`, `create_edge`, `query`, …) are accessible directly via
    /// `Deref`. Call `tx.commit()` to persist writes.
    pub fn write_tx(&mut self) -> Result<WriteTxGuard<'_>> {
        self.ensure_no_active_tx("write_tx")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(WriteTxGuard::new(WriteTransaction::new(
            tx,
            self.max_property_value_bytes,
            self.max_name_bytes,
            self.max_result_rows,
            self.max_traversal_depth,
            self.max_traversal_work,
            &self.parse_cache,
            &self.plan_cache,
            &self.schema_epoch,
        )))
    }

    // ------------------------------------------------------------------
    // Stateful transaction API — bindings-facing entry points.
    // ------------------------------------------------------------------

    /// Begin a read-write transaction (BEGIN IMMEDIATE) on this `Database`
    /// handle. Subsequent `execute` calls run inside it until `commit` or
    /// `rollback`. Returns `GraphError::Transaction` if a txn is already active.
    pub fn begin_write(&mut self) -> Result<()> {
        self.ensure_no_active_tx("begin_write")?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        self.tx_state = TxState::Write;
        Ok(())
    }

    /// Begin a read-only transaction (BEGIN DEFERRED, snapshot isolation via WAL).
    /// Returns `GraphError::Transaction` if a txn is already active.
    pub fn begin_read(&mut self) -> Result<()> {
        self.ensure_no_active_tx("begin_read")?;
        self.conn.execute_batch("BEGIN DEFERRED")?;
        self.tx_state = TxState::Read;
        Ok(())
    }

    /// Execute a Cypher query.
    ///
    /// If a transaction is already active, the query runs inside it. If none
    /// is active, the call is implicitly wrapped in `BEGIN ... COMMIT`: the
    /// mode is picked from the planned query (read-only plans use a deferred
    /// read txn, writes use BEGIN IMMEDIATE). On error the auto-tx is rolled
    /// back. Multi-statement transactions still require explicit
    /// `begin_*` + `commit`.
    pub fn execute(&mut self, cypher: &str) -> Result<Vec<NamedRecord>> {
        self.execute_with_params(cypher, None)
    }

    /// Execute a Cypher query with optional parameter substitution. See
    /// [`Database::execute`] for the auto-tx contract.
    pub fn execute_with_params(
        &mut self,
        cypher: &str,
        params: Option<&std::collections::HashMap<String, Value>>,
    ) -> Result<Vec<NamedRecord>> {
        if self.tx_state != TxState::None {
            let ctx = ExecContext {
                max_result_rows: self.max_result_rows,
                max_traversal_depth: self.max_traversal_depth,
                max_traversal_work: self.max_traversal_work,
                require_read_only: self.tx_state == TxState::Read,
                ..Default::default()
            };
            return execute_cypher(&self.conn, cypher, params, ctx, self.exec_caches());
        }

        // No active txn → auto-begin/auto-commit. Parse + plan once so we can
        // pick the txn mode from the actual plan; then BEGIN, run, COMMIT.
        // Same param-handling contract as `execute_cypher`: validate params
        // are present (no-alloc walker) and publish them to the eval
        // thread-local via `ParamScope` instead of baking them into the plan.
        use crate::cypher::{ast, eval, executor, parser, plan_cache, planner};
        let stmt = self.parse_cache.get_or_parse(cypher)?;
        if let Some(p) = params {
            parser::validate_params(&stmt, p)?;
        }
        let _scope = eval::ParamScope::enter(params);
        let ctx = ExecContext {
            max_result_rows: self.max_result_rows,
            max_traversal_depth: self.max_traversal_depth,
            max_traversal_work: self.max_traversal_work,
            ..Default::default()
        };
        let plan = if plan_cache::is_plan_cacheable(&stmt) {
            let epoch = self.schema_epoch.load(Ordering::Acquire);
            self.plan_cache.get_or_plan(cypher, epoch, || {
                planner::plan_with_procedures(&self.conn, &stmt, &ctx.procedures, params)
            })?
        } else {
            planner::plan_with_procedures(&self.conn, &stmt, &ctx.procedures, params)?
        };
        if matches!(stmt, ast::Statement::Explain(_)) {
            return Ok(crate::cypher::cost::format_explain(&self.conn, &plan));
        }
        let read_only = executor::is_read_only(&plan);
        if read_only {
            self.conn.execute_batch("BEGIN DEFERRED")?;
            self.tx_state = TxState::Read;
        } else {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            self.tx_state = TxState::Write;
        }
        let result = executor::execute_with_ctx(&self.conn, &plan, &ctx);
        match result {
            Ok(records) => {
                self.conn.execute_batch("COMMIT")?;
                self.tx_state = TxState::None;
                Ok(records)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                self.tx_state = TxState::None;
                Err(e)
            }
        }
    }

    /// Commit the currently-active transaction. Returns
    /// `GraphError::Transaction` when no txn is active.
    pub fn commit(&mut self) -> Result<()> {
        if self.tx_state == TxState::None {
            return Err(GraphError::Transaction {
                message: "no active transaction to commit".to_string(),
                hint: None,
            });
        }
        self.conn.execute_batch("COMMIT")?;
        self.tx_state = TxState::None;
        Ok(())
    }

    /// Roll back the currently-active transaction. Returns
    /// `GraphError::Transaction` when no txn is active.
    pub fn rollback(&mut self) -> Result<()> {
        if self.tx_state == TxState::None {
            return Err(GraphError::Transaction {
                message: "no active transaction to roll back".to_string(),
                hint: None,
            });
        }
        self.conn.execute_batch("ROLLBACK")?;
        self.tx_state = TxState::None;
        Ok(())
    }

    /// Create a secondary index on `(label, property)` inside the active
    /// write transaction. Returns `GraphError::Transaction` when no write
    /// transaction is active.
    pub fn create_index(&mut self, label: &str, property: &str) -> Result<()> {
        self.require_write_tx("create_index")?;
        crate::index::create_index(&self.conn, label, property)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Drop a secondary index on `(label, property)` inside the active write
    /// transaction. Returns `GraphError::Transaction` when no write
    /// transaction is active.
    pub fn drop_index(&mut self, label: &str, property: &str) -> Result<()> {
        self.require_write_tx("drop_index")?;
        crate::index::drop_index(&self.conn, label, property)?;
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn exec_caches(&self) -> ExecCaches<'_> {
        ExecCaches {
            parse: Some(&self.parse_cache),
            plan: Some(&self.plan_cache),
            schema_epoch: Some(&self.schema_epoch),
        }
    }

    fn require_write_tx(&self, op: &str) -> Result<()> {
        match self.tx_state {
            TxState::Write => Ok(()),
            _ => Err(GraphError::Transaction {
                message: format!(
                    "{op}: requires an active write transaction (state: {:?})",
                    self.tx_state
                ),
                hint: Some("call begin_write before this operation".to_string()),
            }),
        }
    }

    fn ensure_no_active_tx(&self, op: &str) -> Result<()> {
        if self.tx_state != TxState::None {
            return Err(GraphError::Transaction {
                message: format!(
                    "{op}: a {:?} transaction is already active; nested transactions are not supported",
                    self.tx_state
                ),
                hint: Some(
                    "commit or roll back the current transaction before beginning another"
                        .to_string(),
                ),
            });
        }
        Ok(())
    }
}

/// Verify the database's `application_id` (offset 68) and `user_version`
/// (offset 60) belong to graphdblite. A zero application_id indicates a
/// freshly-created file (or one created before this check existed) — we
/// stamp it. A nonzero ID different from ours rejects the open with a
/// clear error so users don't accidentally point graphdblite at a
/// foreign SQLite file (Firefox cookies, GeoPackage, etc.). A
/// `user_version` greater than the current schema version rejects the
/// open: the file was written by a newer release whose layout this
/// build can't safely interpret.
fn check_and_stamp_identity(conn: &Connection) -> Result<()> {
    let app_id: i32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if app_id != 0 && app_id != GRAPHDBLITE_APPLICATION_ID {
        return Err(GraphError::Storage {
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
                Some(format!(
                    "file is not a graphdblite database (application_id={app_id:#010x}, expected {GRAPHDBLITE_APPLICATION_ID:#010x} 'GDBL')"
                )),
            ),
            hint: Some(
                "this file was created by a different application — point graphdblite at a fresh path or a previously-opened graphdblite database".to_string(),
            ),
        });
    }

    let user_version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if user_version > GRAPHDBLITE_SCHEMA_VERSION {
        return Err(GraphError::Storage {
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
                Some(format!(
                    "database schema version {user_version} is newer than this build supports ({GRAPHDBLITE_SCHEMA_VERSION})"
                )),
            ),
            hint: Some(
                "this file was written by a newer graphdblite release; upgrade the library to read it".to_string(),
            ),
        });
    }

    // `PRAGMA application_id = N` and `PRAGMA user_version = N` only rewrite
    // the header page if the value differs, so this is cheap on every open.
    conn.execute_batch(&format!(
        "PRAGMA application_id = {GRAPHDBLITE_APPLICATION_ID};
         PRAGMA user_version = {GRAPHDBLITE_SCHEMA_VERSION};"
    ))?;
    Ok(())
}

impl Drop for Database {
    fn drop(&mut self) {
        if self.tx_state != TxState::None {
            // Best-effort rollback — anything else risks leaving the on-disk
            // state with a half-applied transaction. Surfacing the warning via
            // `tracing` lets binding authors instrument it without making the
            // destructor itself fallible.
            let prior = self.tx_state;
            self.tx_state = TxState::None;
            match self.conn.execute_batch("ROLLBACK") {
                Ok(()) => tracing::warn!(
                    state = ?prior,
                    "Database dropped with an open transaction; auto-rolled back. \
                     This usually indicates a binding bug — pair begin_* with commit/rollback."
                ),
                Err(e) => tracing::warn!(
                    state = ?prior,
                    error = %e,
                    "Database dropped with an open transaction and the auto-rollback failed."
                ),
            }
        }
    }
}

#[cfg(test)]
mod stateful_tx_tests {
    use super::*;

    #[test]
    fn write_txn_commit_persists() {
        let mut db = Database::open_memory().unwrap();
        db.begin_write().unwrap();
        db.execute("CREATE (:Person {name: 'Alice'})").unwrap();
        db.commit().unwrap();

        db.begin_read().unwrap();
        let rows = db.execute("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(rows.len(), 1);
        db.commit().unwrap();
    }

    #[test]
    fn write_txn_rollback_discards() {
        let mut db = Database::open_memory().unwrap();
        db.begin_write().unwrap();
        db.execute("CREATE (:Person {name: 'Bob'})").unwrap();
        db.rollback().unwrap();

        db.begin_read().unwrap();
        let rows = db.execute("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(rows.len(), 0);
        db.commit().unwrap();
    }

    #[test]
    fn drop_with_open_txn_auto_rolls_back() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        {
            let mut db = Database::open(&path).unwrap();
            db.begin_write().unwrap();
            db.execute("CREATE (:Person {name: 'Carol'})").unwrap();
            // dropped here without commit/rollback
        }
        // Re-open and verify nothing was persisted.
        let mut db = Database::open(&path).unwrap();
        db.begin_read().unwrap();
        let rows = db.execute("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(rows.len(), 0);
        db.commit().unwrap();
    }

    #[test]
    fn nested_begin_rejected() {
        let mut db = Database::open_memory().unwrap();
        db.begin_write().unwrap();
        let err = db.begin_write().unwrap_err();
        assert!(matches!(err, GraphError::Transaction { .. }));
        let err2 = db.begin_read().unwrap_err();
        assert!(matches!(err2, GraphError::Transaction { .. }));
        db.rollback().unwrap();
    }

    #[test]
    fn execute_without_txn_auto_commits_read() {
        // Read query with no active txn auto-begins/auto-commits a read txn.
        let mut db = Database::open_memory().unwrap();
        let rows = db.execute("RETURN 1 AS x").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("x"), Some(&Value::I64(1)));
        // No txn left dangling.
        assert!(db.commit().is_err());
    }

    #[test]
    fn execute_without_txn_auto_commits_write() {
        // Write query with no active txn auto-begins a write txn and commits.
        let mut db = Database::open_memory().unwrap();
        db.execute("CREATE (:Person {name: 'Alice'})").unwrap();
        // Persistence: a fresh read sees the row.
        let rows = db
            .execute("MATCH (n:Person) RETURN n.name AS name")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("name"),
            Some(&Value::String("Alice".to_string()))
        );
    }

    #[test]
    fn execute_auto_tx_rolls_back_on_error() {
        let mut db = Database::open_memory().unwrap();
        let err = db.execute("RETURN this_is_not_valid_cypher").unwrap_err();
        // Some error variant — what matters is that no txn is left dangling.
        let _ = err;
        assert!(db.commit().is_err());
        assert!(db.rollback().is_err());
        // Subsequent queries still work.
        let rows = db.execute("RETURN 1 AS x").unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn commit_or_rollback_without_txn_rejected() {
        let mut db = Database::open_memory().unwrap();
        assert!(matches!(
            db.commit().unwrap_err(),
            GraphError::Transaction { .. }
        ));
        assert!(matches!(
            db.rollback().unwrap_err(),
            GraphError::Transaction { .. }
        ));
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn fresh_database_is_stamped_with_application_id() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        {
            let _db = Database::open(&path).unwrap();
        }
        // Re-open through raw rusqlite to verify the bytes on disk.
        let conn = Connection::open(&path).unwrap();
        let app_id: i32 = conn
            .query_row("PRAGMA application_id", [], |r| r.get(0))
            .unwrap();
        let user_version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(app_id, GRAPHDBLITE_APPLICATION_ID);
        assert_eq!(user_version, GRAPHDBLITE_SCHEMA_VERSION);
    }

    #[test]
    fn reopen_existing_graphdblite_database_succeeds() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        {
            let mut db = Database::open(&path).unwrap();
            db.execute("CREATE (:Person {name: 'Alice'})").unwrap();
        }
        // Second open must not error on the existing application_id.
        let mut db = Database::open(&path).unwrap();
        let rows = db.execute("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn open_rejects_foreign_application_id() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        // Pre-stamp the file with a different application_id (GeoPackage).
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA application_id = 0x47504b47;")
                .unwrap();
        }
        let err = match Database::open(&path) {
            Err(e) => e,
            Ok(_) => panic!("expected Database::open to fail"),
        };
        match err {
            GraphError::Storage { source, .. } => {
                let msg = source.to_string();
                assert!(
                    msg.contains("not a graphdblite database"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Storage error, got {other:?}"),
        }
    }

    #[test]
    fn open_rejects_newer_schema_version() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        // Create a real graphdblite database, then bump user_version past
        // what this build understands.
        {
            let _db = Database::open(&path).unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "PRAGMA user_version = {};",
                GRAPHDBLITE_SCHEMA_VERSION + 1
            ))
            .unwrap();
        }
        let err = match Database::open(&path) {
            Err(e) => e,
            Ok(_) => panic!("expected Database::open to fail"),
        };
        match err {
            GraphError::Storage { source, .. } => {
                let msg = source.to_string();
                assert!(
                    msg.contains("newer than this build"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Storage error, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod plan_cache_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn repeat_query_caches_plan() {
        let mut db = Database::open_memory().unwrap();
        db.execute("CREATE (:Person {name: 'a'}), (:Person {name: 'b'})")
            .unwrap();
        let baseline = db.plan_cache.len();
        let q = "MATCH (n:Person) WHERE n.name = $name RETURN n.name";
        let mut params = std::collections::HashMap::new();
        params.insert("name".to_string(), Value::String("a".into()));
        db.execute_with_params(q, Some(&params)).unwrap();
        assert_eq!(db.plan_cache.len(), baseline + 1);
        params.insert("name".to_string(), Value::String("b".into()));
        let rows = db.execute_with_params(q, Some(&params)).unwrap();
        assert_eq!(
            db.plan_cache.len(),
            baseline + 1,
            "plan should be reused across param values"
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn ddl_bumps_epoch_and_replans() {
        let mut db = Database::open_memory().unwrap();
        for i in 0..5 {
            db.execute(&format!("CREATE (:Person {{name: 'p{i}'}})"))
                .unwrap();
        }
        // First execute populates the plan cache (label scan, no index yet).
        db.execute("MATCH (n:Person) WHERE n.name = 'p3' RETURN n.name")
            .unwrap();
        let len_before = db.plan_cache.len();
        let epoch_before = db.schema_epoch.load(Ordering::Acquire);

        db.begin_write().unwrap();
        db.create_index("Person", "name").unwrap();
        db.commit().unwrap();

        let epoch_after = db.schema_epoch.load(Ordering::Acquire);
        assert!(epoch_after > epoch_before, "create_index must bump epoch");

        // Re-issue the same query — must miss the cache (different epoch),
        // pick up the new IndexLookup plan.
        db.execute("MATCH (n:Person) WHERE n.name = 'p3' RETURN n.name")
            .unwrap();
        assert_eq!(
            db.plan_cache.len(),
            len_before + 1,
            "epoch change should add a fresh plan entry"
        );
    }

    #[test]
    fn limit_param_skips_caching() {
        let mut db = Database::open_memory().unwrap();
        db.execute("CREATE (:Person), (:Person), (:Person)")
            .unwrap();
        let baseline = db.plan_cache.len();
        let q = "MATCH (n:Person) RETURN n LIMIT $k";
        let mut params = std::collections::HashMap::new();
        params.insert("k".to_string(), Value::I64(2));
        db.execute_with_params(q, Some(&params)).unwrap();
        assert_eq!(
            db.plan_cache.len(),
            baseline,
            "LIMIT $param must not be cached"
        );
    }

    #[test]
    fn limit_param_resolves_correctly_each_call() {
        // Regression: with LIMIT $k uncacheable, different $k values must
        // produce different row counts (no stale plan with baked-in count).
        let mut db = Database::open_memory().unwrap();
        for i in 0..10 {
            db.execute(&format!("CREATE (:Person {{n: {i}}})")).unwrap();
        }
        let q = "MATCH (n:Person) RETURN n.n LIMIT $k";
        let mut params = std::collections::HashMap::new();
        params.insert("k".to_string(), Value::I64(2));
        let r2 = db.execute_with_params(q, Some(&params)).unwrap();
        params.insert("k".to_string(), Value::I64(7));
        let r7 = db.execute_with_params(q, Some(&params)).unwrap();
        assert_eq!(r2.len(), 2);
        assert_eq!(r7.len(), 7);
    }
}
