//! Schema-version-2 migration of index and FTS table names.
//!
//! Version 1 databases (graphdblite 0.1.x) can contain index tables under
//! three older naming schemes; version 2 uses only the `storage::naming`
//! scheme (`$` separators, uppercase letters escaped as `^x`, trailing `$` on
//! single-property FTS tables). On open, `Database` runs [`migrate_to_v2`]
//! once inside a write transaction:
//!
//! | v1 name | Action |
//! |---------|--------|
//! | `node_idx$<Label>$<p>...` (raw case) | renamed |
//! | `node_fts$<Label>$<p>` (raw case, no trailing `$`) | renamed (FTS5 renames its shadow tables) |
//! | `node_fts_multi_<Label>_<n>` | rebuilt from node data under the new name |
//! | `node_idx_<label>_<prop>` / `node_fts_<label>_<prop>` | rebuilt from node data |
//!
//! The `_`-separated names are ambiguous when the label or property contains
//! an underscore (`node_idx_A_b_c` is `(A_b, c)` or `(A, b_c)`), and v1 code
//! maintained such a table for every reading, so its content is not
//! trustworthy. Each reading whose label has nodes carrying the property gets
//! a fresh index built from the data. When several readings exist and none
//! has data, the definition cannot be recovered: the table is dropped and a
//! warning is logged (the index was empty under every reading, so no indexed
//! data is lost; recreate it if needed).

use rusqlite::Connection;

use crate::storage::{fts, index, naming};
use crate::types::{validate_name, GraphError, Result, Value};

/// Rename or rebuild every v1-named index table. Must run inside a write
/// transaction, after `schema::init_schema`.
pub(crate) fn migrate_to_v2(conn: &Connection) -> Result<()> {
    let tables: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT name, sql FROM sqlite_master \
             WHERE type='table' AND (name LIKE 'node_idx%' OR name LIKE 'node_fts%')",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.filter_map(|r| match r {
            Ok((name, Some(sql))) => Some(Ok((name, sql))),
            Ok((_, None)) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<rusqlite::Result<_>>()?
    };

    for (name, sql) in &tables {
        let is_virtual = sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE VIRTUAL TABLE");
        if is_virtual {
            migrate_fts_table(conn, name, sql)?;
        } else {
            migrate_btree_table(conn, name)?;
        }
    }
    Ok(())
}

fn migrate_btree_table(conn: &Connection, name: &str) -> Result<()> {
    if naming::parse_btree_table_name(name).is_some() {
        return Ok(()); // already canonical
    }
    if let Some(rest) = name.strip_prefix(naming::BTREE_PREFIX) {
        // v1 `$` scheme with raw-case components.
        let parts: Vec<&str> = rest.split('$').collect();
        if parts.len() < 2 || parts.iter().any(|p| validate_name(p).is_err()) {
            return skip_unknown(name);
        }
        let new = naming::btree_table_name(parts[0], &parts[1..]);
        return rename_table(conn, name, &new);
    }
    let Some(rest) = name.strip_prefix("node_idx_") else {
        return skip_unknown(name);
    };
    let readings = resolve_legacy_name(conn, rest, false)?;
    drop_table(conn, name)?;
    if readings.is_empty() {
        warn_unrecoverable(name, rest);
    }
    for (label, property) in readings {
        match index::create_index(conn, &label, &property) {
            Ok(()) | Err(GraphError::IndexAlreadyExists { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn migrate_fts_table(conn: &Connection, name: &str, sql: &str) -> Result<()> {
    if naming::parse_fts_table_name(name).is_some()
        || naming::parse_fts_multi_table_name(name).is_some()
    {
        return Ok(()); // already canonical
    }
    let kind = fts::parse_tokenizer_kind(sql);
    if let Some(rest) = name.strip_prefix(naming::FTS_PREFIX) {
        // v1 `$` scheme: `node_fts$<Label>$<prop>`.
        let Some((label, property)) = rest.split_once('$') else {
            return skip_unknown(name);
        };
        if validate_name(label).is_err() || validate_name(property).is_err() {
            return skip_unknown(name);
        }
        return rename_table(conn, name, &naming::fts_table_name(label, property));
    }
    if let Some((label, columns)) = parse_legacy_multi(name, sql) {
        drop_table(conn, name)?;
        return fts::recreate_fulltext_index(conn, &label, &columns, kind, true);
    }
    let Some(rest) = name.strip_prefix("node_fts_") else {
        return skip_unknown(name);
    };
    let readings = resolve_legacy_name(conn, rest, true)?;
    drop_table(conn, name)?;
    if readings.is_empty() {
        warn_unrecoverable(name, rest);
    }
    for (label, property) in readings {
        match fts::recreate_fulltext_index(conn, &label, &[property], kind, false) {
            Ok(()) | Err(GraphError::IndexAlreadyExists { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Parse a v1 multi-property FTS table `node_fts_multi_<label>_<n>` whose
/// columns are property names. A single `content` column marks a v1
/// single-property table whose label starts with `multi_`.
fn parse_legacy_multi(name: &str, sql: &str) -> Option<(String, Vec<String>)> {
    let rest = name.strip_prefix("node_fts_multi_")?;
    let (label, n) = rest.rsplit_once('_')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || validate_name(label).is_err() {
        return None;
    }
    let columns = fts::parse_column_list(sql);
    if columns.is_empty()
        || columns == ["content"]
        || columns.iter().any(|c| validate_name(c).is_err())
    {
        return None;
    }
    Some((label.to_string(), columns))
}

/// All `(label, property)` readings of a v1 `<label>_<property>` suffix that
/// are worth an index: the only reading if there is exactly one, otherwise
/// the readings whose label has at least one node carrying the property (a
/// string value when `strings_only`, as FTS only indexes strings).
fn resolve_legacy_name(
    conn: &Connection,
    rest: &str,
    strings_only: bool,
) -> Result<Vec<(String, String)>> {
    let readings: Vec<(String, String)> = rest
        .match_indices('_')
        .map(|(i, _)| (&rest[..i], &rest[i + 1..]))
        .filter(|(l, p)| validate_name(l).is_ok() && validate_name(p).is_ok())
        .map(|(l, p)| (l.to_string(), p.to_string()))
        .collect();
    if readings.len() <= 1 {
        return Ok(readings);
    }
    let mut with_data = Vec::new();
    for (label, property) in readings {
        let nodes = crate::node::find_nodes_by_label(conn, &label)?;
        let has = nodes.iter().any(|n| match n.properties.get(&property) {
            Some(Value::String(_)) => true,
            Some(_) => !strings_only,
            None => false,
        });
        if has {
            with_data.push((label, property));
        }
    }
    Ok(with_data)
}

fn rename_table(conn: &Connection, from: &str, to: &str) -> Result<()> {
    conn.execute(
        &format!(
            "ALTER TABLE {} RENAME TO {}",
            naming::quote_ident(from),
            naming::quote_ident(to)
        ),
        [],
    )?;
    Ok(())
}

fn drop_table(conn: &Connection, name: &str) -> Result<()> {
    conn.execute(&format!("DROP TABLE {}", naming::quote_ident(name)), [])?;
    Ok(())
}

/// Leave a table we do not recognise alone (it may be user data that merely
/// shares the prefix); it is not listed as an index either.
fn skip_unknown(name: &str) -> Result<()> {
    tracing::warn!(
        table = name,
        "schema migration: ignoring table with an unrecognised index name"
    );
    Ok(())
}

fn warn_unrecoverable(name: &str, rest: &str) {
    tracing::warn!(
        table = name,
        definition = rest,
        "schema migration: dropped a legacy index whose (label, property) split is \
         ambiguous and which no node data matches; recreate it if it is still needed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&c).unwrap();
        c
    }

    fn put_node(c: &Connection, label: &str, key: &str, v: Value) {
        let props = std::collections::HashMap::from([(key.to_string(), v)]);
        crate::node::create_node(c, &[label.to_string()], props).unwrap();
    }

    #[test]
    fn unambiguous_legacy_name_has_one_reading_without_data() {
        let c = conn();
        assert_eq!(
            resolve_legacy_name(&c, "Person_name", false).unwrap(),
            vec![("Person".to_string(), "name".to_string())]
        );
    }

    #[test]
    fn ambiguous_legacy_name_resolves_by_data() {
        let c = conn();
        put_node(&c, "My_Label", "first_name", Value::I64(1));
        assert_eq!(
            resolve_legacy_name(&c, "My_Label_first_name", false).unwrap(),
            vec![("My_Label".to_string(), "first_name".to_string())]
        );
        // FTS only counts string values.
        assert!(resolve_legacy_name(&c, "My_Label_first_name", true)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ambiguous_legacy_name_without_data_has_no_reading() {
        let c = conn();
        assert!(resolve_legacy_name(&c, "A_b_c", false).unwrap().is_empty());
    }

    #[test]
    fn legacy_multi_requires_numeric_suffix_and_property_columns() {
        let sql =
            "CREATE VIRTUAL TABLE \"x\" USING fts5(\"title\", \"body\", tokenize='unicode61')";
        assert_eq!(
            parse_legacy_multi("node_fts_multi_Art_Icle_3", sql),
            Some((
                "Art_Icle".to_string(),
                vec!["title".to_string(), "body".to_string()]
            ))
        );
        assert_eq!(parse_legacy_multi("node_fts_multi_A_x", sql), None);
        let single = "CREATE VIRTUAL TABLE \"x\" USING fts5(content, tokenize='unicode61')";
        assert_eq!(parse_legacy_multi("node_fts_multi_A_1", single), None);
    }
}
