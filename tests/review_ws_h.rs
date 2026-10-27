//! WS-H review findings: WITH-scope pattern predicates / EXISTS (N1, N2),
//! stack safety on small thread stacks (#19), column names (N3), temporal
//! parse errors (N4) and UNWIND inside EXISTS.

use graphdblite::{Database, ErrorCode, GraphError, Record, Value};

fn run(db: &mut Database, q: &str) -> Vec<Record> {
    db.execute(q)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

fn err(db: &mut Database, q: &str) -> GraphError {
    match db.execute(q) {
        Ok(rows) => panic!("expected an error from {q}, got {rows:?}"),
        Err(e) => e,
    }
}

fn strs(rows: &[Record], col: &str) -> Vec<String> {
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

fn mem() -> Database {
    Database::open_memory().unwrap()
}

// ---------------------------------------------------------------- N1

fn n1_db() -> Database {
    let mut db = mem();
    run(
        &mut db,
        "CREATE (:A {name:'x'})-[:R]->(:B), (:A {name:'y'})",
    );
    db
}

#[test]
fn n1_pattern_predicate_on_renamed_node() {
    let mut db = n1_db();
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a AS z WHERE (z)-[:R]->() RETURN z.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["x"]);
}

#[test]
fn n1_exists_pattern_on_renamed_node() {
    let mut db = n1_db();
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a AS z WHERE EXISTS { (z)-[:R]->() } RETURN z.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["x"]);
}

#[test]
fn n1_exists_match_on_renamed_node() {
    let mut db = n1_db();
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a AS z WHERE EXISTS { MATCH (z)-[:R]->() } RETURN z.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["x"]);
}

#[test]
fn n1_negated_pattern_predicate_on_renamed_node() {
    let mut db = n1_db();
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a AS z WHERE NOT (z)-[:R]->() RETURN z.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["y"]);
}

#[test]
fn n1_predicate_false_for_all_rows() {
    let mut db = n1_db();
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a AS z WHERE (z)-[:NOPE]->() RETURN z.name AS n",
    );
    assert!(rows.is_empty(), "got {rows:?}");
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a AS z WHERE EXISTS { MATCH (z)-[:NOPE]->() } RETURN z.name AS n",
    );
    assert!(rows.is_empty(), "got {rows:?}");
}

#[test]
fn n1_pattern_predicate_with_relationship_rename() {
    let mut db = n1_db();
    let rows = run(
        &mut db,
        "MATCH (a:A)-[r:R]->(b) WITH a AS z, r AS q WHERE (z)-[q]->() RETURN z.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["x"]);
    let rows = run(
        &mut db,
        "MATCH (a:A) OPTIONAL MATCH (a)-[r:R]->() WITH a AS z, r AS q WHERE EXISTS { MATCH ()-[q]->() } RETURN z.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["x"]);
}

#[test]
fn n1_string_literal_resembling_an_alias_reference_is_not_a_reference() {
    // The alias text appears only inside a string literal: the conjunct still
    // reads the dropped variable `a`, so it must filter before projection.
    let mut db = n1_db();
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a AS z WHERE a.name = 'Variable(\"z\")' OR a.name = 'x' RETURN z.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["x"]);
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a.name AS z WHERE EXISTS { (a)-[:R]->() } AND a.name <> 'Some(\"z\")' RETURN z",
    );
    assert_eq!(strs(&rows, "z"), vec!["x"]);
}

#[test]
fn n1_control_unrenamed_and_later_with() {
    let mut db = n1_db();
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a WHERE (a)-->() RETURN a.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["x"]);
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a AS z WITH z WHERE (z)-[:R]->() RETURN z.name AS n",
    );
    assert_eq!(strs(&rows, "n"), vec!["x"]);
}

// ---------------------------------------------------------------- N2

fn n2_db() -> Database {
    let mut db = mem();
    run(&mut db, "CREATE (:M {k:1}), (:M {k:2}), (:L {k:1})");
    db
}

#[test]
fn n2_scalar_alias_visible_in_exists_match_where() {
    let mut db = n2_db();
    let rows = run(
        &mut db,
        "MATCH (m:M) WITH m.k AS mk WHERE EXISTS { MATCH (n:L) WHERE n.k = mk } RETURN mk",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("mk"), Some(&Value::I64(1)));
}

#[test]
fn n2_scalar_alias_visible_in_exists_inline_props() {
    let mut db = n2_db();
    let rows = run(
        &mut db,
        "MATCH (m:M) WITH m.k AS mk WHERE EXISTS { (n:L {k: mk}) } RETURN mk",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("mk"), Some(&Value::I64(1)));
}

#[test]
fn n2_scalar_alias_exists_false_for_all_rows() {
    let mut db = n2_db();
    let rows = run(
        &mut db,
        "MATCH (m:M) WITH m.k + 10 AS mk WHERE EXISTS { MATCH (n:L) WHERE n.k = mk } RETURN mk",
    );
    assert!(rows.is_empty(), "got {rows:?}");
}

#[test]
fn n2_undefined_variable_still_rejected() {
    let mut db = n2_db();
    let e = err(
        &mut db,
        "MATCH (m:M) WITH m.k AS mk WHERE EXISTS { MATCH (n:L) WHERE n.k = nope } RETURN mk",
    );
    assert!(
        matches!(&e, GraphError::Query(q) if q.code() == ErrorCode::UndefinedVariable),
        "got {e:?}"
    );
}

// ---------------------------------------------------------------- N3

#[test]
fn n3_unaliased_list_comprehensions_get_distinct_names() {
    let mut db = mem();
    let rows = run(&mut db, "RETURN [x IN [1,2] | x*2], [x IN [1] | x]");
    assert_eq!(rows.len(), 1);
    let keys: Vec<String> = rows[0].keys().map(|k| k.to_string()).collect();
    assert_eq!(keys, vec!["[x IN [1, 2] | x * 2]", "[x IN [1] | x]"]);
}

#[test]
fn n3_pattern_comprehension_names_distinct() {
    let mut db = mem();
    run(&mut db, "CREATE (:A)-[:R]->(:B)");
    let rows = run(
        &mut db,
        "MATCH (a:A) RETURN [(a)-[:R]->(b) | b], [(a)-[:R]->(c) | c]",
    );
    let keys: Vec<String> = rows[0].keys().map(|k| k.to_string()).collect();
    assert_eq!(keys, vec!["[(a)-[:R]->(b) | b]", "[(a)-[:R]->(c) | c]"]);
}

#[test]
fn n3_other_expression_kinds_get_distinct_names() {
    let mut db = mem();
    let rows = run(
        &mut db,
        "RETURN CASE WHEN true THEN 1 END, CASE WHEN false THEN 2 ELSE 3 END, \
         all(x IN [1] WHERE x > 0), any(x IN [1] WHERE x > 5)",
    );
    let keys: Vec<String> = rows[0].keys().map(|k| k.to_string()).collect();
    assert_eq!(
        keys,
        vec![
            "CASE WHEN true THEN 1 END",
            "CASE WHEN false THEN 2 ELSE 3 END",
            "all(x IN [1] WHERE x > 0)",
            "any(x IN [1] WHERE x > 5)",
        ]
    );
    assert_eq!(
        rows[0].get("CASE WHEN false THEN 2 ELSE 3 END"),
        Some(&Value::I64(3))
    );
}

// ---------------------------------------------------------------- N4

fn assert_temporal_arg_error(e: GraphError) {
    let msg = e.to_string();
    assert!(
        matches!(&e, GraphError::Query(q) if q.code() == ErrorCode::InvalidArgumentValue),
        "wrong class: {e:?}"
    );
    assert!(!msg.contains("serialization"), "{msg}");
    assert!(!msg.contains("()"), "{msg}");
    assert!(!msg.contains("2020-13-45"), "echoes input: {msg}");
}

#[test]
fn n4_temporal_parse_errors_are_argument_errors_without_echo() {
    let mut db = mem();
    assert_temporal_arg_error(err(&mut db, "RETURN date('2020-13-45')"));
    assert_temporal_arg_error(err(&mut db, "RETURN datetime('2020-13-45T00:00:00')"));
    assert_temporal_arg_error(err(&mut db, "RETURN localdatetime('2020-13-45T00:00:00')"));
    assert_temporal_arg_error(err(&mut db, "RETURN time('2020-13-45')"));
    assert_temporal_arg_error(err(&mut db, "RETURN duration('2020-13-45')"));
}

// ---------------------------------------------------------------- UNWIND in EXISTS

#[test]
fn exists_with_leading_unwind() {
    let mut db = n2_db();
    let rows = run(
        &mut db,
        "MATCH (m:M) WHERE EXISTS { UNWIND [1] AS x MATCH (n:L) WHERE n.k = x AND n.k = m.k } RETURN m.k AS k",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("k"), Some(&Value::I64(1)));
    let rows = run(
        &mut db,
        "MATCH (m:M) WHERE EXISTS { UNWIND [7] AS x MATCH (n:L) WHERE n.k = x } RETURN m.k AS k",
    );
    assert!(rows.is_empty(), "got {rows:?}");
}

// ---------------------------------------------------------------- #19 / R1: stack safety

/// The Rust default stack for spawned threads, and what Python and Node
/// worker threads commonly get.
const SMALL_STACK: usize = 2 * 1024 * 1024;

/// Run `query` on a fresh in-memory database on a thread with a 2 MiB stack.
/// A stack overflow aborts the whole test process, so merely returning
/// proves the query neither crashed nor ran away.
fn run_on_small_stack(query: String) -> Result<Vec<Record>, GraphError> {
    std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(move || {
            let mut db = Database::open_memory().unwrap();
            db.execute(&query)
        })
        .unwrap()
        .join()
        .expect("query thread panicked")
}

fn assert_size_limit(res: Result<Vec<Record>, GraphError>, what_contains: &str, limit: usize) {
    match res {
        Err(GraphError::SizeLimit {
            what,
            limit: l,
            actual,
            ..
        }) => {
            assert!(what.contains(what_contains), "what = {what}");
            assert_eq!(l, limit);
            assert!(actual > limit, "actual {actual} must exceed the limit");
        }
        other => panic!("expected SizeLimit({what_contains}), got {other:?}"),
    }
}

/// `n` levels of `open ... close` around `1`.
fn nested(open: &str, close: &str, n: usize) -> String {
    let core = if open == "NOT " { "true" } else { "1" };
    format!("RETURN {}{core}{} AS x", open.repeat(n), close.repeat(n))
}

#[test]
fn deep_expressions_do_not_overflow_a_small_stack() {
    // (shape name, opener, closer, first depth the parser rejects)
    let shapes: [(&str, &str, &str, usize, &str); 5] = [
        ("abs", "abs(", ")", 256, "expression nesting depth"),
        ("list", "[", "]", 256, "expression nesting depth"),
        (
            "case",
            "CASE WHEN true THEN ",
            " END",
            256,
            "expression nesting depth",
        ),
        ("not", "NOT ", "", 256, "expression nesting depth"),
        ("paren", "(", ")", 256, "expression nesting depth"),
    ];
    for (name, open, close, reject_at, what) in shapes {
        // Just under the limit: must succeed, not abort.
        let rows = run_on_small_stack(nested(open, close, reject_at - 1))
            .unwrap_or_else(|e| panic!("{name} at {}: {e}", reject_at - 1));
        assert_eq!(rows.len(), 1, "{name}");
        // At the limit: a clean SizeLimit.
        assert_size_limit(
            run_on_small_stack(nested(open, close, reject_at)),
            what,
            256,
        );
    }
}

#[test]
fn deep_abs_returns_the_correct_value() {
    let rows = run_on_small_stack(nested("abs(", ")", 255)).unwrap();
    assert_eq!(rows[0].get("x"), Some(&Value::I64(1)));
}

#[test]
fn deep_list_comprehension_does_not_overflow_a_small_stack() {
    let q = |n: usize| format!("RETURN {}[1]{} AS x", "[x IN ".repeat(n), " | x]".repeat(n));
    let rows = run_on_small_stack(q(200)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_size_limit(run_on_small_stack(q(256)), "bracket nesting depth", 256);
}

#[test]
fn deep_nested_exists_does_not_overflow_a_small_stack() {
    let q = |n: usize| {
        format!(
            "MATCH (n) WHERE {}true{} RETURN n",
            "EXISTS { MATCH (m) WHERE ".repeat(n),
            " }".repeat(n)
        )
    };
    let rows = run_on_small_stack(q(60)).unwrap();
    assert_eq!(rows.len(), 0);
    assert_size_limit(run_on_small_stack(q(300)), "bracket nesting depth", 256);
}

#[test]
fn long_clause_chains_do_not_overflow_a_small_stack() {
    let with_chain = |n: usize| format!("{}RETURN x", "WITH 1 AS x ".repeat(n));
    let unwind_chain = |n: usize| format!("{}RETURN x", "UNWIND [1] AS x ".repeat(n));
    let set_chain = |n: usize| {
        let items: Vec<String> = (0..n).map(|i| format!("n.p{i} = 1")).collect();
        format!("CREATE (n) SET {}", items.join(", "))
    };
    let hop_chain = |n: usize| format!("MATCH (a){} RETURN count(*) AS c", "-[:R]->()".repeat(n));
    let match_chain = |n: usize| {
        let clauses: String = (0..n).map(|i| format!("MATCH (n{i}) ")).collect();
        format!("{clauses}RETURN 1")
    };

    let limit = 1024;
    // WITH / UNWIND: one counted item per clause.
    assert_eq!(run_on_small_stack(with_chain(limit)).unwrap().len(), 1);
    assert_size_limit(
        run_on_small_stack(with_chain(limit + 1)),
        "pattern elements",
        limit,
    );
    // The leading UNWIND is the statement itself, so one more fits.
    assert_eq!(
        run_on_small_stack(unwind_chain(limit + 1)).unwrap().len(),
        1
    );
    assert_size_limit(
        run_on_small_stack(unwind_chain(limit + 2)),
        "pattern elements",
        limit,
    );
    // The CREATE and SET clauses count 2 items besides the SET items.
    run_on_small_stack(set_chain(limit - 2)).unwrap();
    assert_size_limit(
        run_on_small_stack(set_chain(limit - 1)),
        "pattern elements",
        limit,
    );
    // MATCH (a) + n x (rel, node): the MATCH clause counts 1, (a) 1, 2 per hop.
    let rows = run_on_small_stack(hop_chain(511)).unwrap();
    assert_eq!(rows[0].get("c"), Some(&Value::I64(0)));
    assert_size_limit(
        run_on_small_stack(hop_chain(512)),
        "pattern elements",
        limit,
    );
    // MATCH clauses: 2 counted items each.
    run_on_small_stack(match_chain(limit / 2)).unwrap();
    assert_size_limit(
        run_on_small_stack(match_chain(limit / 2 + 1)),
        "pattern elements",
        limit,
    );
}

// ---------------------------------------------------------------- D10: errors do not echo values

#[test]
fn errors_do_not_echo_runtime_values() {
    let mut db = mem();
    let queries = [
        "RETURN date('SECRET')",
        "RETURN date('2020-SECRET')",
        "RETURN date('2020-13-SECRET')",
        "RETURN time('SECRET')",
        "RETURN time('12:SECRET')",
        "RETURN localtime('SECRET')",
        "RETURN datetime('SECRET')",
        "RETURN datetime('2020-01-01TSECRET')",
        "RETURN datetime('2020-01-01T00:00:00[Mars/SECRET]')",
        "RETURN datetime('2020-01-01T00:00:00+SECRET')",
        "RETURN datetime('2020-01-01T00:00:00+99:00')",
        "RETURN localdatetime('SECRET')",
        "RETURN duration('SECRET')",
        "RETURN duration('PSECRET')",
        "RETURN duration('P1YSECRET')",
        "RETURN duration('PT1HSECRET')",
        "RETURN datetime({year: 2020, timezone: 'Mars/SECRET'})",
        "UNWIND 'SECRET' AS x RETURN x",
        "WITH 'SECRET' AS s UNWIND s AS x RETURN x",
    ];
    for q in queries {
        let e = err(&mut db, q);
        let msg = e.to_string();
        assert!(!msg.contains("SECRET"), "{q} echoes its input: {msg}");
        assert!(
            !matches!(e, GraphError::Serialization { .. }),
            "{q} is a serialization error: {msg}"
        );
    }
}

#[test]
fn unwind_non_list_reports_the_type_name() {
    let mut db = mem();
    let e = err(&mut db, "UNWIND 'SECRET' AS x RETURN x");
    assert!(
        e.to_string().contains("UNWIND requires a list, got String"),
        "{e}"
    );
}

#[test]
fn bulk_create_lists_are_not_limited_by_the_item_cap() {
    // CREATE pattern lists and REMOVE lists are processed iteratively, so a
    // large bulk CREATE keeps working (and fits a small stack).
    let nodes: Vec<String> = (0..3000).map(|i| format!("(:L {{k: {i}}})")).collect();
    let q = format!("CREATE {} RETURN 1 AS one", nodes.join(", "));
    let rows = run_on_small_stack(q).unwrap();
    assert_eq!(rows[0].get("one"), Some(&Value::I64(1)));
}
