//! Physical SQLite table names for secondary (btree) and fulltext indexes.
//!
//! Index tables are named from their `(label, properties)` definition and the
//! name is parsed back when listing indexes, so the scheme must be
//! unambiguous and survive SQLite's case-insensitive identifier matching:
//!
//! - `$` separates components. `validate_name` only admits `[A-Za-z0-9_]`, so
//!   `$` can never appear inside a label or property.
//! - Uppercase ASCII letters are written as `^` + the lowercase letter
//!   (`Person` -> `^person`). SQLite treats `node_idx$Person$name` and
//!   `node_idx$person$name` as the same table; with the escape they differ.
//!   `^` is also rejected by `validate_name`, so decoding is unambiguous.
//! - Single-property FTS tables end with a trailing `$` so that no index
//!   table can equal an FTS5 shadow table (`<table>_data`, `_idx`,
//!   `_content`, `_docsize`, `_config`) of another index.
//!
//! | Kind | Name |
//! |------|------|
//! | btree (1..N props) | `node_idx$<label>$<p1>$...$<pN>` |
//! | FTS single-prop | `node_fts$<label>$<prop>$` |
//! | FTS multi-prop | `node_fts_multi$<label>$<n>` |
//!
//! Databases written before schema version 2 used other names; they are
//! renamed on open by `storage::migrate`.

use crate::types::validate_name;

/// Prefix of btree index tables.
pub(crate) const BTREE_PREFIX: &str = "node_idx$";
/// Prefix of single-property FTS tables.
pub(crate) const FTS_PREFIX: &str = "node_fts$";
/// Prefix of multi-property FTS tables.
pub(crate) const FTS_MULTI_PREFIX: &str = "node_fts_multi$";

/// Tables created by `schema::init_schema`.
pub(crate) const CORE_TABLES: [&str; 5] = ["nodes", "adj_out", "adj_in", "edge_props", "metadata"];

/// A column as `(name, declared type, part of the primary key)`.
pub(crate) type ColumnShape = (&'static str, &'static str, bool);

/// The columns `schema::init_schema` gives each core table, in order, as
/// `(name, declared type, part of the primary key)`. `Database::open` checks
/// a not-yet-stamped file's core tables against this before touching it, so a
/// foreign file whose tables merely share graphdblite's names is refused
/// instead of being switched to WAL and half-initialized.
pub(crate) const CORE_TABLE_COLUMNS: [(&str, &[ColumnShape]); 5] = [
    (
        "nodes",
        &[
            ("key", "BLOB", true),
            ("label", "TEXT", false),
            ("value", "BLOB", false),
        ],
    ),
    (
        "adj_out",
        &[("key", "BLOB", true), ("value", "BLOB", false)],
    ),
    ("adj_in", &[("key", "BLOB", true), ("value", "BLOB", false)]),
    (
        "edge_props",
        &[("key", "BLOB", true), ("value", "BLOB", false)],
    ),
    (
        "metadata",
        &[("key", "TEXT", true), ("value", "BLOB", false)],
    ),
];

/// Whether `name` is a table graphdblite creates or owns: a core table, an
/// index or FTS table under any naming scheme (including FTS5 shadow
/// tables), or an SQLite-internal table. Compared case-insensitively, like
/// SQLite itself.
pub(crate) fn is_graphdblite_table(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CORE_TABLES.contains(&lower.as_str())
        || lower.starts_with("node_idx")
        || lower.starts_with("node_fts")
        || lower.starts_with("sqlite_")
}

/// Encode a validated identifier for use inside a table name: every
/// uppercase ASCII letter `X` becomes `^x`.
pub(crate) fn encode_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    push_encoded(&mut out, name);
    out
}

/// Append the [`encode_ident`] form of `name` to `out` (no intermediate
/// allocation; these names are built on every indexed read and write).
fn push_encoded(out: &mut String, name: &str) {
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('^');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
}

/// Decode one table-name component produced by [`encode_ident`].
///
/// Returns `None` unless `seg` is the canonical encoding of a name accepted
/// by `validate_name` (no raw uppercase letters, every `^` followed by a
/// lowercase letter). Table names come from the database file, which may be
/// crafted, so anything else is rejected rather than interpolated into SQL.
pub(crate) fn decode_ident(seg: &str) -> Option<String> {
    let mut out = String::with_capacity(seg.len());
    let mut chars = seg.chars();
    while let Some(c) = chars.next() {
        match c {
            '^' => match chars.next() {
                Some(l) if l.is_ascii_lowercase() => out.push(l.to_ascii_uppercase()),
                _ => return None,
            },
            c if c.is_ascii_uppercase() => return None,
            c => out.push(c),
        }
    }
    validate_name(&out).ok()?;
    Some(out)
}

/// Quote an SQL identifier: wrap it in double quotes and double any embedded
/// double quote. Every table / column name interpolated into SQL goes
/// through this, even when it was validated, as defence in depth.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Table name of the btree index on `(label, properties)`. Callers validate
/// the components first.
pub(crate) fn btree_table_name(label: &str, properties: &[&str]) -> String {
    let len = properties.iter().map(|p| p.len() + 4).sum::<usize>();
    let mut name = String::with_capacity(BTREE_PREFIX.len() + label.len() + len + 8);
    name.push_str(BTREE_PREFIX);
    push_encoded(&mut name, label);
    for p in properties {
        name.push('$');
        push_encoded(&mut name, p);
    }
    name
}

/// Parse a btree index table name back to `(label, properties)`. `None` for
/// any name that is not exactly what [`btree_table_name`] produces.
pub(crate) fn parse_btree_table_name(name: &str) -> Option<(String, Vec<String>)> {
    let rest = name.strip_prefix(BTREE_PREFIX)?;
    let mut parts = rest.split('$');
    let label = decode_ident(parts.next()?)?;
    let properties: Vec<String> = parts.map(decode_ident).collect::<Option<_>>()?;
    if properties.is_empty() {
        return None;
    }
    Some((label, properties))
}

/// Table name of the single-property FTS index on `(label, property)`.
pub(crate) fn fts_table_name(label: &str, property: &str) -> String {
    format!(
        "{FTS_PREFIX}{}${}$",
        encode_ident(label),
        encode_ident(property)
    )
}

/// Parse a single-property FTS table name back to `(label, property)`.
pub(crate) fn parse_fts_table_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix(FTS_PREFIX)?.strip_suffix('$')?;
    let (label, property) = rest.split_once('$')?;
    Some((decode_ident(label)?, decode_ident(property)?))
}

/// Table name of the `n`-th multi-property FTS index on `label`.
pub(crate) fn fts_multi_table_name(label: &str, n: u64) -> String {
    format!("{FTS_MULTI_PREFIX}{}${n}", encode_ident(label))
}

/// Parse a multi-property FTS table name back to `(label, n)`.
pub(crate) fn parse_fts_multi_table_name(name: &str) -> Option<(String, u64)> {
    let rest = name.strip_prefix(FTS_MULTI_PREFIX)?;
    let (label, n) = rest.split_once('$')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((decode_ident(label)?, n.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_table_columns_match_what_init_schema_creates() {
        // Guards `CORE_TABLE_COLUMNS` (used to refuse lookalike foreign
        // files) against drifting from the real schema.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        for (table, expected) in CORE_TABLE_COLUMNS {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT name, type, pk FROM pragma_table_info('{table}')"
                ))
                .unwrap();
            let actual: Vec<(String, String, bool)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let expected: Vec<(String, String, bool)> = expected
                .iter()
                .map(|(n, t, pk)| (n.to_string(), t.to_string(), *pk))
                .collect();
            assert_eq!(actual, expected, "table {table}");
        }
        assert_eq!(CORE_TABLE_COLUMNS.map(|(t, _)| t), CORE_TABLES);
    }

    #[test]
    fn encode_escapes_uppercase_only() {
        assert_eq!(encode_ident("Person"), "^person");
        assert_eq!(encode_ident("person"), "person");
        assert_eq!(encode_ident("A_b9Z"), "^a_b9^z");
    }

    #[test]
    fn decode_round_trips_and_rejects_non_canonical() {
        for name in ["Person", "person", "A_b9Z", "_", "x"] {
            assert_eq!(decode_ident(&encode_ident(name)).as_deref(), Some(name));
        }
        // Raw uppercase, dangling or misused escape, invalid characters.
        for bad in ["Person", "^", "a^", "^1", "^A", "", "a\"b", "a b", "a$b"] {
            assert_eq!(decode_ident(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn case_only_different_names_get_distinct_tables() {
        let a = btree_table_name("Person", &["name"]);
        let b = btree_table_name("person", &["name"]);
        assert_ne!(a.to_ascii_lowercase(), b.to_ascii_lowercase());
        assert_ne!(
            fts_table_name("Doc", "Body").to_ascii_lowercase(),
            fts_table_name("doc", "body").to_ascii_lowercase()
        );
        assert_ne!(
            fts_multi_table_name("Doc", 1).to_ascii_lowercase(),
            fts_multi_table_name("doc", 1).to_ascii_lowercase()
        );
    }

    #[test]
    fn btree_name_round_trip() {
        let name = btree_table_name("My_Label", &["a_B", "c"]);
        assert_eq!(name, "node_idx$^my_^label$a_^b$c");
        assert_eq!(
            parse_btree_table_name(&name),
            Some((
                "My_Label".to_string(),
                vec!["a_B".to_string(), "c".to_string()]
            ))
        );
        assert_eq!(parse_btree_table_name("node_idx$A"), None);
        assert_eq!(parse_btree_table_name("node_idx$a$"), None);
        assert_eq!(parse_btree_table_name("node_idx_a_b"), None);
    }

    #[test]
    fn fts_name_round_trip_and_shadow_tables_do_not_parse() {
        let name = fts_table_name("A", "b");
        assert_eq!(name, "node_fts$^a$b$");
        assert_eq!(
            parse_fts_table_name(&name),
            Some(("A".to_string(), "b".to_string()))
        );
        // The shadow table of (A, b) is not the table of (A, b_data).
        assert_ne!(format!("{name}_data"), fts_table_name("A", "b_data"));
        for suffix in ["_data", "_idx", "_content", "_docsize", "_config"] {
            assert_eq!(parse_fts_table_name(&format!("{name}{suffix}")), None);
        }
    }

    #[test]
    fn fts_multi_name_round_trip() {
        let name = fts_multi_table_name("Art_Icle", 12);
        assert_eq!(name, "node_fts_multi$^art_^icle$12");
        assert_eq!(
            parse_fts_multi_table_name(&name),
            Some(("Art_Icle".to_string(), 12))
        );
        assert_eq!(parse_fts_multi_table_name("node_fts_multi$a$"), None);
        assert_eq!(parse_fts_multi_table_name("node_fts_multi$a$1x"), None);
        assert_eq!(parse_fts_multi_table_name("node_fts_multi$a$1_data"), None);
    }

    #[test]
    fn quote_ident_doubles_quotes() {
        assert_eq!(quote_ident("t"), "\"t\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
