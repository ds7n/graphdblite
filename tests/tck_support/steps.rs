//! Cucumber step definitions for the openCypher TCK.
//!
//! Each `#[given]`, `#[when]`, or `#[then]` attribute binds a Gherkin phrase
//! from the `.feature` files to a Rust function. The canonical TCK vocabulary
//! is relatively small (~12 distinct phrases); all are defined here.
//!
//! Step text includes trailing colons (e.g. `When executing query:`) because
//! the Gherkin parser treats everything before the docstring delimiter as the
//! step name.

use std::collections::HashMap;

use cucumber::{gherkin::Step, given, then, when};
use graphdblite::{Database, Value};

use super::compare;
use super::errors;
use super::graphs;
use super::world::{GraphCounts, World};

// ─── Given ───────────────────────────────────────────────────────────────

#[given("any graph")]
fn given_any_graph(world: &mut World) {
    world.db = Some(Database::open_memory().expect("open_memory"));
}

#[given("an empty graph")]
fn given_empty_graph(world: &mut World) {
    world.db = Some(Database::open_memory().expect("open_memory"));
}

#[given(expr = "the {word} graph")]
fn given_named_graph(world: &mut World, name: String) {
    world.db = Some(graphs::load_named_graph(&name).expect("load named graph"));
}

#[given("having executed:")]
fn having_executed(world: &mut World, step: &Step) {
    let query = step
        .docstring
        .as_ref()
        .expect("having executed requires a docstring")
        .trim();
    let db = world.db.as_mut().expect("database not initialized");
    let tx = db.begin_write().expect("begin_write");

    // TCK setup scripts may contain multiple sequential statements separated
    // by a newline followed by a Cypher keyword at the start of a line
    // (e.g. "CREATE (a)\nCREATE (a)-[:R]->(b)"). Split and execute each.
    for stmt in split_setup_statements(query) {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        tx.query(stmt).unwrap_or_else(|e| {
            panic!("setup query failed: {e}\nstatement: {stmt}\nfull script:\n{query}");
        });
    }
    tx.commit().expect("commit");
}

/// Split a multi-statement setup script into individual Cypher statements.
///
/// openCypher setup scripts use newlines before top-level keywords
/// (`CREATE`, `MATCH`, `UNWIND`, etc.) to separate statements. This is a
/// heuristic splitter — it looks for lines that start with a top-level
/// keyword and treats each as a new statement.
fn split_setup_statements(script: &str) -> Vec<String> {
    let keywords = [
        "CREATE", "MATCH", "MERGE", "DELETE", "SET", "UNWIND", "RETURN",
    ];
    let mut stmts = Vec::new();
    let mut current = String::new();

    for line in script.lines() {
        let trimmed = line.trim_start();
        let is_keyword_start = keywords.iter().any(|kw| {
            trimmed.starts_with(kw)
                && trimmed[kw.len()..].starts_with(|c: char| c.is_whitespace() || c == '(')
        });
        // Don't split when a CREATE follows another CREATE — the grammar
        // supports multi-clause CREATE natively (e.g. CREATE (a) CREATE (a)-[:R]->(b)).
        // Also don't split when CREATE follows UNWIND — UNWIND...CREATE is a single statement.
        let current_trimmed = current.trim_start();
        let current_starts_with_create = current_trimmed.starts_with("CREATE");
        let current_starts_with_unwind = current_trimmed.starts_with("UNWIND");
        let line_starts_with_create = trimmed.starts_with("CREATE");
        let keep_together =
            line_starts_with_create && (current_starts_with_create || current_starts_with_unwind);
        let should_split = is_keyword_start && !current.is_empty() && !keep_together;
        if should_split {
            stmts.push(current.clone());
            current.clear();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        stmts.push(current);
    }
    stmts
}

#[given("parameters are:")]
fn given_parameters(world: &mut World, step: &Step) {
    if let Some(table) = &step.table {
        for row in &table.rows {
            if row.len() >= 2 {
                let key = row[0].trim().to_string();
                let val = compare::parse_expected(row[1].trim())
                    .unwrap_or_else(|e| panic!("bad param value {}: {e}", row[1]));
                world.params.insert(key, val);
            }
        }
    }
}

// ─── When ────────────────────────────────────────────────────────────────

#[when("executing query:")]
fn when_executing_query(world: &mut World, step: &Step) {
    let query = step
        .docstring
        .as_ref()
        .expect("executing query requires a docstring")
        .trim();

    let db = world.db.as_mut().expect("database not initialized");

    // Snapshot counts before execution for side-effect checks.
    world.pre_counts = GraphCounts::snapshot(db.connection());

    // Build optional parameter map.
    let params: Option<HashMap<String, Value>> = if world.params.is_empty() {
        None
    } else {
        Some(world.params.drain().collect())
    };

    // Decide tx mode: if the query looks like a write, open a write tx.
    let is_write = {
        let upper = query.to_uppercase();
        upper.contains("CREATE")
            || upper.contains("DELETE")
            || upper.contains("SET ")
            || upper.contains("MERGE")
    };

    if is_write {
        let tx = db.begin_write().expect("begin_write");
        match tx.query_with_params(query, params.as_ref()) {
            Ok(records) => {
                world.last_result = Some(records);
                world.last_error = None;
                tx.commit().expect("commit");
            }
            Err(e) => {
                world.last_result = None;
                world.last_error = Some(e);
                tx.rollback().expect("rollback");
            }
        }
    } else {
        let tx = db.begin_read().expect("begin_read");
        match tx.query_with_params(query, params.as_ref()) {
            Ok(records) => {
                world.last_result = Some(records);
                world.last_error = None;
            }
            Err(e) => {
                world.last_result = None;
                world.last_error = Some(e);
            }
        }
        tx.commit().expect("commit read tx");
    }
}

// ─── Then ────────────────────────────────────────────────────────────────

#[then("the result should be, in any order:")]
fn then_result_any_order(world: &mut World, step: &Step) {
    let results = world
        .last_result
        .as_ref()
        .expect("expected results but query returned an error");
    let table = step.table.as_ref().expect("expected a result table");
    let (columns, expected_rows) = parse_result_table(table);
    compare::compare_result(results, &columns, &expected_rows, false)
        .expect("result comparison failed");
}

#[then("the result should be, in order:")]
fn then_result_in_order(world: &mut World, step: &Step) {
    let results = world
        .last_result
        .as_ref()
        .expect("expected results but query returned an error");
    let table = step.table.as_ref().expect("expected a result table");
    let (columns, expected_rows) = parse_result_table(table);
    compare::compare_result(results, &columns, &expected_rows, true)
        .expect("result comparison failed");
}

#[then("the result should be empty")]
fn then_result_empty(world: &mut World) {
    let results = world
        .last_result
        .as_ref()
        .expect("expected results but query returned an error");
    assert!(
        results.is_empty(),
        "expected empty result set, got {} rows",
        results.len()
    );
}

#[then("no side effects")]
fn then_no_side_effects(world: &mut World) {
    let db = world.db.as_mut().expect("database not initialized");
    let post = GraphCounts::snapshot(db.connection());
    let delta = world.pre_counts.delta(&post);
    for (name, count) in &delta {
        assert_eq!(*count, 0, "expected no side effects but {name} = {count}");
    }
}

#[then("the side effects should be:")]
fn then_side_effects(world: &mut World, step: &Step) {
    let db = world.db.as_mut().expect("database not initialized");
    let post = GraphCounts::snapshot(db.connection());
    let delta = world.pre_counts.delta(&post);

    if let Some(table) = &step.table {
        for row in &table.rows {
            if row.len() >= 2 {
                let name = row[0].trim();
                let expected: i64 = row[1].trim().parse().unwrap_or(0);
                let actual = *delta.get(name).unwrap_or(&0);
                assert_eq!(
                    actual, expected,
                    "side effect mismatch for {name}: expected {expected}, got {actual}"
                );
            }
        }
    }
}

#[then(regex = r"^a (\w+) should be raised")]
fn then_error_raised(world: &mut World, kind: String) {
    let error = world
        .last_error
        .as_ref()
        .unwrap_or_else(|| panic!("expected a {kind} to be raised, but query succeeded"));
    assert!(
        errors::matches_tck_error(error, &kind, None),
        "expected {kind} error, got: {error:?}"
    );
}

// ─── Helpers ─────────────────────────────────────────────────────────────

fn parse_result_table(table: &cucumber::gherkin::Table) -> (Vec<String>, Vec<Vec<Value>>) {
    if table.rows.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // First row is column headers.
    let columns: Vec<String> = table.rows[0].iter().map(|c| c.trim().to_string()).collect();

    let expected_rows: Vec<Vec<Value>> = table.rows[1..]
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| {
                    compare::parse_expected(cell.trim())
                        .unwrap_or_else(|e| panic!("bad expected cell {cell:?}: {e}"))
                })
                .collect()
        })
        .collect();

    (columns, expected_rows)
}
