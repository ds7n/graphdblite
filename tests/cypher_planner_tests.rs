use graphdblite::cypher::ir::*;
use graphdblite::cypher::parser::parse;
use graphdblite::cypher::planner::plan;
use graphdblite::types::Direction;

fn plan_query(q: &str) -> LogicalOp {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let stmt = parse(q).unwrap();
    plan(&conn, &stmt).unwrap()
}

#[test]
fn plan_simple_scan() {
    let op = plan_query("MATCH (n:Person) RETURN n");
    // Should be: Limit? -> Sort? -> Project -> Scan
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
    // Plan is: Project → Filter(b.__label = "Person") → Expand → Scan
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
    // Project -> Aggregate -> Scan
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
    // Pipeline: Limit → Project → Sort (sort before projection so ORDER BY
    // can reference pre-projection variables).
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
            assert_eq!(ops.len(), 3); // CreateNode, CreateNode, CreateEdge
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
                graphdblite::cypher::ast::ExprKind::Variable(v) if v == "n"
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
