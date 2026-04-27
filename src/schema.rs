use rusqlite::Connection;

use crate::types::{GraphError, NodeRecord, Result};

/// Current schema version. Bump when table layout changes.
const SCHEMA_VERSION: u64 = 2;

/// Initialize the database schema. Creates tables if they don't exist.
///
/// Runs inside an explicit transaction so the schema is created atomically.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        BEGIN;

        CREATE TABLE IF NOT EXISTS nodes (
            key BLOB PRIMARY KEY,
            label TEXT NOT NULL DEFAULT '',
            value BLOB NOT NULL
        ) WITHOUT ROWID;

        CREATE INDEX IF NOT EXISTS idx_nodes_label ON nodes(label);

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

    // Initialize global edge sequence counter if not set.
    conn.execute(
        "INSERT OR IGNORE INTO metadata (key, value) VALUES ('next_edge_seq', ?1)",
        [&0u64.to_be_bytes()[..]],
    )?;

    // Read stored schema version.
    let mut stmt =
        conn.prepare_cached("SELECT value FROM metadata WHERE key = 'schema_version'")?;
    let db_version = if let Ok(raw) = stmt.query_row([], |row| row.get::<_, Vec<u8>>(0)) {
        if raw.len() == 8 {
            if let Ok(bytes) = <[u8; 8]>::try_from(&raw[..8]) {
                u64::from_be_bytes(bytes)
            } else {
                0
            }
        } else {
            0
        }
    } else {
        0
    };
    drop(stmt);

    // Reject databases created by a newer library.
    if db_version > SCHEMA_VERSION {
        return Err(GraphError::SchemaMismatch(db_version, SCHEMA_VERSION));
    }

    // Migrate v1 → v2: add label column to nodes table.
    if db_version == 1 {
        migrate_v1_to_v2(conn)?;
    }

    Ok(())
}

/// Migrate schema from v1 (nodes has key+value only) to v2 (key+label+value).
///
/// Recreates the nodes table with the label column, backfills label from the
/// msgpack-serialized value blob, and creates the label index.
fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS nodes_v2 (
            key BLOB PRIMARY KEY,
            label TEXT NOT NULL DEFAULT '',
            value BLOB NOT NULL
        ) WITHOUT ROWID;",
    )?;

    // Read all existing nodes and extract labels from msgpack.
    let mut read_stmt = conn.prepare("SELECT key, value FROM nodes ORDER BY key")?;
    let rows: Vec<(Vec<u8>, Vec<u8>)> = read_stmt
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(read_stmt);

    let mut insert_stmt =
        conn.prepare("INSERT INTO nodes_v2 (key, label, value) VALUES (?1, ?2, ?3)")?;
    for (key, data) in &rows {
        let label = match rmp_serde::from_slice::<NodeRecord>(data) {
            Ok(record) => record.labels.join(":"),
            Err(_) => String::new(),
        };
        insert_stmt.execute(rusqlite::params![key, label, data])?;
    }
    drop(insert_stmt);

    conn.execute_batch(
        "DROP TABLE nodes;
         ALTER TABLE nodes_v2 RENAME TO nodes;
         CREATE INDEX idx_nodes_label ON nodes(label);",
    )?;

    // Update schema version to 2.
    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?1)",
        [&2u64.to_be_bytes()[..]],
    )?;

    Ok(())
}
