use rusqlite::Connection;

use crate::types::{GraphError, Result};

/// Metadata key prefix for label node counts.
const LABEL_COUNT_PREFIX: &str = "stats:label_count:";

/// Get the node count for a label. Returns 0 if no stats available.
pub fn get_label_count(conn: &Connection, label: &str) -> Result<u64> {
    let key = format!("{LABEL_COUNT_PREFIX}{label}");
    let mut stmt = conn.prepare_cached("SELECT value FROM metadata WHERE key = ?1")?;
    let result = stmt.query_row([&key], |row| row.get::<_, Vec<u8>>(0));
    match result {
        Ok(data) if data.len() == 8 => {
            Ok(u64::from_be_bytes(data[..8].try_into().map_err(|_| {
                GraphError::Serialization {
                    context: String::new(),
                    source: "corrupt label count bytes".into(),
                    hint: None,
                }
            })?))
        }
        _ => Ok(0),
    }
}

/// Increment the node count for a label by 1.
///
/// Safe: always called within a BEGIN IMMEDIATE write transaction,
/// which serializes all writers (even cross-process under WAL).
pub fn increment_label_count(conn: &Connection, label: &str) -> Result<()> {
    let count = get_label_count(conn, label)?;
    set_label_count(conn, label, count + 1)
}

/// Decrement the node count for a label by 1.
pub fn decrement_label_count(conn: &Connection, label: &str) -> Result<()> {
    let count = get_label_count(conn, label)?;
    let new_count = count.saturating_sub(1);
    if new_count == 0 {
        let key = format!("{LABEL_COUNT_PREFIX}{label}");
        conn.execute(
            "DELETE FROM metadata WHERE key = ?1",
            rusqlite::params![key],
        )?;
        Ok(())
    } else {
        set_label_count(conn, label, new_count)
    }
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

/// Metadata key prefix for edge-type counts.
const EDGE_TYPE_COUNT_PREFIX: &str = "stats:edge_type_count:";

/// Get the edge count for a relationship type. Returns 0 if no stats available.
pub fn get_edge_type_count(conn: &Connection, type_name: &str) -> Result<u64> {
    let key = format!("{EDGE_TYPE_COUNT_PREFIX}{type_name}");
    let mut stmt = conn.prepare_cached("SELECT value FROM metadata WHERE key = ?1")?;
    let result = stmt.query_row([&key], |row| row.get::<_, Vec<u8>>(0));
    match result {
        Ok(data) if data.len() == 8 => {
            Ok(u64::from_be_bytes(data[..8].try_into().map_err(|_| {
                GraphError::Serialization {
                    context: String::new(),
                    source: "corrupt edge-type count bytes".into(),
                    hint: None,
                }
            })?))
        }
        _ => Ok(0),
    }
}

/// Increment the edge count for a relationship type by 1.
pub fn increment_edge_type_count(conn: &Connection, type_name: &str) -> Result<()> {
    let count = get_edge_type_count(conn, type_name)?;
    set_edge_type_count(conn, type_name, count + 1)
}

/// Decrement the edge count for a relationship type by `n`. Saturating at 0.
///
/// When the resulting count is 0, removes the metadata row entirely so that
/// `get_all_edge_type_counts` no longer reports the (now-defunct) type.
pub fn decrement_edge_type_count_by(conn: &Connection, type_name: &str, n: u64) -> Result<()> {
    let count = get_edge_type_count(conn, type_name)?;
    let new_count = count.saturating_sub(n);
    if new_count == 0 {
        let key = format!("{EDGE_TYPE_COUNT_PREFIX}{type_name}");
        conn.execute(
            "DELETE FROM metadata WHERE key = ?1",
            rusqlite::params![key],
        )?;
        Ok(())
    } else {
        set_edge_type_count(conn, type_name, new_count)
    }
}

fn set_edge_type_count(conn: &Connection, type_name: &str, count: u64) -> Result<()> {
    let key = format!("{EDGE_TYPE_COUNT_PREFIX}{type_name}");
    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, count.to_be_bytes().as_slice()],
    )?;
    Ok(())
}

/// Return all `(label, count)` pairs currently tracked.
pub fn get_all_label_counts(conn: &Connection) -> Result<Vec<(String, u64)>> {
    collect_counts(conn, LABEL_COUNT_PREFIX)
}

/// Return all `(edge_type, count)` pairs currently tracked.
pub fn get_all_edge_type_counts(conn: &Connection) -> Result<Vec<(String, u64)>> {
    collect_counts(conn, EDGE_TYPE_COUNT_PREFIX)
}

fn collect_counts(conn: &Connection, prefix: &str) -> Result<Vec<(String, u64)>> {
    let like = format!("{prefix}%");
    let mut stmt =
        conn.prepare_cached("SELECT key, value FROM metadata WHERE key LIKE ?1 ORDER BY key")?;
    let rows = stmt
        .query_map([&like], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (key, data) in rows {
        if data.len() != 8 {
            continue;
        }
        let bytes = <[u8; 8]>::try_from(&data[..8]).map_err(|_| GraphError::Serialization {
            context: String::new(),
            source: "corrupt stats count bytes".into(),
            hint: None,
        })?;
        let name = key.strip_prefix(prefix).unwrap_or(&key).to_string();
        out.push((name, u64::from_be_bytes(bytes)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&c).unwrap();
        c
    }

    #[test]
    fn edge_type_count_round_trip() {
        let conn = fresh_conn();
        assert_eq!(get_edge_type_count(&conn, "KNOWS").unwrap(), 0);
        increment_edge_type_count(&conn, "KNOWS").unwrap();
        increment_edge_type_count(&conn, "KNOWS").unwrap();
        increment_edge_type_count(&conn, "KNOWS").unwrap();
        assert_eq!(get_edge_type_count(&conn, "KNOWS").unwrap(), 3);
        decrement_edge_type_count_by(&conn, "KNOWS", 2).unwrap();
        assert_eq!(get_edge_type_count(&conn, "KNOWS").unwrap(), 1);
    }

    #[test]
    fn decrement_edge_type_count_by_to_zero_removes_row() {
        let conn = fresh_conn();
        increment_edge_type_count(&conn, "X").unwrap();
        decrement_edge_type_count_by(&conn, "X", 5).unwrap();
        assert_eq!(get_edge_type_count(&conn, "X").unwrap(), 0);
        // Row should be gone; aggregate reader must not include "X".
        let all = get_all_edge_type_counts(&conn).unwrap();
        assert!(all.iter().all(|(name, _)| name != "X"));
    }

    #[test]
    fn get_all_label_counts_empty_on_fresh_db() {
        let conn = fresh_conn();
        assert!(get_all_label_counts(&conn).unwrap().is_empty());
        assert!(get_all_edge_type_counts(&conn).unwrap().is_empty());
    }

    #[test]
    fn get_all_label_counts_returns_seeded_pairs() {
        let conn = fresh_conn();
        increment_label_count(&conn, "Person").unwrap();
        increment_label_count(&conn, "Person").unwrap();
        increment_label_count(&conn, "Company").unwrap();
        let mut got = get_all_label_counts(&conn).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![("Company".to_string(), 1), ("Person".to_string(), 2)]
        );
    }

    #[test]
    fn get_all_edge_type_counts_returns_seeded_pairs() {
        let conn = fresh_conn();
        increment_edge_type_count(&conn, "KNOWS").unwrap();
        increment_edge_type_count(&conn, "KNOWS").unwrap();
        increment_edge_type_count(&conn, "WORKS_AT").unwrap();
        let mut got = get_all_edge_type_counts(&conn).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![("KNOWS".to_string(), 2), ("WORKS_AT".to_string(), 1)]
        );
    }
}
