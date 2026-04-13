use std::fmt;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::schema;
use crate::transaction::{ReadTransaction, WriteTransaction};
use crate::types::Result;

/// SQLite synchronous PRAGMA mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Maximum traversal depth for variable-length paths. Default: 15.
    pub max_traversal_depth: u32,
    /// Maximum byte length for a single property value. Default: 1 MiB.
    pub max_property_value_bytes: usize,
    /// Maximum byte length for label and property key names. Default: 256.
    pub max_name_bytes: usize,
    /// Maximum number of result rows before the executor aborts. Default: 100,000.
    /// Set to 0 to disable the limit.
    pub max_result_rows: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            busy_timeout_ms: 5000,
            synchronous: SyncMode::Normal,
            max_traversal_depth: 15,
            max_property_value_bytes: 1024 * 1024,
            max_name_bytes: 256,
            max_result_rows: 100_000,
        }
    }
}

/// An embedded graph database backed by SQLite.
///
/// Every `Database` instance owns its own SQLite connection. No global state,
/// no shared mutexes. Two handles in the same process are fully independent.
pub struct Database {
    conn: Connection,
    pub(crate) max_property_value_bytes: usize,
    pub(crate) max_name_bytes: usize,
    pub(crate) max_result_rows: usize,
}

impl Database {
    /// Open a database at the given file path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_config(path, Config::default())
    }

    /// Open a database with custom configuration.
    pub fn open_with_config<P: AsRef<Path>>(
        path: P,
        config: Config,
    ) -> Result<Self> {
        let p = path.as_ref();
        let is_new = !p.exists();
        let conn = Connection::open_with_flags(
            p,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                // NO_MUTEX is safe: Database requires &mut self for all operations,
                // so concurrent access from multiple threads is prevented at compile time.
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Restrict file permissions to owner-only on newly created databases.
        #[cfg(unix)]
        if is_new {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
        }
        #[cfg(not(unix))]
        let _ = is_new;
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
             PRAGMA cache_size=-8000;",
            config.busy_timeout_ms, config.synchronous,
        ))?;
        schema::init_schema(&conn)?;
        Ok(Self {
            conn,
            max_property_value_bytes: config.max_property_value_bytes,
            max_name_bytes: config.max_name_bytes,
            max_result_rows: config.max_result_rows,
        })
    }

    /// Begin a read-only transaction (snapshot isolation via WAL).
    pub fn begin_read(&mut self) -> Result<ReadTransaction<'_>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        Ok(ReadTransaction::new(tx, self.max_result_rows))
    }

    /// Begin a read-write transaction (acquires write lock via BEGIN IMMEDIATE).
    pub fn begin_write(&mut self) -> Result<WriteTransaction<'_>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(WriteTransaction::new(
            tx,
            self.max_property_value_bytes,
            self.max_name_bytes,
            self.max_result_rows,
        ))
    }
}
