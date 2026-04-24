use graphdblite::cypher::ast::*;
use graphdblite::cypher::parser::parse;
use graphdblite::{GraphError, QueryError, QueryPhase};

/// Phase 2 acceptance: a malformed query raises a structured
/// `QueryError::SyntaxError` at the `Parse` phase — not a stringly-typed
/// `GraphError::ParseError(String)`.
#[test]
fn parse_error_is_structured_syntax_error_at_parse_phase() {
    let err = parse("MATCH (n) RETURN n.").expect_err("expected parse error");
    match err {
        GraphError::Query(QueryError::SyntaxError { phase, message }) => {
            assert_eq!(phase, QueryPhase::Parse);
            assert!(!message.is_empty());
        }
        other => panic!("expected QueryError::SyntaxError at Parse phase, got {other:?}"),
    }
}

#[test]
fn parse_simple_match() {
    let stmt = parse("MATCH (n:Person) RETURN n").unwrap();
    match stmt {
        Statement::Match(m) => {
            assert_eq!(m.patterns.len(), 1);
            let pat = &m.patterns[0];
            assert_eq!(pat.elements.len(), 1);
            match &pat.elements[0] {
                PatternElement::Node(n) => {
                    assert_eq!(n.variable.as_deref(), Some("n"));
                    assert_eq!(n.labels.first().map(|s| s.as_str()), Some("Person"));
                }
                _ => panic!("expected node pattern"),
            }
            assert_eq!(m.return_clause.items.len(), 1);
        }
        _ => panic!("expected Match statement"),
    }
}

#[test]
fn parse_match_with_relationship() {
    let stmt = parse("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b").unwrap();
    match stmt {
        Statement::Match(m) => {
            let pat = &m.patterns[0];
            assert_eq!(pat.elements.len(), 3); // node, rel, node
            match &pat.elements[1] {
                PatternElement::Relationship(r) => {
                    assert_eq!(r.rel_types.first().map(|s| s.as_str()), Some("KNOWS"));
                    assert_eq!(r.direction, RelDirection::Outgoing);
                }
                _ => panic!("expected relationship"),
            }
        }
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_match_with_where() {
    let stmt = parse("MATCH (n:Person) WHERE n.age = 30 RETURN n").unwrap();
    match stmt {
        Statement::Match(m) => {
            assert!(m.where_clause.is_some());
            match m.where_clause.unwrap() {
                Expr::BinaryOp { left, op, right } => {
                    assert_eq!(op, BinOp::Eq);
                    assert!(matches!(*left, Expr::Property(_, _)));
                    assert!(matches!(*right, Expr::Literal(LiteralValue::I64(30))));
                }
                _ => panic!("expected binary op"),
            }
        }
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_match_with_variable_length_path() {
    let stmt = parse("MATCH (a)-[:KNOWS*1..3]->(b) RETURN b").unwrap();
    match stmt {
        Statement::Match(m) => {
            let rel = &m.patterns[0].elements[1];
            match rel {
                PatternElement::Relationship(r) => {
                    assert_eq!(r.var_length, Some((1, 3)));
                }
                _ => panic!("expected relationship"),
            }
        }
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_match_with_order_by_and_limit() {
    let stmt = parse("MATCH (n:Person) RETURN n.name ORDER BY n.name DESC LIMIT 10").unwrap();
    match stmt {
        Statement::Match(m) => {
            assert_eq!(m.order_by.len(), 1);
            assert!(m.order_by[0].descending);
            assert_eq!(m.limit, Some(Expr::Literal(LiteralValue::I64(10))));
        }
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_match_with_count() {
    let stmt = parse("MATCH (n:Person) RETURN count(*)").unwrap();
    match stmt {
        Statement::Match(m) => {
            let item = &m.return_clause.items[0];
            match &item.expr {
                Expr::FunctionCall { name, args, .. } => {
                    assert_eq!(name, "count");
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0], Expr::Star));
                }
                _ => panic!("expected function call"),
            }
        }
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_match_with_alias() {
    let stmt = parse("MATCH (n:Person) RETURN count(*) AS cnt").unwrap();
    match stmt {
        Statement::Match(m) => {
            assert_eq!(m.return_clause.items[0].alias.as_deref(), Some("cnt"));
        }
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_create_node() {
    let stmt = parse("CREATE (n:Person {name: 'Alice', age: 30})").unwrap();
    match stmt {
        Statement::Create(c) => {
            assert_eq!(c.patterns.len(), 1);
            match &c.patterns[0].elements[0] {
                PatternElement::Node(n) => {
                    assert_eq!(n.variable.as_deref(), Some("n"));
                    assert_eq!(n.labels.first().map(|s| s.as_str()), Some("Person"));
                    assert_eq!(n.properties.len(), 2);
                }
                _ => panic!("expected node"),
            }
        }
        _ => panic!("expected Create"),
    }
}

#[test]
fn parse_create_edge() {
    let stmt =
        parse("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})").unwrap();
    match stmt {
        Statement::Create(c) => {
            assert_eq!(c.patterns[0].elements.len(), 3);
        }
        _ => panic!("expected Create"),
    }
}

#[test]
fn parse_delete() {
    let stmt = parse("MATCH (n:Person) WHERE n.name = 'Alice' DELETE n").unwrap();
    match stmt {
        Statement::Delete(d) => {
            assert!(d.where_clause.is_some());
            assert_eq!(d.variables, vec!["n"]);
        }
        _ => panic!("expected Delete"),
    }
}

#[test]
fn parse_set() {
    let stmt = parse("MATCH (n:Person) WHERE n.name = 'Alice' SET n.age = 31").unwrap();
    match stmt {
        Statement::Set(s) => {
            assert_eq!(s.items.len(), 1);
            match &s.items[0] {
                graphdblite::cypher::ast::SetItem::Property(a) => {
                    assert_eq!(a.variable, "n");
                    assert_eq!(a.property, "age");
                }
                _ => panic!("expected Property set item"),
            }
        }
        _ => panic!("expected Set"),
    }
}

#[test]
fn parse_merge() {
    let stmt = parse(
        "MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.seen = true",
    )
    .unwrap();
    match stmt {
        Statement::Merge(m) => {
            assert_eq!(m.on_create.len(), 1);
            assert_eq!(m.on_match.len(), 1);
        }
        _ => panic!("expected Merge"),
    }
}

#[test]
fn parse_boolean_logic() {
    let stmt = parse("MATCH (n:Person) WHERE n.age > 20 AND n.age < 40 RETURN n").unwrap();
    match stmt {
        Statement::Match(m) => match m.where_clause.unwrap() {
            Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::And),
            _ => panic!("expected AND"),
        },
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_incoming_relationship() {
    let stmt = parse("MATCH (a:Person)<-[:KNOWS]-(b:Person) RETURN a").unwrap();
    match stmt {
        Statement::Match(m) => match &m.patterns[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.direction, RelDirection::Incoming);
            }
            _ => panic!("expected relationship"),
        },
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_string_literal() {
    let stmt = parse("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n").unwrap();
    match stmt {
        Statement::Match(m) => match m.where_clause.unwrap() {
            Expr::BinaryOp { right, .. } => {
                assert!(
                    matches!(*right, Expr::Literal(LiteralValue::String(ref s)) if s == "Alice")
                );
            }
            _ => panic!("expected comparison"),
        },
        _ => panic!("expected Match"),
    }
}

#[test]
fn parse_error_on_invalid_input() {
    let result = parse("BANANA SPLIT");
    assert!(result.is_err());
}

#[test]
fn parse_match_create_is_multi_clause() {
    // MATCH...CREATE now falls through to multi_clause_stmt (more general).
    let stmt = parse("MATCH (x:X), (y:Y) CREATE (x)-[:R]->(y)").unwrap();
    assert!(
        matches!(stmt, Statement::MultiClause(_)),
        "expected MultiClause, got {:?}",
        std::mem::discriminant(&stmt)
    );
}

#[test]
fn parse_create_merge_uses_multi_clause() {
    let result = parse("CREATE (a), (b) MERGE (a)-[:X]->(b) RETURN count(a)");
    match result {
        Ok(stmt) => assert!(
            matches!(stmt, Statement::MultiClause(_)),
            "expected MultiClause, got {:?}",
            std::mem::discriminant(&stmt)
        ),
        Err(e) => panic!("parse failed: {e}"),
    }
}

#[test]
fn parse_merge_merge_merge_uses_multi_clause() {
    let stmt = parse("MERGE (a:A) MERGE (b:B) MERGE (a)-[:FOO]->(b)").unwrap();
    assert!(
        matches!(stmt, Statement::MultiClause(_)),
        "expected MultiClause, got {:?}",
        std::mem::discriminant(&stmt)
    );
}

#[test]
fn parse_match_create_with_create_uses_multi_clause() {
    let stmt = parse("MATCH () CREATE () WITH * CREATE ()").unwrap();
    assert!(
        matches!(stmt, Statement::MultiClause(_)),
        "expected MultiClause, got {:?}",
        std::mem::discriminant(&stmt)
    );
}
