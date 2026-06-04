use rusqlite::Connection;

use crate::types::{GraphError, NodeRecord, Result};

/// Current schema version. Bump when table layout changes.
const SCHEMA_VERSION: u64 = 3;

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
        return Err(GraphError::SchemaMismatch {
            found: db_version,
            supported: SCHEMA_VERSION,
            hint: None,
        });
    }

    // Migrate v1 → v2: add label column to nodes table.
    if db_version == 1 {
        migrate_v1_to_v2(conn)?;
    }

    if db_version <= 2 {
        // Fresh DBs stamp `schema_version = SCHEMA_VERSION` via the
        // INSERT OR IGNORE above (so db_version == 3 here and we skip).
        // Genuine v1 DBs hit migrate_v1_to_v2 first; v1 → v3 then needs
        // this v2 → v3 step as well, which is why this is `<= 2`.
        migrate_v2_to_v3(conn)?;
    }

    Ok(())
}

#[cfg(test)]
mod migration_v3_tests {
    use super::*;
    use rusqlite::Connection;

    /// Initialize a v2-shaped DB, then forcibly downgrade `schema_version` to 2
    /// and seed `edge_props` rows directly so the v3 counters are absent.
    fn seed_v2_db_with_raw_edges() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // Force schema_version back to 2.
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?1)",
            [&2u64.to_be_bytes()[..]],
        )
        .unwrap();
        // Drop any edge-type stats that the v3 init may have written.
        conn.execute(
            "DELETE FROM metadata WHERE key LIKE 'stats:edge_type_count:%'",
            [],
        )
        .unwrap();

        // Seed edge_props rows directly. Layout: [src:8][dst:8][label][0x00][seq:8].
        let put = |src: u64, dst: u64, label: &str, seq: u64| {
            let mut key = Vec::new();
            key.extend_from_slice(&src.to_be_bytes());
            key.extend_from_slice(&dst.to_be_bytes());
            key.extend_from_slice(label.as_bytes());
            key.push(0x00);
            key.extend_from_slice(&seq.to_be_bytes());
            conn.execute(
                "INSERT INTO edge_props (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, &[][..]],
            )
            .unwrap();
        };
        put(1, 2, "KNOWS", 0);
        put(1, 2, "KNOWS", 1);
        put(2, 3, "KNOWS", 2);
        put(1, 2, "WORKS_AT", 3);
        conn
    }

    #[test]
    fn migrate_v2_to_v3_backfills_edge_type_counts() {
        let conn = seed_v2_db_with_raw_edges();
        // Re-run init_schema to trigger v2 → v3 migration.
        init_schema(&conn).unwrap();

        let mut got = crate::stats::get_all_edge_type_counts(&conn).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![("KNOWS".to_string(), 3), ("WORKS_AT".to_string(), 1),]
        );

        // schema_version stamped to 3.
        let raw: Vec<u8> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw.len(), 8);
        assert_eq!(u64::from_be_bytes(raw.try_into().unwrap()), 3);
    }

    #[test]
    fn migrate_is_idempotent_when_already_v3() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // Second call must not double-count anything.
        init_schema(&conn).unwrap();
        assert!(crate::stats::get_all_edge_type_counts(&conn)
            .unwrap()
            .is_empty());
    }
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

/// Migrate schema from v2 to v3: backfill `stats:edge_type_count:*` from
/// existing `edge_props` rows.
fn migrate_v2_to_v3(conn: &Connection) -> Result<()> {
    use std::collections::HashMap;

    let mut counts: HashMap<String, u64> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT key FROM edge_props")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        for row in rows {
            let key = row?;
            let label = crate::storage::edge::label_from_edge_props_key(&key)
                .ok_or_else(|| GraphError::Serialization {
                    context: "migrate_v2_to_v3".to_string(),
                    source: format!("malformed edge_props key (len={})", key.len()),
                    hint: None,
                })?
                .to_string();
            *counts.entry(label).or_insert(0) += 1;
        }
    }

    for (type_name, count) in counts {
        let key = format!("stats:edge_type_count:{type_name}");
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, count.to_be_bytes().as_slice()],
        )?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?1)",
        [&3u64.to_be_bytes()[..]],
    )?;

    Ok(())
}
