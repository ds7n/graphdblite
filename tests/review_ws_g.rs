//! WS-G review findings: storage, untrusted database files and fulltext
//! security. Each section is keyed by the finding id (G1..G12) from the
//! October 2026 code/security review and fails on the pre-fix code.

use std::collections::HashMap;

use graphdblite::{Database, ErrorCode, GraphError, QueryError, Record, Value};

fn run(db: &mut Database, q: &str) -> Vec<Record> {
    db.execute(q)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

fn run_p(db: &mut Database, q: &str, params: &[(&str, Value)]) -> Vec<Record> {
    let p: HashMap<String, Value> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    db.execute_with_params(q, Some(&p))
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

fn err_p(db: &mut Database, q: &str, params: &[(&str, Value)]) -> GraphError {
    let p: HashMap<String, Value> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    match db.execute_with_params(q, Some(&p)) {
        Ok(rows) => panic!("expected an error from {q}, got {rows:?}"),
        Err(e) => e,
    }
}

fn int(db: &mut Database, q: &str) -> i64 {
    let rows = run(db, q);
    assert_eq!(rows.len(), 1, "expected one row from {q}, got {rows:?}");
    let first = rows[0].values().next().cloned();
    match first {
        Some(Value::I64(n)) => n,
        other => panic!("expected integer from {q}, got {other:?}"),
    }
}

fn strings(rows: &[Record], col: &str) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| match r.get(col) {
            Some(Value::String(s)) => s.clone(),
            other => panic!("expected string in {col}, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::List(items)) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => s.clone(),
                other => panic!("expected string list item, got {other:?}"),
            })
            .collect(),
        other => panic!("expected list, got {other:?}"),
    }
}

/// `CALL db.counts()` as `(kind, name) -> count`.
fn counts(db: &mut Database) -> HashMap<(String, String), i64> {
    let rows = run(
        db,
        "CALL db.counts() YIELD kind, name, count RETURN kind, name, count",
    );
    rows.iter()
        .map(|r| {
            let s = |c: &str| match r.get(c) {
                Some(Value::String(s)) => s.clone(),
                other => panic!("expected string in {c}, got {other:?}"),
            };
            let n = match r.get("count") {
                Some(Value::I64(n)) => *n,
                other => panic!("expected count, got {other:?}"),
            };
            ((s("kind"), s("name")), n)
        })
        .collect()
}

fn label_count(db: &mut Database, label: &str) -> i64 {
    counts(db)
        .get(&("label".to_string(), label.to_string()))
        .copied()
        .unwrap_or(0)
}

// -- G5: varint decoding of on-disk adjacency blobs --------------------------

#[test]
fn g5_crafted_adjacency_blob_errors_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g5.db");
    {
        let mut db = Database::open(&path).unwrap();
        run(&mut db, "CREATE (:A {n: 1})-[:R]->(:B {n: 2})");
    }
    {
        // An unterminated, over-long varint (20 continuation bytes): the old
        // decoder shifted past 64 bits (panic in debug, garbage in release).
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("UPDATE adj_out SET value = ?1", [vec![0xFFu8; 20]])
            .unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    let err = db
        .execute("MATCH (a:A)-[:R]->(b) RETURN b.n AS n")
        .unwrap_err();
    match err {
        GraphError::Serialization { source, .. } => {
            assert!(source.contains("varint"), "unexpected source: {source}")
        }
        other => panic!("expected Serialization error, got {other:?}"),
    }
}

// -- G10: duplicate labels -----------------------------------------------------

#[test]
fn g10_create_with_duplicate_labels_stores_each_label_once() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(&mut db, "CREATE (n:A:B:A) RETURN labels(n) AS l");
    assert_eq!(str_list(rows[0].get("l")), vec!["A", "B"]);
    assert_eq!(label_count(&mut db, "A"), 1);
    assert_eq!(label_count(&mut db, "B"), 1);
    run(&mut db, "MATCH (n) DETACH DELETE n");
    assert_eq!(label_count(&mut db, "A"), 0);
    assert_eq!(label_count(&mut db, "B"), 0);
}

#[test]
fn g10_typed_create_with_duplicate_labels_dedups() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    let id = tx
        .create_node_with_labels(&["X".to_string(), "X".to_string()], HashMap::new())
        .unwrap();
    assert_eq!(tx.get_node(id).unwrap().labels, vec!["X".to_string()]);
    tx.commit().unwrap();
    assert_eq!(label_count(&mut db, "X"), 1);
    assert_eq!(int(&mut db, "MATCH (n:X) RETURN count(*)"), 1);
}

// -- G11: typed Rust API gaps ------------------------------------------------

#[test]
fn g11_typed_create_edge_drops_null_properties() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    let a = tx.create_node("P", HashMap::new()).unwrap();
    let b = tx.create_node("P", HashMap::new()).unwrap();
    let props = HashMap::from([
        ("gone".to_string(), Value::Null),
        ("kept".to_string(), Value::I64(1)),
    ]);
    tx.create_edge(a, b, "R", props).unwrap();
    tx.commit().unwrap();
    let rows = run(&mut db, "MATCH ()-[r:R]->() RETURN keys(r) AS k");
    assert_eq!(str_list(rows[0].get("k")), vec!["kept"]);
}

fn assert_invalid_property_type(err: GraphError) {
    match err {
        GraphError::Query(q @ QueryError::TypeError { .. }) => {
            assert_eq!(q.code(), ErrorCode::InvalidPropertyType, "{q}")
        }
        other => panic!("expected TypeError(InvalidPropertyType), got {other:?}"),
    }
}

#[test]
fn g11_typed_api_rejects_map_property_values() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    let map = Value::Map(std::collections::BTreeMap::from([(
        "k".to_string(),
        Value::I64(1),
    )]));
    let err = tx
        .create_node("P", HashMap::from([("m".to_string(), map.clone())]))
        .unwrap_err();
    assert_invalid_property_type(err);

    let a = tx.create_node("P", HashMap::new()).unwrap();
    // A map nested in a list is rejected too.
    let err = tx
        .set_node_property(a, "m", Value::List(vec![map.clone()]))
        .unwrap_err();
    assert_invalid_property_type(err);
    let err = tx
        .create_edge(a, a, "R", HashMap::from([("m".to_string(), map)]))
        .unwrap_err();
    assert_invalid_property_type(err);
    // Scalars and scalar lists remain valid.
    tx.set_node_property(a, "ok", Value::List(vec![Value::I64(1), Value::I64(2)]))
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(int(&mut db, "MATCH (n:P) RETURN size(n.ok)"), 2);
}

#[test]
fn g11_typed_create_edge_returns_seq_and_delete_edge_at_removes_one() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    let a = tx.create_node("P", HashMap::new()).unwrap();
    let b = tx.create_node("P", HashMap::new()).unwrap();
    let s1 = tx
        .create_edge(a, b, "R", HashMap::from([("w".to_string(), Value::I64(1))]))
        .unwrap();
    let s2 = tx
        .create_edge(a, b, "R", HashMap::from([("w".to_string(), Value::I64(2))]))
        .unwrap();
    assert_ne!(s1, s2);
    tx.delete_edge_at(a, b, "R", s1).unwrap();
    tx.commit().unwrap();
    let rows = run(&mut db, "MATCH (:P)-[r:R]->(:P) RETURN r.w AS w");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("w"), Some(&Value::I64(2)));
    let c = counts(&mut db);
    assert_eq!(c.get(&("edge_type".to_string(), "R".to_string())), Some(&1));

    // Deleting the last parallel edge removes the adjacency entry too.
    let tx = db.write_tx().unwrap();
    tx.delete_edge_at(a, b, "R", s2).unwrap();
    tx.commit().unwrap();
    assert_eq!(int(&mut db, "MATCH (:P)-[r]->(:P) RETURN count(r)"), 0);
    assert_eq!(int(&mut db, "MATCH (a:P)-->(b) RETURN count(b)"), 0);
}

#[test]
fn g11_delete_edge_at_unknown_seq_is_edge_not_found() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    let a = tx.create_node("P", HashMap::new()).unwrap();
    let b = tx.create_node("P", HashMap::new()).unwrap();
    let s = tx.create_edge(a, b, "R", HashMap::new()).unwrap();
    match tx.delete_edge_at(a, b, "R", s + 100) {
        Err(GraphError::EdgeNotFound { src, dst, .. }) => {
            assert_eq!((src, dst), (a, b));
        }
        other => panic!("expected EdgeNotFound, got {other:?}"),
    }
    // The real edge is untouched.
    tx.commit().unwrap();
    assert_eq!(int(&mut db, "MATCH ()-[r:R]->() RETURN count(r)"), 1);
}

// -- G2: errors must not echo user data -----------------------------------------

const SECRET: &str = "s3cr3tV4lue";

fn assert_no_secret(err: &GraphError) {
    let text = format!("{err} {err:?}");
    assert!(!text.contains(SECRET), "error echoes user data: {text}");
}

#[test]
fn g2_fts_search_syntax_error_does_not_echo_query() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_word("Doc", "body").unwrap();
    db.commit().unwrap();
    run(&mut db, "CREATE (:Doc {body: 'hello world'})");
    let q = format!("{SECRET} AND (");
    let err = err_p(
        &mut db,
        "CALL fts.search('Doc', 'body', $q) YIELD node RETURN node",
        &[("q", Value::String(q))],
    );
    assert_no_secret(&err);
    match &err {
        GraphError::Query(q @ QueryError::ProcedureError { .. }) => {
            assert!(q.message().contains("fts.search"), "{q}");
            assert!(q.message().contains("syntax"), "{q}");
        }
        other => panic!("expected ProcedureError, got {other:?}"),
    }
}

#[test]
fn g2_percentile_type_error_names_the_type_not_the_value() {
    let mut db = Database::open_memory().unwrap();
    let err = err_p(
        &mut db,
        "UNWIND [1, 2] AS x RETURN percentileDisc(x, $p) AS p",
        &[("p", Value::String(SECRET.to_string()))],
    );
    assert_no_secret(&err);
    match &err {
        GraphError::Query(q) => assert!(q.message().contains("String"), "{q}"),
        other => panic!("expected query error, got {other:?}"),
    }
}

#[test]
fn g2_delete_non_entity_names_the_type_not_the_value() {
    let mut db = Database::open_memory().unwrap();
    let err = err_p(
        &mut db,
        "WITH [$s] AS l UNWIND l AS x DELETE x",
        &[("s", Value::String(SECRET.to_string()))],
    );
    assert_no_secret(&err);
    match &err {
        GraphError::Query(q) => {
            assert_eq!(q.code(), ErrorCode::InvalidArgumentType, "{q}");
            assert!(q.message().contains("String"), "{q}");
        }
        other => panic!("expected query error, got {other:?}"),
    }
}

#[test]
fn g2_name_size_limit_does_not_echo_the_name() {
    let mut db = Database::open_memory().unwrap();
    db.max_name_bytes = 4;
    let q = format!("CREATE (:{SECRET})");
    let err = db.execute(&q).unwrap_err();
    assert_no_secret(&err);
    // Not even the truncated preview the old message carried.
    let text = format!("{err} {err:?}");
    assert!(
        !text.contains(&SECRET[..4]),
        "error echoes a name prefix: {text}"
    );
    match err {
        GraphError::SizeLimit { limit, actual, .. } => {
            assert_eq!((limit, actual), (4, SECRET.len()));
        }
        other => panic!("expected SizeLimit, got {other:?}"),
    }
}

// -- G1: fts.search column scope cannot be escaped -----------------------------

/// `Article(title, body)` word index: one doc whose secret word is only in
/// `title`, one whose public word is in `body`.
fn article_db() -> Database {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_word_multi("Article", &["title".to_string(), "body".to_string()])
        .unwrap();
    db.commit().unwrap();
    run(
        &mut db,
        "CREATE (:Article {id: 'a', title: 'secret plans', body: 'nothing to see'}), \
                (:Article {id: 'b', title: 'public notes', body: 'lunch menu here'})",
    );
    db
}

fn fts_ids(db: &mut Database, property: &str, q: &str) -> Vec<String> {
    let rows = run_p(
        db,
        "CALL fts.search('Article', $p, $q) YIELD node RETURN node.id AS id",
        &[
            ("p", Value::String(property.to_string())),
            ("q", Value::String(q.to_string())),
        ],
    );
    strings(&rows, "id")
}

#[test]
fn g1_scoped_search_does_not_match_other_columns() {
    let mut db = article_db();
    assert_eq!(fts_ids(&mut db, "body", "secret"), Vec::<String>::new());
    // Injection payloads: the first two are from the security review and
    // matched doc `a` through `title`; the rest use valid FTS5 syntax with an
    // inner column filter. `secret` appears in no `body`, so a body-scoped
    // search must either reject the query or return no rows.
    for payload in [
        "zzz) OR (secret",
        "x) OR \"title\":(secret",
        "x OR title:secret",
        "x OR \"title\":secret",
        "x OR {title}:secret",
        "x OR {title body}:secret",
        "- title:secret",
    ] {
        let p: HashMap<String, Value> = HashMap::from([
            ("p".to_string(), Value::String("body".to_string())),
            ("q".to_string(), Value::String(payload.to_string())),
        ]);
        match db.execute_with_params(
            "CALL fts.search('Article', $p, $q) YIELD node RETURN node.id AS id",
            Some(&p),
        ) {
            Ok(rows) => assert_eq!(rows.len(), 0, "payload {payload:?} leaked: {rows:?}"),
            Err(GraphError::Query(q @ QueryError::ProcedureError { .. })) => {
                assert!(q.message().contains("syntax"), "payload {payload:?}: {q}")
            }
            Err(other) => panic!("payload {payload:?}: unexpected error {other:?}"),
        }
    }
}

#[test]
fn g1_scoped_and_unscoped_searches_still_work() {
    let mut db = article_db();
    // Plain word, scoped to the right column.
    assert_eq!(fts_ids(&mut db, "title", "secret"), vec!["a"]);
    assert_eq!(fts_ids(&mut db, "body", "lunch"), vec!["b"]);
    // Boolean OR inside the scoped column.
    assert_eq!(fts_ids(&mut db, "body", "lunch OR nothing"), vec!["a", "b"]);
    // Phrase and prefix queries.
    assert_eq!(fts_ids(&mut db, "body", "\"lunch menu\""), vec!["b"]);
    assert_eq!(
        fts_ids(&mut db, "body", "\"menu lunch\""),
        Vec::<String>::new()
    );
    assert_eq!(fts_ids(&mut db, "title", "pub*"), vec!["b"]);
    // '*' searches every column of the index.
    assert_eq!(fts_ids(&mut db, "*", "secret OR lunch"), vec!["a", "b"]);
    assert_eq!(fts_ids(&mut db, "*", "notes"), vec!["b"]);
}

// -- G3: SET / REMOVE label on a node with an FTS-indexed property ------------

#[test]
fn g3_set_label_on_fts_indexed_node_succeeds() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();
    run(&mut db, "CREATE (:Doc {body: 'hello world'})");

    run(&mut db, "MATCH (n:Doc) SET n:Extra");
    let q = "MATCH (n:Doc) WHERE n.body CONTAINS 'llo w' RETURN n.body AS b";
    assert_eq!(strings(&run(&mut db, q), "b"), vec!["hello world"]);

    // MERGE ... ON MATCH SET n:Label takes the same path.
    run(
        &mut db,
        "MERGE (n:Doc {body: 'hello world'}) ON MATCH SET n:Again",
    );
    assert_eq!(strings(&run(&mut db, q), "b"), vec!["hello world"]);

    // REMOVE of an unrelated label keeps the entry; REMOVE :Doc drops it.
    run(&mut db, "MATCH (n:Doc) REMOVE n:Extra");
    assert_eq!(strings(&run(&mut db, q), "b"), vec!["hello world"]);
    run(&mut db, "MATCH (n:Doc) REMOVE n:Doc");
    assert_eq!(run(&mut db, q).len(), 0);
    // Re-adding the label re-indexes exactly once.
    run(&mut db, "MATCH (n:Again) SET n:Doc");
    assert_eq!(strings(&run(&mut db, q), "b"), vec!["hello world"]);
}

#[test]
fn g3_set_label_on_multi_prop_fts_node_keeps_one_search_hit() {
    let mut db = article_db();
    run(&mut db, "MATCH (n:Article) SET n:Pinned");
    assert_eq!(fts_ids(&mut db, "*", "secret"), vec!["a"]);
    assert_eq!(fts_ids(&mut db, "body", "lunch"), vec!["b"]);
}

// -- G4: FTS identifiers and FTS5 shadow tables ---------------------------------

#[test]
fn g4_drop_of_shadow_table_name_is_index_not_found() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("A", "b").unwrap();
    db.commit().unwrap();
    run(&mut db, "CREATE (:A {b: 'alphabet soup'})");

    db.begin_write().unwrap();
    for shadow in ["b_data", "b_idx", "b_content", "b_docsize", "b_config"] {
        match db.drop_fulltext_index("A", shadow) {
            Err(GraphError::IndexNotFound { properties, .. }) => {
                assert_eq!(properties, vec![shadow.to_string()])
            }
            other => panic!("expected IndexNotFound for {shadow}, got {other:?}"),
        }
    }
    db.commit().unwrap();
    // Index (A, b) is intact and still used.
    let rows = run(
        &mut db,
        "MATCH (n:A) WHERE n.b CONTAINS 'habet' RETURN n.b AS b",
    );
    assert_eq!(strings(&rows, "b"), vec!["alphabet soup"]);
}

#[test]
fn g4_index_on_shadow_suffixed_property_coexists() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("A", "b").unwrap();
    db.create_fulltext_index("A", "b_data").unwrap();
    db.commit().unwrap();
    run(
        &mut db,
        "CREATE (:A {b: 'first thing', b_data: 'second thing'})",
    );
    let b = run(
        &mut db,
        "MATCH (n:A) WHERE n.b CONTAINS 'irst' RETURN n.b AS v",
    );
    assert_eq!(strings(&b, "v"), vec!["first thing"]);
    let bd = run(
        &mut db,
        "MATCH (n:A) WHERE n.b_data CONTAINS 'econd' RETURN n.b_data AS v",
    );
    assert_eq!(strings(&bd, "v"), vec!["second thing"]);
    db.begin_write().unwrap();
    db.drop_fulltext_index("A", "b_data").unwrap();
    db.commit().unwrap();
    let b = run(
        &mut db,
        "MATCH (n:A) WHERE n.b CONTAINS 'irst' RETURN n.b AS v",
    );
    assert_eq!(strings(&b, "v"), vec!["first thing"]);
}

#[test]
fn g4_crafted_fts_table_name_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g4.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.begin_write().unwrap();
        db.create_fulltext_index_word_multi("A", &["t".to_string()])
            .unwrap();
        db.commit().unwrap();
        run(&mut db, "CREATE (:A {t: 'keep me'}), (:A {t: 'drop me'})");
    }
    {
        // A table name that, interpolated unescaped, turns a single-row
        // DELETE into `DELETE ... WHERE rowid=?1 OR 1 --`.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE \"node_fts_multi_A_1\"\" WHERE rowid=?1 OR 1 --\" \
             USING fts5(t, tokenize='unicode61');",
        )
        .unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    run(&mut db, "MATCH (n:A {t: 'drop me'}) DETACH DELETE n");
    let rows = run_p(
        &mut db,
        "CALL fts.search('A', 't', $q) YIELD node RETURN node.t AS t",
        &[("q", Value::String("me".to_string()))],
    );
    assert_eq!(strings(&rows, "t"), vec!["keep me"]);
    // Writes to the label keep working.
    run(&mut db, "CREATE (:A {t: 'new one'})");
    let rows = run_p(
        &mut db,
        "CALL fts.search('A', 't', $q) YIELD node RETURN node.t AS t",
        &[("q", Value::String("one".to_string()))],
    );
    assert_eq!(strings(&rows, "t"), vec!["new one"]);
}

// -- Shared helpers for crafted database files ------------------------------------

const GDBL_APP_ID: i32 = 0x4744_424C;

fn open_err(path: &std::path::Path) -> GraphError {
    match Database::open(path) {
        Ok(_) => panic!("expected Database::open to fail"),
        Err(e) => e,
    }
}

/// Assert a `Storage` error carrying `SQLITE_NOTADB` whose message contains
/// `needle`.
fn assert_notadb(err: GraphError, needle: &str) {
    match err {
        GraphError::Storage {
            source: rusqlite::Error::SqliteFailure(e, Some(msg)),
            ..
        } => {
            assert_eq!(e.code, rusqlite::ErrorCode::NotADatabase, "{msg}");
            assert!(msg.contains(needle), "unexpected message: {msg}");
        }
        other => panic!("expected Storage(NOTADB) error, got {other:?}"),
    }
}

fn header(path: &std::path::Path) -> (i32, i32, String) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let app: i32 = conn
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .unwrap();
    let ver: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    (app, ver, mode)
}

fn table_names(path: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    names
}

// -- G7: foreign SQLite files are not adopted -----------------------------------------

#[test]
fn g7_open_refuses_foreign_file_and_leaves_it_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);")
            .unwrap();
    }
    assert_notadb(open_err(&path), "non-graphdblite table `users`");
    // No stamping, no WAL switch, no graphdblite tables.
    assert_eq!(header(&path), (0, 0, "delete".to_string()));
    assert_eq!(table_names(&path), vec!["users"]);
}

#[test]
fn g7_open_refuses_graphdblite_tables_mixed_with_foreign_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.db");
    {
        let _ = Database::open(&path).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA application_id = 0; CREATE TABLE extra (x);")
            .unwrap();
    }
    assert_notadb(open_err(&path), "non-graphdblite table `extra`");
}

#[test]
fn g7_pre_identity_graphdblite_file_is_still_adopted() {
    // A graphdblite file from before the identity stamp: graphdblite tables
    // only, application_id 0, user_version 0.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    {
        let mut db = Database::open(&path).unwrap();
        run(&mut db, "CREATE (:Person {name: 'Ann'})");
        db.begin_write().unwrap();
        db.create_index("Person", "name").unwrap();
        db.create_fulltext_index("Person", "name").unwrap();
        db.commit().unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA application_id = 0; PRAGMA user_version = 0;")
            .unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        int(&mut db, "MATCH (n:Person {name: 'Ann'}) RETURN count(n)"),
        1
    );
    drop(db);
    assert_eq!(header(&path), (GDBL_APP_ID, 2, "wal".to_string()));
}

#[test]
fn g7_empty_file_is_adopted_and_stamped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.db");
    std::fs::File::create(&path).unwrap();
    {
        let mut db = Database::open(&path).unwrap();
        run(&mut db, "CREATE (:X)");
    }
    assert_eq!(header(&path), (GDBL_APP_ID, 2, "wal".to_string()));
}

// -- G6: schema-defined SQL in untrusted files ------------------------------------------

#[test]
fn g6_open_rejects_trigger_on_core_table_and_it_never_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trig.db");
    {
        let _ = Database::open(&path).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER pwn AFTER INSERT ON nodes BEGIN \
               INSERT OR REPLACE INTO metadata (key, value) VALUES ('pwned', x'01'); \
             END;",
        )
        .unwrap();
    }
    assert_notadb(
        open_err(&path),
        "trigger `pwn` on graphdblite table `nodes`",
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    let pwned: i64 = conn
        .query_row(
            "SELECT count(*) FROM metadata WHERE key = 'pwned'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pwned, 0);
}

#[test]
fn g6_open_rejects_trigger_on_index_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trig2.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.begin_write().unwrap();
        db.create_index("Person", "name").unwrap();
        db.commit().unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER t2 AFTER INSERT ON \"node_idx$^person$name\" BEGIN SELECT 1; END;",
        )
        .unwrap();
    }
    assert_notadb(open_err(&path), "trigger `t2`");
}

#[test]
fn g6_open_rejects_view_shadowing_a_core_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("view.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA application_id = {GDBL_APP_ID}; PRAGMA user_version = 2; \
             CREATE VIEW nodes AS SELECT 1 AS key, '' AS label, x'' AS value;"
        ))
        .unwrap();
    }
    assert_notadb(open_err(&path), "view `nodes`");
}

#[test]
fn g6_fulltext_indexes_work_with_hardened_connection() {
    // trusted_schema=OFF + DEFENSIVE must not break FTS5 reads or writes,
    // including after reopening the file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fts.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.begin_write().unwrap();
        db.create_fulltext_index("Doc", "body").unwrap();
        db.create_fulltext_index_word("Doc", "title").unwrap();
        db.commit().unwrap();
        run(
            &mut db,
            "CREATE (:Doc {body: 'hello world', title: 'Greeting card'})",
        );
    }
    let mut db = Database::open(&path).unwrap();
    run(
        &mut db,
        "CREATE (:Doc {body: 'other text', title: 'Card games'})",
    );
    let rows = run(
        &mut db,
        "MATCH (n:Doc) WHERE n.body CONTAINS 'llo wo' RETURN n.body AS b",
    );
    assert_eq!(strings(&rows, "b"), vec!["hello world"]);
    let rows = run(
        &mut db,
        "CALL fts.search('Doc', 'title', 'card') YIELD node RETURN node.title AS t",
    );
    assert_eq!(strings(&rows, "t"), vec!["Card games", "Greeting card"]);
}

// -- G9: snapshot_to file creation ----------------------------------------------------

#[cfg(unix)]
#[test]
fn g9_snapshot_is_owner_only_even_with_permissive_umask_and_existing_target_errors() {
    use std::os::unix::fs::PermissionsExt;
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:S {v: 1})");
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("snap.db");
    db.snapshot_to(&dst).unwrap();
    let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let mut snap = Database::open(&dst).unwrap();
    assert_eq!(int(&mut snap, "MATCH (n:S) RETURN n.v"), 1);

    // An existing (even empty) target is refused and left as it was.
    let existing = dir.path().join("existing.db");
    std::fs::write(&existing, b"").unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
    match db.snapshot_to(&existing) {
        Err(GraphError::Storage {
            source: rusqlite::Error::SqliteFailure(e, Some(msg)),
            ..
        }) => {
            assert_eq!(e.code, rusqlite::ErrorCode::CannotOpen);
            assert!(msg.contains("already exists"), "{msg}");
        }
        other => panic!("expected CannotOpen storage error, got {other:?}"),
    }
    assert_eq!(std::fs::metadata(&existing).unwrap().len(), 0);
    let mode = std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);
}

#[test]
fn g9_snapshot_failure_removes_placeholder() {
    let mut db = Database::open_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // The parent directory does not exist: creation fails, nothing is left.
    let dst = dir.path().join("missing").join("snap.db");
    match db.snapshot_to(&dst) {
        Err(GraphError::Storage {
            source: rusqlite::Error::SqliteFailure(e, _),
            ..
        }) => assert_eq!(e.code, rusqlite::ErrorCode::CannotOpen),
        other => panic!("expected CannotOpen storage error, got {other:?}"),
    }
    assert!(!dst.exists());
}

// -- G8: schema v2 migration + case-only collisions ----------------------------------------

fn explain(db: &mut Database, q: &str) -> String {
    run(db, &format!("EXPLAIN {q}"))
        .iter()
        .flat_map(|r| r.values().cloned().collect::<Vec<_>>())
        .map(|v| match v {
            Value::String(s) => s,
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `CALL db.indexes()` rows as sorted `(label, property, kind)`.
fn indexes(db: &mut Database) -> Vec<(String, String, String)> {
    let rows = run(
        db,
        "CALL db.indexes() YIELD label, property, kind RETURN label, property, kind",
    );
    let mut out: Vec<(String, String, String)> = rows
        .iter()
        .map(|r| {
            let s = |c: &str| match r.get(c) {
                Some(Value::String(s)) => s.clone(),
                other => panic!("expected string in {c}, got {other:?}"),
            };
            (s("label"), s("property"), s("kind"))
        })
        .collect();
    out.sort();
    out
}

fn ix(label: &str, prop: &str, kind: &str) -> (String, String, String) {
    (label.to_string(), prop.to_string(), kind.to_string())
}

/// Build a schema-version-1 database the way graphdblite 0.1.x laid it out:
/// legacy `_`-named btree / FTS tables (left empty, as their v1 content is
/// not trusted), a v1 multi-prop FTS table, and raw-case `$`-named tables.
fn v1_fixture(path: &std::path::Path) {
    {
        let mut db = Database::open(path).unwrap();
        run(
            &mut db,
            "CREATE (:Person {name: 'Ann', age: 30, bio: 'likes hiking'}), \
                    (:Person {name: 'Bob', age: 40, bio: 'enjoys chess'}), \
                    (:My_Label {first_name: 'Cy'}), \
                    (:A_b {c: 1}), (:A {b_c: 2}), \
                    (:Article {title: 'rust tips', body: 'borrow checker'})",
        );
        // Raw-case `$` tables are renamed in place, so build them for real.
        db.begin_write().unwrap();
        db.create_index("Person", "age").unwrap();
        db.create_fulltext_index("Person", "bio").unwrap();
        db.commit().unwrap();
    }
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "ALTER TABLE \"node_idx$^person$age\" RENAME TO \"node_idx$Person$age\";
         ALTER TABLE \"node_fts$^person$bio$\" RENAME TO \"node_fts$Person$bio\";
         CREATE TABLE \"node_idx_Person_name\" (key BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
         CREATE TABLE \"node_idx_My_Label_first_name\" (key BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
         CREATE TABLE \"node_idx_A_b_c\" (key BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
         CREATE TABLE \"node_idx_Q_r_s\" (key BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
         CREATE VIRTUAL TABLE \"node_fts_Person_name\" USING fts5(content, tokenize='trigram case_sensitive 0');
         CREATE VIRTUAL TABLE \"node_fts_multi_Article_1\" USING fts5(\"title\", \"body\", tokenize='unicode61');
         PRAGMA user_version = 1;",
    )
    .unwrap();
}

#[test]
fn g8_v1_database_is_migrated_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.db");
    v1_fixture(&path);

    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        indexes(&mut db),
        vec![
            // Ambiguous `node_idx_A_b_c`: both readings have data, so both
            // get an index. `node_idx_Q_r_s` (no data) is dropped.
            ix("A", "b_c", "btree"),
            ix("A_b", "c", "btree"),
            ix("Article", "body", "fulltext_word"),
            ix("Article", "title", "fulltext_word"),
            ix("My_Label", "first_name", "btree"),
            ix("Person", "age", "btree"),
            ix("Person", "bio", "fulltext"),
            ix("Person", "name", "btree"),
            ix("Person", "name", "fulltext_ci"),
        ]
    );
    drop(db);
    let names = table_names(&path);
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("node_idx_") || n.starts_with("node_fts_multi_")),
        "legacy tables left behind: {names:?}"
    );
    assert_eq!(header(&path).1, 2);

    // Rebuilt and renamed indexes are used and return the right rows.
    let mut db = Database::open(&path).unwrap();
    let q = "MATCH (n:Person) WHERE n.name = 'Bob' RETURN n.age AS a";
    assert!(
        explain(&mut db, q).contains("IndexLookup"),
        "{}",
        explain(&mut db, q)
    );
    assert_eq!(int(&mut db, q), 40);
    assert_eq!(
        int(&mut db, "MATCH (n:Person) WHERE n.age = 30 RETURN count(n)"),
        1
    );
    assert_eq!(
        int(&mut db, "MATCH (n:A_b) WHERE n.c = 1 RETURN count(n)"),
        1
    );
    assert_eq!(
        int(&mut db, "MATCH (n:A) WHERE n.b_c = 2 RETURN count(n)"),
        1
    );
    let rows = run(
        &mut db,
        "MATCH (n:My_Label) WHERE n.first_name = 'Cy' RETURN n.first_name AS f",
    );
    assert_eq!(strings(&rows, "f"), vec!["Cy"]);
    let rows = run(
        &mut db,
        "MATCH (n:Person) WHERE n.bio CONTAINS 'hik' RETURN n.name AS n",
    );
    assert_eq!(strings(&rows, "n"), vec!["Ann"]);
    let rows = run(
        &mut db,
        "MATCH (n:Person) WHERE toLower(n.name) CONTAINS toLower('BOB') RETURN n.name AS n",
    );
    assert_eq!(strings(&rows, "n"), vec!["Bob"]);
    let rows = run(
        &mut db,
        "CALL fts.search('Article', 'body', 'borrow') YIELD node RETURN node.title AS t",
    );
    assert_eq!(strings(&rows, "t"), vec!["rust tips"]);

    // ... and maintained by later writes.
    run(
        &mut db,
        "CREATE (:Person {name: 'Cat', age: 30, bio: 'hiking too'})",
    );
    assert_eq!(
        int(&mut db, "MATCH (n:Person) WHERE n.age = 30 RETURN count(n)"),
        2
    );
    let rows = run(
        &mut db,
        "MATCH (n:Person) WHERE n.bio CONTAINS 'hik' RETURN n.name AS n",
    );
    assert_eq!(strings(&rows, "n"), vec!["Ann", "Cat"]);

    // Re-opening is a no-op (already version 2).
    drop(db);
    let before = table_names(&path);
    let _ = Database::open(&path).unwrap();
    assert_eq!(table_names(&path), before);
}

#[test]
fn g8_case_only_different_labels_get_separate_indexes() {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:Person {name: 'upper', a: 1, b: 2, bio: 'Upper case bio'}), \
                (:person {name: 'lower', a: 1, b: 2, bio: 'lower case bio'})",
    );
    // Before the fix each second create failed with a raw SQLite
    // "table ... already exists" error.
    db.begin_write().unwrap();
    db.create_index("Person", "name").unwrap();
    db.create_index("person", "name").unwrap();
    db.create_composite_index("Person", &["a", "b"]).unwrap();
    db.create_composite_index("person", &["a", "b"]).unwrap();
    db.create_fulltext_index("Person", "bio").unwrap();
    db.create_fulltext_index("person", "bio").unwrap();
    db.create_fulltext_index_word_multi("Doc", &["t".to_string()])
        .unwrap();
    db.create_fulltext_index_word_multi("doc", &["t".to_string()])
        .unwrap();
    db.commit().unwrap();
    run(
        &mut db,
        "CREATE (:Doc {t: 'Upper doc'}), (:doc {t: 'lower doc'})",
    );

    for (label, name) in [("Person", "upper"), ("person", "lower")] {
        let q = format!("MATCH (n:{label}) WHERE n.name = '{name}' RETURN n.name AS n");
        assert!(explain(&mut db, &q).contains("IndexLookup"));
        assert_eq!(strings(&run(&mut db, &q), "n"), vec![name]);
        let q = format!("MATCH (n:{label}) WHERE n.a = 1 AND n.b = 2 RETURN n.name AS n");
        assert_eq!(strings(&run(&mut db, &q), "n"), vec![name]);
        let q = format!("MATCH (n:{label}) WHERE n.bio CONTAINS 'case' RETURN n.name AS n");
        assert_eq!(strings(&run(&mut db, &q), "n"), vec![name]);
    }
    let rows = run(
        &mut db,
        "CALL fts.search('doc', '*', 'doc') YIELD node RETURN node.t AS t",
    );
    assert_eq!(strings(&rows, "t"), vec!["lower doc"]);
    let rows = run(
        &mut db,
        "CALL fts.search('Doc', 't', 'doc') YIELD node RETURN node.t AS t",
    );
    assert_eq!(strings(&rows, "t"), vec!["Upper doc"]);

    assert_eq!(
        indexes(&mut db),
        vec![
            ix("Doc", "t", "fulltext_word"),
            ix("Person", "a", "btree"),
            ix("Person", "b", "btree"),
            ix("Person", "bio", "fulltext"),
            ix("Person", "name", "btree"),
            ix("doc", "t", "fulltext_word"),
            ix("person", "a", "btree"),
            ix("person", "b", "btree"),
            ix("person", "bio", "fulltext"),
            ix("person", "name", "btree"),
        ]
    );
    // Dropping one leaves the other.
    db.begin_write().unwrap();
    db.drop_index("Person", "name").unwrap();
    db.drop_fulltext_index("person", "bio").unwrap();
    db.commit().unwrap();
    let q = "MATCH (n:person) WHERE n.name = 'lower' RETURN n.name AS n";
    assert!(explain(&mut db, q).contains("IndexLookup"));
    let q = "MATCH (n:Person) WHERE n.bio CONTAINS 'case' RETURN n.name AS n";
    assert_eq!(strings(&run(&mut db, q), "n"), vec!["upper"]);
}

#[test]
fn g8_multi_prop_fts_rejects_case_only_duplicate_properties() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    match db.create_fulltext_index_word_multi("Doc", &["t".to_string(), "T".to_string()]) {
        Err(GraphError::IndexAlreadyExists { properties, .. }) => {
            assert_eq!(properties, vec!["T".to_string()])
        }
        other => panic!("expected IndexAlreadyExists, got {other:?}"),
    }
    db.rollback().unwrap();
}
