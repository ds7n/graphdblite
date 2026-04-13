use rusqlite::Connection;

use crate::types::{GraphError, Result};

/// Current schema version. Bump when table layout changes.
const SCHEMA_VERSION: u64 = 1;

/// Initialize the database schema. Creates tables if they don't exist.
///
/// Runs inside an explicit transaction so the schema is created atomically.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        BEGIN;

        CREATE TABLE IF NOT EXISTS nodes (
            key BLOB PRIMARY KEY,
            value BLOB NOT NULL
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS adj_out (
            key BLOB PRIMARY KEY,
            value BLOB NOT NULL
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS adj_in (
            key BLOB PRIMARY KEY,
            value BLOB NOT NULL
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS edge_props (
            key BLOB PRIMARY KEY,
            value BLOB NOT NULL
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
            value BLOB NOT NULL
        ) WITHOUT ROWID;

        COMMIT;
        ",
    )?;

    // Initialize schema version if not set.
    conn.execute(
        "INSERT OR IGNORE INTO metadata (key, value) VALUES ('schema_version', ?1)",
        [&SCHEMA_VERSION.to_be_bytes()[..]],
    )?;

    // Initialize node ID counter if not set.
    conn.execute(
        "INSERT OR IGNORE INTO metadata (key, value) VALUES ('next_node_id', ?1)",
        [&1u64.to_be_bytes()[..]],
    )?;

    // Validate schema version — reject databases created by a newer library.
    let mut stmt = conn.prepare_cached(
        "SELECT value FROM metadata WHERE key = 'schema_version'",
    )?;
    if let Ok(raw) = stmt.query_row([], |row| row.get::<_, Vec<u8>>(0)) {
        if raw.len() == 8 {
            if let Ok(bytes) = <[u8; 8]>::try_from(&raw[..8]) {
                let db_version = u64::from_be_bytes(bytes);
                if db_version > SCHEMA_VERSION {
                    return Err(GraphError::SchemaMismatch(db_version, SCHEMA_VERSION));
                }
            }
        }
    }

    Ok(())
}
