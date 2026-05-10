//! Parameter-vs-literal equivalence harness for Phase 2 of plans/plan-cache.md.
//!
//! Verifies that for every plan-affecting decision (IndexLookup picked,
//! Scan-then-Filter, MERGE find-or-create, var-length prop filters), a
//! `$param` query produces the same result rows as the equivalent literal
//! query — across multiple distinct param values, on the same database.
//!
//! These tests are the safety net for the planner's "literals and params
//! are interchangeable" invariant. They fail loudly if a code path bakes
//! literal values into a plan in a way that doesn't have a param-aware
//! counterpart.

use std::collections::HashMap;

use graphdblite::{Database, Value};

fn open_with_people() -> Database {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (:Person {name: 'Bob',   age: 40})")
            .unwrap();
        tx.query("CREATE (:Person {name: 'Carol', age: 50})")
            .unwrap();
        tx.commit().unwrap();
    }
    db
}

fn names(rows: &[graphdblite::Record]) -> Vec<String> {
    let mut v: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.get("name") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    v.sort();
    v
}

#[test]
fn equality_filter_param_matches_literal_no_index() {
    let mut db = open_with_people();
    let tx = db.read_tx().unwrap();

    for &name in &["Alice", "Bob", "Carol", "Missing"] {
        let lit_q = format!("MATCH (p:Person) WHERE p.name = '{name}' RETURN p.name AS name");
        let lit_rows = tx.query(&lit_q).unwrap();

        let mut params = HashMap::new();
        params.insert("n".to_string(), Value::String(name.to_string()));
        let par_rows = tx
            .query_with_params(
                "MATCH (p:Person) WHERE p.name = $n RETURN p.name AS name",
                Some(&params),
            )
            .unwrap();

        assert_eq!(
            names(&lit_rows),
            names(&par_rows),
            "literal vs param mismatch for name={name}"
        );
    }
}

#[test]
fn equality_filter_param_matches_literal_with_index() {
    let mut db = open_with_people();
    {
        let tx = db.write_tx().unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();

    for &name in &["Alice", "Bob", "Carol", "Missing"] {
        let lit_q = format!("MATCH (p:Person {{name: '{name}'}}) RETURN p.name AS name");
        let lit_rows = tx.query(&lit_q).unwrap();

        let mut params = HashMap::new();
        params.insert("n".to_string(), Value::String(name.to_string()));
        let par_rows = tx
            .query_with_params(
                "MATCH (p:Person {name: $n}) RETURN p.name AS name",
                Some(&params),
            )
            .unwrap();

        assert_eq!(
            names(&lit_rows),
            names(&par_rows),
            "literal vs param mismatch for name={name}"
        );
    }
}

#[test]
fn explain_with_param_picks_indexlookup() {
    let mut db = open_with_people();
    {
        let tx = db.write_tx().unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }

    let mut params = HashMap::new();
    params.insert("n".to_string(), Value::String("Alice".to_string()));
    let rows = db
        .execute_with_params(
            "EXPLAIN MATCH (p:Person {name: $n}) RETURN p.name",
            Some(&params),
        )
        .unwrap();

    let plan = rows
        .iter()
        .filter_map(|r| match r.get("plan") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        plan.contains("IndexLookup"),
        "expected param query to plan as IndexLookup, got:\n{plan}"
    );
    assert!(
        plan.contains("$n"),
        "expected $n placeholder in EXPLAIN output, got:\n{plan}"
    );
}

#[test]
fn merge_find_or_create_param_matches_literal() {
    // Run literal and param flavors in two fresh databases so the find-or-create
    // semantics aren't confounded by prior runs.
    fn run(query: &str, params: Option<&HashMap<String, Value>>) -> Vec<String> {
        let mut db = Database::open_memory().unwrap();
        let tx = db.write_tx().unwrap();
        let _ = tx.query("CREATE (:Account {id: 'a1'})").unwrap();
        match params {
            Some(p) => tx.query_with_params(query, Some(p)).unwrap(),
            None => tx.query(query).unwrap(),
        };
        let rows = tx
            .query("MATCH (n:Account) RETURN n.id AS id ORDER BY n.id")
            .unwrap();
        tx.commit().unwrap();
        rows.iter()
            .filter_map(|r| match r.get("id") {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    // Existing id: MERGE must find, not create.
    let lit_existing = run("MERGE (n:Account {id: 'a1'}) RETURN n", None);
    let mut p1 = HashMap::new();
    p1.insert("id".to_string(), Value::String("a1".to_string()));
    let par_existing = run("MERGE (n:Account {id: $id}) RETURN n", Some(&p1));
    assert_eq!(lit_existing, par_existing);

    // Missing id: MERGE must create.
    let lit_missing = run("MERGE (n:Account {id: 'a2'}) RETURN n", None);
    let mut p2 = HashMap::new();
    p2.insert("id".to_string(), Value::String("a2".to_string()));
    let par_missing = run("MERGE (n:Account {id: $id}) RETURN n", Some(&p2));
    assert_eq!(lit_missing, par_missing);
}

#[test]
fn limit_param_matches_literal() {
    let mut db = open_with_people();
    let tx = db.read_tx().unwrap();
    for &n in &[0u64, 1, 2, 3, 100] {
        let lit_q = format!("MATCH (p:Person) RETURN p.name AS name ORDER BY p.name LIMIT {n}");
        let lit_rows = tx.query(&lit_q).unwrap();
        let mut params = HashMap::new();
        params.insert("k".to_string(), Value::I64(n as i64));
        let par_rows = tx
            .query_with_params(
                "MATCH (p:Person) RETURN p.name AS name ORDER BY p.name LIMIT $k",
                Some(&params),
            )
            .unwrap();
        assert_eq!(names(&lit_rows), names(&par_rows), "n={n}");
    }
}

#[test]
fn missing_param_errors_at_planning() {
    let mut db = open_with_people();
    let tx = db.read_tx().unwrap();
    let empty: HashMap<String, Value> = HashMap::new();
    let r = tx.query_with_params(
        "MATCH (p:Person) WHERE p.name = $missing RETURN p",
        Some(&empty),
    );
    assert!(r.is_err());
    let msg = r.unwrap_err().to_string();
    assert!(
        msg.contains("missing parameter") || msg.contains("MissingParameter"),
        "got: {msg}"
    );
}
