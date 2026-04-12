use rusqlite::Connection;

use crate::types::Result;

/// Current schema version. Bump when table layout changes.
const SCHEMA_VERSION: u64 = 1;

/// Initialize the database schema. Creates tables if they don't exist.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
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

    Ok(())
}
