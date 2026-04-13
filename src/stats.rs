use std::collections::HashMap;

use rusqlite::Connection;

use crate::types::Result;

/// Metadata key prefix for label node counts.
const LABEL_COUNT_PREFIX: &str = "stats:label_count:";

/// Get the node count for a label. Returns 0 if no stats available.
pub fn get_label_count(conn: &Connection, label: &str) -> Result<u64> {
    let key = format!("{LABEL_COUNT_PREFIX}{label}");
    let mut stmt = conn.prepare_cached(
        "SELECT value FROM metadata WHERE key = ?1",
    )?;
    let result = stmt.query_row([&key], |row| row.get::<_, Vec<u8>>(0));
    match result {
        Ok(data) if data.len() == 8 => {
            Ok(u64::from_be_bytes(data[..8].try_into().unwrap()))
        }
        _ => Ok(0),
    }
}

/// Increment the node count for a label by 1.
pub fn increment_label_count(conn: &Connection, label: &str) -> Result<()> {
    let count = get_label_count(conn, label)?;
    set_label_count(conn, label, count + 1)
}

/// Decrement the node count for a label by 1.
pub fn decrement_label_count(conn: &Connection, label: &str) -> Result<()> {
    let count = get_label_count(conn, label)?;
    set_label_count(conn, label, count.saturating_sub(1))
}

/// Set the node count for a label.
fn set_label_count(conn: &Connection, label: &str, count: u64) -> Result<()> {
    let key = format!("{LABEL_COUNT_PREFIX}{label}");
    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, count.to_be_bytes().as_slice()],
    )?;
    Ok(())
}

/// Get all label counts as a map.
#[allow(dead_code)]
pub fn get_all_label_counts(conn: &Connection) -> Result<HashMap<String, u64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT key, value FROM metadata WHERE key LIKE 'stats:label_count:%'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;

    let mut counts = HashMap::new();
    for row in rows {
        let (key, data) = row?;
        if let Some(label) = key.strip_prefix(LABEL_COUNT_PREFIX) {
            if data.len() == 8 {
                let count = u64::from_be_bytes(data[..8].try_into().unwrap());
                counts.insert(label.to_string(), count);
            }
        }
    }
    Ok(counts)
}

/// Rebuild all label statistics by scanning the nodes table.
///
/// This is the implementation behind an ANALYZE-style command.
#[allow(dead_code)]
pub fn refresh_stats(conn: &Connection) -> Result<()> {
    // Clear existing label counts.
    conn.execute(
        "DELETE FROM metadata WHERE key LIKE 'stats:label_count:%'",
        [],
    )?;

    // Full scan of nodes table, counting by label.
    let mut stmt = conn.prepare_cached(
        "SELECT value FROM nodes",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;

    let mut counts: HashMap<String, u64> = HashMap::new();
    for row in rows {
        let data = row?;
        let record: crate::types::NodeRecord = rmp_serde::from_slice(&data)
            .map_err(|e| crate::types::GraphError::Serialization(e.to_string()))?;
        *counts.entry(record.label).or_insert(0) += 1;
    }

    // Write counts to metadata.
    for (label, count) in &counts {
        set_label_count(conn, label, *count)?;
    }

    Ok(())
}
