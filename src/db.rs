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
        schema::init_schema(&conn)?;
        Ok(Self {
            conn,
            max_property_value_bytes: config.max_property_value_bytes,
            max_name_bytes: config.max_name_bytes,
            max_result_rows: config.max_result_rows,
            max_traversal_depth: config.max_traversal_depth,
            max_traversal_work: config.max_traversal_work,
        })
    }

    /// Access the underlying SQLite connection.
    ///
    /// Used by language bindings (Python, Node.js, etc.) for manual transaction
    /// management across separate method calls.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Begin a read-only transaction (snapshot isolation via WAL).
    pub fn begin_read(&mut self) -> Result<ReadTransaction<'_>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        Ok(ReadTransaction::new(
            tx,
            self.max_result_rows,
            self.max_traversal_depth,
            self.max_traversal_work,
        ))
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
            self.max_traversal_depth,
            self.max_traversal_work,
        ))
    }
}
