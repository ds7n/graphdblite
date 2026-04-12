use rusqlite::params;

use crate::types::Result;

/// Table names used in the KV schema.
pub const TABLE_NODES: &str = "nodes";
pub const TABLE_ADJ_OUT: &str = "adj_out";
pub const TABLE_ADJ_IN: &str = "adj_in";
pub const TABLE_EDGE_PROPS: &str = "edge_props";

/// Get a value by key from a KV table. Returns None if not found.
pub fn get(
    conn: &rusqlite::Connection,
    table: &str,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    let sql = format!("SELECT value FROM \"{table}\" WHERE key = ?1");
    let mut stmt = conn.prepare_cached(&sql)?;
    let result = stmt.query_row(params![key], |row| row.get::<_, Vec<u8>>(0));
    match result {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Put a key-value pair into a KV table (insert or replace).
pub fn put(
    conn: &rusqlite::Connection,
    table: &str,
    key: &[u8],
    value: &[u8],
) -> Result<()> {
    let sql = format!(
        "INSERT OR REPLACE INTO \"{table}\" (key, value) VALUES (?1, ?2)"
    );
    conn.execute(&sql, params![key, value])?;
    Ok(())
}

/// Delete a key from a KV table. Returns true if a row was deleted.
pub fn delete(
    conn: &rusqlite::Connection,
    table: &str,
    key: &[u8],
) -> Result<bool> {
    let sql = format!("DELETE FROM \"{table}\" WHERE key = ?1");
    let count = conn.execute(&sql, params![key])?;
    Ok(count > 0)
}

/// Scan all keys with a given prefix from a KV table.
/// Returns (key, value) pairs in key order.
pub fn scan_prefix(
    conn: &rusqlite::Connection,
    table: &str,
    prefix: &[u8],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    // Prefix scan: key >= prefix AND key < prefix_upper_bound
    // Upper bound is prefix with last byte incremented (or extended with 0xFF).
    let upper = prefix_upper_bound(prefix);
    let sql = match &upper {
        Some(_) => format!(
            "SELECT key, value FROM \"{table}\" WHERE key >= ?1 AND key < ?2 ORDER BY key"
        ),
        None => format!(
            "SELECT key, value FROM \"{table}\" WHERE key >= ?1 ORDER BY key"
        ),
    };

    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = match &upper {
        Some(ub) => {
            let mapped = stmt.query_map(params![prefix, ub], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            mapped.collect::<std::result::Result<Vec<_>, _>>()?
        }
        None => {
            let mapped = stmt.query_map(params![prefix], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            mapped.collect::<std::result::Result<Vec<_>, _>>()?
        }
    };
    Ok(rows)
}

/// Compute the exclusive upper bound for a prefix scan.
/// Increments the last non-0xFF byte. Returns None if prefix is all 0xFF.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    while let Some(last) = upper.last_mut() {
        if *last < 0xFF {
            *last += 1;
            return Some(upper);
        }
        upper.pop();
    }
    None // prefix is all 0xFF or empty — scan to end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_upper_bound() {
        assert_eq!(prefix_upper_bound(b"\x00\x01"), Some(b"\x00\x02".to_vec()));
        assert_eq!(prefix_upper_bound(b"\xFF"), None);
        assert_eq!(prefix_upper_bound(b"\xFF\xFF"), None);
        assert_eq!(prefix_upper_bound(b"abc"), Some(b"abd".to_vec()));
        assert_eq!(prefix_upper_bound(b""), None);
    }
}
