use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::ir::*;
use crate::index;
use crate::types::Value;

/// Global counter for unique anonymous variable aliases across all plan_single_pattern calls.
static ANON_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Suggest the closest in-scope name for a misspelled identifier.
///
/// Returns the closest candidate within Levenshtein distance ≤ 2 (or ≤ 1
/// for short names), ignoring case. Returns `None` if nothing close enough
/// is found — callers should not blindly attach a suggestion that's only
/// vaguely similar.
pub fn plan(conn: &Connection, stmt: &Statement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_inner(conn, stmt, false)?;
    push_limit_into_var_length_expand(&mut op);
    Ok(op)
}

/// Optimization: push a `LIMIT N` cap down into a var-length `Expand` when the
/// chain between them is row-preserving.
///
/// Safe pattern: `Limit { count: N, input: chain }` where `chain` is zero or
/// more `Project` (always 1:1) wrapping a single `Expand { var_length: true }`.
/// Anything else (Sort, Distinct, Filter, Aggregate, CrossProduct, another
/// Expand) breaks the equivalence — `LIMIT` and Expand row counts diverge.
///
/// Recurses into all child operators so nested patterns are still optimized.
pub(in crate::cypher::planner) fn push_limit_into_var_length_expand(op: &mut LogicalOp) {
    if let LogicalOp::Limit { input, count } = op {
        if let Some(LogicalOp::Expand { result_cap, .. }) = find_pushdown_target(input) {
            // Take the tighter of any existing cap and the new one.
            let new_cap = match *result_cap {
                Some(existing) => existing.min(*count),
                None => *count,
            };
            *result_cap = Some(new_cap);
        }
    }
    // Recurse into children so nested Limit/Expand chains (e.g. inside a
    // CorrelatedJoin's right side) also get the pushdown.
    walk_children_mut(op, push_limit_into_var_length_expand);
}

/// Returns a mutable reference to the var-length Expand directly reachable from
/// `op` through only Project (or empty) wrappers, or `None` if any disqualifying
/// operator is in the chain.
pub(in crate::cypher::planner) fn find_pushdown_target(
    op: &mut LogicalOp,
) -> Option<&mut LogicalOp> {
    match op {
        LogicalOp::Project { input, .. } => find_pushdown_target(input),
        LogicalOp::Expand { var_length, .. } if *var_length => Some(op),
        _ => None,
    }
}

/// Apply `f` to each direct child operator (skipping non-LogicalOp fields).
pub(in crate::cypher::planner) fn walk_children_mut(op: &mut LogicalOp, f: fn(&mut LogicalOp)) {
    match op {
        LogicalOp::Expand { input, .. }
        | LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Aggregate { input, .. }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Distinct { input }
        | LogicalOp::Skip { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::MatchCreate { input, .. }
        | LogicalOp::Delete { input, .. }
        | LogicalOp::SetProperty { input, .. }
        | LogicalOp::SetLabel { input, .. }
        | LogicalOp::SetProperties { input, .. }
        | LogicalOp::Remove { input, .. }
        | LogicalOp::MatchMerge { input, .. }
        | LogicalOp::MaterializePath { input, .. }
        | LogicalOp::Unwind { input, .. }
        | LogicalOp::Call { input, .. }
        | LogicalOp::ShortestPath { input, .. } => f(input),
        LogicalOp::CrossProduct { left, right, .. } => {
            f(left);
            f(right);
        }
        LogicalOp::CorrelatedJoin { input, right, .. }
        | LogicalOp::LeftOuterJoin { input, right, .. } => {
            f(input);
            f(right);
        }
        LogicalOp::Union { inputs, .. } => {
            for inp in inputs {
                f(inp);
            }
        }
        LogicalOp::CreateSequence { ops } => {
            for inner in ops {
                f(inner);
            }
        }
        LogicalOp::SingleRow
        | LogicalOp::Scan { .. }
        | LogicalOp::IndexLookup { .. }
        | LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::Merge { .. }
        | LogicalOp::EmptyRow => {}
    }
}

/// Plan a statement that may be inside a subquery (EXISTS).
/// Subquery context disables certain validations that require full scope.
pub fn plan_subquery(conn: &Connection, stmt: &Statement) -> crate::types::Result<LogicalOp> {
    plan_inner(conn, stmt, true)
}

/// Plan a statement with a procedure registry for CALL validation.
pub fn plan_with_procedures(
    conn: &Connection,
    stmt: &Statement,
    procedures: &crate::cypher::procedure::ProcedureRegistry,
    params: Option<&std::collections::HashMap<String, Value>>,
) -> crate::types::Result<LogicalOp> {
    match stmt {
        Statement::Call {
            procedure_name,
            args,
            implicit_args,
            yield_items,
            yield_star,
            return_clause,
            order_by,
            skip,
            limit,
        } => plan_call(
            conn,
            procedure_name,
            args,
            *implicit_args,
            yield_items.as_deref(),
            *yield_star,
            return_clause.as_ref(),
            order_by,
            skip.as_deref(),
            limit.as_deref(),
            procedures,
            params,
        ),
        Statement::Explain(inner) => plan_with_procedures(conn, inner, procedures, params),
        _ => plan(conn, stmt),
    }
    .map(|mut op| {
        push_limit_into_var_length_expand(&mut op);
        op
    })
}

#[cfg(test)]
mod limit_pushdown_tests {
    use super::*;
    use crate::cypher::parser;
    use rusqlite::Connection;

    fn plan_query(query: &str) -> LogicalOp {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        let stmt = parser::parse(query).unwrap();
        plan(&conn, &stmt).unwrap()
    }

    fn find_var_length_expand(op: &LogicalOp) -> Option<&LogicalOp> {
        match op {
            LogicalOp::Expand {
                var_length: true, ..
            } => Some(op),
            LogicalOp::Limit { input, .. }
            | LogicalOp::Project { input, .. }
            | LogicalOp::Filter { input, .. }
            | LogicalOp::Sort { input, .. }
            | LogicalOp::Distinct { input } => find_var_length_expand(input),
            _ => None,
        }
    }

    #[test]
    fn pushdown_applies_to_simple_var_length_with_limit() {
        let plan = plan_query("MATCH (a)-[*1..3]->(b) RETURN b LIMIT 10");
        let expand = find_var_length_expand(&plan).expect("expected var-length Expand");
        let LogicalOp::Expand { result_cap, .. } = expand else {
            unreachable!()
        };
        assert_eq!(*result_cap, Some(10));
    }

    #[test]
    fn pushdown_skipped_when_sort_intervenes() {
        let plan = plan_query("MATCH (a)-[*1..3]->(b) RETURN b ORDER BY b LIMIT 10");
        let expand = find_var_length_expand(&plan).expect("expected var-length Expand");
        let LogicalOp::Expand { result_cap, .. } = expand else {
            unreachable!()
        };
        assert_eq!(
            *result_cap, None,
            "Sort between Limit and Expand should block pushdown"
        );
    }

    #[test]
    fn pushdown_skipped_for_fixed_length_expand() {
        let plan = plan_query("MATCH (a)-[r]->(b) RETURN b LIMIT 10");
        // Fixed-length Expand is not a pushdown target; just verify no crash.
        match &plan {
            LogicalOp::Limit { input, count } => {
                assert_eq!(*count, 10);
                // Walk down — any Expand we find should have result_cap=None.
                fn check(op: &LogicalOp) {
                    if let LogicalOp::Expand { result_cap, .. } = op {
                        assert_eq!(*result_cap, None);
                    }
                    match op {
                        LogicalOp::Project { input, .. }
                        | LogicalOp::Expand { input, .. }
                        | LogicalOp::Filter { input, .. } => check(input),
                        _ => {}
                    }
                }
                check(input);
            }
            _ => panic!("expected Limit at top, got {:?}", plan.op_name()),
        }
    }
}

#[cfg(test)]
mod plan_tests {
    use super::plan;
    use crate::cypher::ir::*;
    use crate::cypher::parser::parse;
    use crate::types::Direction;
    use rusqlite::Connection;

    fn plan_query(q: &str) -> LogicalOp {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        let stmt = parse(q).unwrap();
        plan(&conn, &stmt).unwrap()
    }

    #[test]
    fn plan_simple_scan() {
        let op = plan_query("MATCH (n:Person) RETURN n");
        match op {
            LogicalOp::Project { input, items, .. } => {
                assert_eq!(items.len(), 1);
                match *input {
                    LogicalOp::Scan {
                        ref label,
                        ref alias,
                    } => {
                        assert_eq!(label, "Person");
                        assert_eq!(alias, "n");
                    }
                    _ => panic!("expected Scan, got {input:?}"),
                }
            }
            _ => panic!("expected Project, got {op:?}"),
        }
    }

    #[test]
    fn plan_scan_with_expand() {
        let op = plan_query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b");
        match op {
            LogicalOp::Project { input, .. } => match *input {
                LogicalOp::Filter { input, .. } => match *input {
                    LogicalOp::Expand {
                        ref src_alias,
                        ref dst_alias,
                        ref edge_types,
                        direction,
                        min_hops,
                        max_hops,
                        ..
                    } => {
                        assert_eq!(src_alias, "a");
                        assert_eq!(dst_alias, "b");
                        assert_eq!(edge_types.first().map(|s| s.as_str()), Some("KNOWS"));
                        assert_eq!(direction, Direction::Outgoing);
                        assert_eq!(min_hops, 1);
                        assert_eq!(max_hops, 1);
                    }
                    _ => panic!("expected Expand"),
                },
                _ => panic!("expected Filter for destination label"),
            },
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn plan_variable_length_expand() {
        let op = plan_query("MATCH (a)-[:CALLS*1..5]->(b) RETURN b");
        match op {
            LogicalOp::Project { input, .. } => match *input {
                LogicalOp::Expand {
                    min_hops, max_hops, ..
                } => {
                    assert_eq!(min_hops, 1);
                    assert_eq!(max_hops, 5);
                }
                _ => panic!("expected Expand"),
            },
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn plan_with_filter() {
        let op = plan_query("MATCH (n:Person) WHERE n.age = 30 RETURN n");
        match op {
            LogicalOp::Project { input, .. } => match *input {
                LogicalOp::Filter { .. } => {}
                _ => panic!("expected Filter"),
            },
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn plan_with_aggregate() {
        let op = plan_query("MATCH (n:Person) RETURN count(*) AS cnt");
        match op {
            LogicalOp::Project { input, .. } => match *input {
                LogicalOp::Aggregate { ref aggregates, .. } => {
                    assert_eq!(aggregates.len(), 1);
                    assert_eq!(aggregates[0].function, AggregateFunction::Count);
                }
                _ => panic!("expected Aggregate"),
            },
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn plan_with_order_by_and_limit() {
        let op = plan_query("MATCH (n:Person) RETURN n.name ORDER BY n.name LIMIT 5");
        match op {
            LogicalOp::Limit { input, count } => {
                assert_eq!(count, 5);
                match *input {
                    LogicalOp::Project { input, .. } => match *input {
                        LogicalOp::Sort { .. } => {}
                        _ => panic!("expected Sort"),
                    },
                    _ => panic!("expected Project"),
                }
            }
            _ => panic!("expected Limit"),
        }
    }

    #[test]
    fn plan_create_node() {
        let op = plan_query("CREATE (n:Person {name: 'Alice'})");
        match op {
            LogicalOp::CreateNode {
                labels,
                alias,
                properties,
            } => {
                assert_eq!(labels, vec!["Person".to_string()]);
                assert_eq!(alias.as_deref(), Some("n"));
                assert_eq!(properties.len(), 1);
            }
            _ => panic!("expected CreateNode, got {op:?}"),
        }
    }

    #[test]
    fn plan_create_edge() {
        let op = plan_query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})");
        match op {
            LogicalOp::CreateSequence { ref ops } => {
                assert_eq!(ops.len(), 3);
                assert!(matches!(ops[0], LogicalOp::CreateNode { .. }));
                assert!(matches!(ops[1], LogicalOp::CreateNode { .. }));
                assert!(matches!(ops[2], LogicalOp::CreateEdge { .. }));
            }
            _ => panic!("expected CreateSequence, got {op:?}"),
        }
    }

    #[test]
    fn plan_delete() {
        let op = plan_query("MATCH (n:Person) WHERE n.name = 'Alice' DELETE n");
        match op {
            LogicalOp::Delete { exprs, .. } => {
                assert_eq!(exprs.len(), 1);
                assert!(matches!(
                    &exprs[0].kind,
                    crate::cypher::ast::ExprKind::Variable(v) if v == "n"
                ));
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn plan_set_property() {
        let op = plan_query("MATCH (n:Person) WHERE n.name = 'Alice' SET n.age = 31");
        match op {
            LogicalOp::SetProperty { assignments, .. } => {
                assert_eq!(assignments.len(), 1);
                assert_eq!(assignments[0].property, "age");
            }
            _ => panic!("expected SetProperty"),
        }
    }

    #[test]
    fn plan_merge() {
        let op = plan_query(
            "MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.seen = true",
        );
        match op {
            LogicalOp::Merge {
                on_create,
                on_match,
                ..
            } => {
                assert_eq!(on_create.len(), 1);
                assert_eq!(on_match.len(), 1);
            }
            _ => panic!("expected Merge"),
        }
    }
}

// === planner split: submodule declarations ===

mod helpers;
mod multi;
mod pattern;
mod statement;
mod validation;

pub use pattern::plan_patterns;

// Sibling fns called from mod.rs's public planners.
use statement::{plan_call, plan_inner};
