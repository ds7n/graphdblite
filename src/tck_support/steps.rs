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

use crate::procedures::{Def as ProcedureDef, Param as ProcParam};
use crate::{Database, Value};
use cucumber::{gherkin::Step, given, then, when};

use super::compare;
use super::errors;
use super::graphs;
use super::world::{dual_run_enabled, GraphCounts, World};
use crate::cypher::executor::Path;

// ─── Given ───────────────────────────────────────────────────────────────

#[given("any graph")]
fn given_any_graph(world: &mut World) {
    world.db = Some(Database::open_memory().expect("open_memory"));
    if dual_run_enabled() {
        world.db_slot = Some(Database::open_memory().expect("open_memory shadow"));
    }
}

#[given("an empty graph")]
fn given_empty_graph(world: &mut World) {
    world.db = Some(Database::open_memory().expect("open_memory"));
    if dual_run_enabled() {
        world.db_slot = Some(Database::open_memory().expect("open_memory shadow"));
    }
}

#[given(expr = "the {word} graph")]
fn given_named_graph(world: &mut World, name: String) {
    world.db = Some(graphs::load_named_graph(&name).expect("load named graph"));
    if dual_run_enabled() {
        world.db_slot = Some(graphs::load_named_graph(&name).expect("load named graph (shadow)"));
    }
}

#[given("having executed:")]
fn having_executed(world: &mut World, step: &Step) {
    let query = step
        .docstring
        .as_ref()
        .expect("having executed requires a docstring")
        .trim();
    let db = world.db.as_mut().expect("database not initialized");
    let tx = db.write_tx().expect("begin_write");

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

    // Mirror to the shadow DB so its post-setup state matches before the
    // first `When executing query:` step compares the two paths.
    if let Some(shadow) = world.db_slot.as_mut() {
        let tx = shadow.write_tx().expect("begin_write (shadow)");
        for stmt in split_setup_statements(query) {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            tx.query(stmt).unwrap_or_else(|e| {
                panic!(
                    "setup query failed (shadow): {e}\nstatement: {stmt}\nfull script:\n{query}"
                );
            });
        }
        tx.commit().expect("commit shadow");
    }
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
        // Also don't split when CREATE follows UNWIND or MATCH — those are
        // single multi-clause statements (UNWIND...CREATE, MATCH...CREATE).
        let current_trimmed = current.trim_start();
        let current_starts_with_create = current_trimmed.starts_with("CREATE");
        let current_starts_with_unwind = current_trimmed.starts_with("UNWIND");
        let current_starts_with_match = current_trimmed.starts_with("MATCH");
        let line_starts_with_create = trimmed.starts_with("CREATE");
        let line_starts_with_unwind = trimmed.starts_with("UNWIND");
        let line_starts_with_delete = trimmed.starts_with("DELETE");
        let line_starts_with_set = trimmed.starts_with("SET");
        let keep_together = (line_starts_with_create
            && (current_starts_with_create
                || current_starts_with_unwind
                || current_starts_with_match))
            || (line_starts_with_unwind && current_starts_with_unwind)
            // MATCH...DELETE and MATCH...SET are single multi-clause statements.
            || ((line_starts_with_delete || line_starts_with_set)
                && current_starts_with_match);
        // If the current buffer contains WITH, this is a multi-clause
        // pipeline (CREATE...WITH...UNWIND...CREATE) — don't split.
        let has_with = current.to_ascii_uppercase().contains("\nWITH ")
            || current.to_ascii_uppercase().starts_with("WITH ");
        let should_split = is_keyword_start && !current.is_empty() && !keep_together && !has_with;
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

#[given(regex = r"^there exists a procedure (.+):$")]
fn given_procedure(world: &mut World, step: &Step, sig: String) {
    let (name, inputs, outputs) = parse_procedure_signature(&sig);

    // Parse the data table (if present) into rows.
    let mut rows = Vec::new();
    if let Some(table) = &step.table {
        if table.rows.len() > 1 {
            let headers: Vec<String> = table.rows[0].iter().map(|c| c.trim().to_string()).collect();
            for data_row in &table.rows[1..] {
                let mut row = std::collections::HashMap::new();
                for (i, cell) in data_row.iter().enumerate() {
                    if i < headers.len() {
                        let value = compare::parse_expected(cell.trim())
                            .unwrap_or_else(|e| panic!("bad procedure data {cell}: {e}"));
                        row.insert(headers[i].clone(), value);
                    }
                }
                rows.push(row);
            }
        }
    }

    world.procedures.register(ProcedureDef {
        name,
        inputs,
        outputs,
        rows,
    });
}

// ─── When ────────────────────────────────────────────────────────────────

/// Run `query` against `db` with the given path pinned. Returns either the
/// result records or the error, plus a flag indicating whether the tx was
/// committed (writes only commit on success; reads always release).
fn run_query_pinned(
    db: &mut Database,
    query: &str,
    params: Option<&HashMap<String, Value>>,
    procedures: &crate::procedures::Registry,
    path: Path,
    is_write: bool,
) -> Result<Vec<crate::cypher::record::NamedRecord>, crate::GraphError> {
    if is_write {
        let tx = db.write_tx().expect("begin_write");
        let outcome = tx.query_with_procedures_path(query, params, procedures, path);
        match &outcome {
            Ok(_) => tx.commit().expect("commit"),
            Err(_) => tx.rollback().expect("rollback"),
        }
        outcome
    } else {
        let tx = db.read_tx().expect("begin_read");
        let outcome = tx.query_with_procedures_path(query, params, procedures, path);
        tx.commit().expect("commit read tx");
        outcome
    }
}

/// Compare two query outcomes (named is the reference, slot the candidate)
/// and panic with a clear diff on divergence. Order-sensitive when
/// `ordered` is true (ORDER BY / LIMIT / SKIP); multiset otherwise.
fn assert_results_equivalent(
    named: &Result<Vec<crate::cypher::record::NamedRecord>, crate::GraphError>,
    slot: &Result<Vec<crate::cypher::record::NamedRecord>, crate::GraphError>,
    query: &str,
    ordered: bool,
    feature: &str,
    scenario: &str,
) {
    use crate::types::Value;
    use std::collections::HashMap as Hm;

    let ctx = || format!("{feature}::{scenario}\nquery: {query}");

    match (named, slot) {
        (Ok(n), Ok(s)) => {
            assert_eq!(
                n.len(),
                s.len(),
                "row count differs (named={} slot={}) [{}]",
                n.len(),
                s.len(),
                ctx()
            );
            if ordered {
                for (i, (a, b)) in n.iter().zip(s.iter()).enumerate() {
                    assert_eq!(
                        a,
                        b,
                        "row {i} differs [{}]\n  named={a:?}\n   slot={b:?}",
                        ctx()
                    );
                }
            } else {
                let mut counts: Hm<Vec<(String, Value)>, isize> = Hm::new();
                for r in n {
                    *counts
                        .entry(
                            r.fields
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        )
                        .or_default() += 1;
                }
                for r in s {
                    *counts
                        .entry(
                            r.fields
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        )
                        .or_default() -= 1;
                }
                let leftover: Vec<_> = counts.into_iter().filter(|(_, c)| *c != 0).collect();
                assert!(
                    leftover.is_empty(),
                    "multiset diff [{}]: {leftover:?}",
                    ctx()
                );
            }
        }
        (Err(en), Err(es)) => {
            // Both paths errored — accept any error pair (TCK Then-step
            // checks the kind on the named result; if both errored, the
            // scenario assertion will still gate on the right kind).
            let _ = (en, es);
        }
        (Ok(n), Err(es)) => panic!(
            "named succeeded ({} rows) but slot errored: {es:?} [{}]",
            n.len(),
            ctx()
        ),
        (Err(en), Ok(s)) => panic!(
            "slot succeeded ({} rows) but named errored: {en:?} [{}]",
            s.len(),
            ctx()
        ),
    }
}

fn is_write_query(query: &str) -> bool {
    let upper = query.to_uppercase();
    upper.contains("CREATE")
        || upper.contains("DELETE")
        || upper.contains("SET ")
        || upper.contains("MERGE")
        || upper.contains("REMOVE")
}

fn query_is_ordered(query: &str) -> bool {
    let upper = query.to_uppercase();
    upper.contains("ORDER BY") || upper.contains(" LIMIT ") || upper.contains(" SKIP ")
}

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

    let is_write = is_write_query(query);
    let dual = world.db_slot.is_some();
    // Pin the primary to Named when dual mode is active so the comparison
    // is deterministic regardless of the active feature flag. Outside dual
    // mode keep `Path::Default` so the existing TCK runs still exercise
    // whichever path the feature flag selects.
    let primary_path = if dual { Path::Named } else { Path::Default };

    // Snapshot the planner's `_anon_*` counter so the shadow run reuses the
    // same generated column names as the primary. Otherwise running the
    // same query twice consumes the counter twice and produces identical
    // records under different `_anon_rel_N` keys, which the multiset
    // comparison would flag as divergence.
    let anon_pre = if dual {
        Some(crate::cypher::planner::anon_counter_snapshot())
    } else {
        None
    };

    let primary = run_query_pinned(
        db,
        query,
        params.as_ref(),
        &world.procedures,
        primary_path,
        is_write,
    );

    if let Some(shadow) = world.db_slot.as_mut() {
        if let Some(v) = anon_pre {
            crate::cypher::planner::anon_counter_restore(v);
        }
        let shadow_outcome = run_query_pinned(
            shadow,
            query,
            params.as_ref(),
            &world.procedures,
            Path::Slot,
            is_write,
        );
        assert_results_equivalent(
            &primary,
            &shadow_outcome,
            query,
            query_is_ordered(query),
            &world.current_feature,
            &world.current_scenario,
        );
    }

    match primary {
        Ok(records) => {
            crate::tck_support::headers::log_headers(
                &world.current_feature,
                &world.current_scenario,
                &records,
            );
            world.last_result = Some(records);
            world.last_error = None;
        }
        Err(e) => {
            world.last_result = None;
            world.last_error = Some(e);
        }
    }
}

#[when("executing control query:")]
fn when_executing_control_query(world: &mut World, step: &Step) {
    let query = step
        .docstring
        .as_ref()
        .expect("executing control query requires a docstring")
        .trim();

    let db = world.db.as_mut().expect("database not initialized");

    let dual = world.db_slot.is_some();
    let primary_path = if dual { Path::Named } else { Path::Default };

    let anon_pre = if dual {
        Some(crate::cypher::planner::anon_counter_snapshot())
    } else {
        None
    };

    let primary = run_query_pinned(db, query, None, &world.procedures, primary_path, false);

    if let Some(shadow) = world.db_slot.as_mut() {
        if let Some(v) = anon_pre {
            crate::cypher::planner::anon_counter_restore(v);
        }
        let shadow_outcome =
            run_query_pinned(shadow, query, None, &world.procedures, Path::Slot, false);
        assert_results_equivalent(
            &primary,
            &shadow_outcome,
            query,
            query_is_ordered(query),
            &world.current_feature,
            &world.current_scenario,
        );
    }

    match primary {
        Ok(records) => {
            world.last_result = Some(records);
            world.last_error = None;
        }
        Err(e) => {
            world.last_result = None;
            world.last_error = Some(e);
        }
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

#[then("the result should be (ignoring element order for lists):")]
fn then_result_any_order_ignore_list_order(world: &mut World, step: &Step) {
    let results = world
        .last_result
        .as_ref()
        .expect("expected results but query returned an error");
    let table = step.table.as_ref().expect("expected a result table");
    let (columns, expected_rows) = parse_result_table(table);
    compare::compare_result_ignore_list_order(results, &columns, &expected_rows, false)
        .expect("result comparison failed");
}

#[then("the result should be, in order (ignoring element order for lists):")]
fn then_result_in_order_ignore_list_order(world: &mut World, step: &Step) {
    let results = world
        .last_result
        .as_ref()
        .expect("expected results but query returned an error");
    let table = step.table.as_ref().expect("expected a result table");
    let (columns, expected_rows) = parse_result_table(table);
    compare::compare_result_ignore_list_order(results, &columns, &expected_rows, true)
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

/// Parse a procedure signature from a TCK step string.
///
/// Format: `proc.name(in1 :: TYPE?, in2 :: TYPE?) :: (out1 :: TYPE?, out2 :: TYPE?)`
fn parse_procedure_signature(sig: &str) -> (String, Vec<ProcParam>, Vec<ProcParam>) {
    // Split on `) :: (` to separate input params from output params,
    // since individual params also contain ` :: `.
    let (left, right) = if let Some(pos) = sig.find(") :: (") {
        (&sig[..pos + 1], sig[pos + 4..].trim()) // +1 to include `)`, +4 to skip ` :: `
    } else {
        (sig.trim_end(), "()")
    };
    let left = left.trim();

    let (name, inputs) = if let Some(paren_pos) = left.find('(') {
        let name = left[..paren_pos].trim().to_string();
        let close = left.rfind(')').unwrap_or(left.len());
        let params_str = &left[paren_pos + 1..close];
        (name, parse_params(params_str))
    } else {
        (left.to_string(), Vec::new())
    };

    let outputs = if let Some(start) = right.find('(') {
        let end = right.rfind(')').unwrap_or(right.len());
        parse_params(&right[start + 1..end])
    } else {
        Vec::new()
    };

    (name, inputs, outputs)
}

fn parse_params(s: &str) -> Vec<ProcParam> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(',')
        .map(|p| {
            let parts: Vec<&str> = p.trim().splitn(2, "::").collect();
            ProcParam {
                name: parts[0].trim().to_string(),
                type_name: if parts.len() > 1 {
                    parts[1].trim().to_string()
                } else {
                    "ANY?".to_string()
                },
            }
        })
        .collect()
}
