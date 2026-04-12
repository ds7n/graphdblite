use std::path::Path;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::schema;
use crate::transaction::{ReadTransaction, WriteTransaction};
use crate::types::Result;

/// Configuration for opening a database.
pub struct Config {
    /// SQLite busy timeout in milliseconds. Default: 5000.
    pub busy_timeout_ms: u32,
    /// SQLite synchronous mode. Default: "NORMAL" (WAL-safe).
    /// Set to "FULL" for zero-loss guarantees on power failure.
    pub synchronous: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            busy_timeout_ms: 5000,
            synchronous: "NORMAL".to_string(),
        }
    }
}

/// An embedded graph database backed by SQLite.
///
/// Every `Database` instance owns its own SQLite connection. No global state,
/// no shared mutexes. Two handles in the same process are fully independent.
pub struct Database {
    conn: Connection,
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
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
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
             PRAGMA foreign_keys=OFF;
             PRAGMA cache_size=-8000;",
            config.busy_timeout_ms, config.synchronous,
        ))?;
        schema::init_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Begin a read-only transaction (snapshot isolation via WAL).
    pub fn begin_read(&mut self) -> Result<ReadTransaction<'_>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        Ok(ReadTransaction::new(tx))
    }

    /// Begin a read-write transaction (acquires write lock via BEGIN IMMEDIATE).
    pub fn begin_write(&mut self) -> Result<WriteTransaction<'_>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(WriteTransaction::new(tx))
    }
}
