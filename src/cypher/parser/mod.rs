use std::collections::HashMap;

use pest::Parser;
use pest_derive::Parser;

use crate::cypher::ast::*;
use crate::types::{ErrorCode, GraphError, QueryError, QueryPhase, Span};

#[derive(Parser)]
#[grammar = "cypher/grammar.pest"]
pub(in crate::cypher::parser) struct CypherParser;

/// Strip backticks from a delimited identifier.
pub(in crate::cypher::parser) fn strip_backticks(s: &str) -> &str {
    s.strip_prefix('`')
        .and_then(|s| s.strip_suffix('`'))
        .unwrap_or(s)
}

/// Process backslash escape sequences in a string literal.
pub(in crate::cypher::parser) fn unescape_string(raw: &str) -> crate::types::Result<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{0008}'), // backspace
                Some('f') => out.push('\u{000C}'), // form feed
                Some('\\') => out.push('\\'),
                Some('\'') => out.push('\''),
                Some('"') => out.push('"'),
                Some('/') => out.push('/'),
                Some('u') => {
                    // \uXXXX unicode escape
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() == 4 {
                        if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                            if let Some(c) = char::from_u32(cp) {
                                out.push(c);
                            } else {
                                return Err(GraphError::query(
                                    QueryPhase::Parse,
                                    ErrorCode::InvalidUnicodeLiteral,
                                    format!("InvalidUnicodeLiteral: invalid code point \\u{hex}"),
                                ));
                            }
                        } else {
                            return Err(GraphError::query(
                                QueryPhase::Parse,
                                ErrorCode::InvalidUnicodeLiteral,
                                format!("InvalidUnicodeLiteral: \\u{hex}"),
                            ));
                        }
                    } else {
                        return Err(GraphError::query(
                            QueryPhase::Parse,
                            ErrorCode::InvalidUnicodeLiteral,
                            format!("InvalidUnicodeLiteral: \\u{hex}"),
                        ));
                    }
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

/// Convert a pest pair span into our lightweight `Span`.
pub(in crate::cypher::parser) fn span_from_pair(pair: &pest::iterators::Pair<Rule>) -> Span {
    let s = pair.as_span();
    let (line, col) = s.start_pos().line_col();
    Span {
        start: s.start(),
        end: s.end(),
        line: line as u32,
        col: col as u32,
    }
}

/// Combine two spans into one covering both — uses the earlier `start`
/// (with its line/col) and the later `end`. If either is synthetic, returns
/// the other; if both are synthetic, returns synthetic.
pub(in crate::cypher::parser) fn combine_spans(a: Span, b: Span) -> Span {
    if a.is_synthetic() {
        return b;
    }
    if b.is_synthetic() {
        return a;
    }
    let (lead, tail) = if a.start <= b.start { (a, b) } else { (b, a) };
    Span {
        start: lead.start,
        end: tail.end.max(lead.end),
        line: lead.line,
        col: lead.col,
    }
}

/// Convert a pest error position into our lightweight `Span`.
pub(in crate::cypher::parser) fn pest_span(e: &pest::error::Error<Rule>) -> Span {
    use pest::error::{InputLocation, LineColLocation};
    let (start, end) = match e.location {
        InputLocation::Pos(p) => (p, p),
        InputLocation::Span((s, e)) => (s, e),
    };
    let (line, col) = match e.line_col {
        LineColLocation::Pos((l, c)) => (l, c),
        LineColLocation::Span((l, c), _) => (l, c),
    };
    Span {
        start,
        end,
        line: line as u32,
        col: col as u32,
    }
}

/// Maximum byte length of a single Cypher query accepted by the parser.
/// Defends against memory/stack exhaustion from pathologically large inputs.
pub const MAX_QUERY_BYTES: usize = 1 << 20; // 1 MiB

/// Maximum nesting depth for parsed expressions (recursion guard for
/// `parse_expr`, which descends through parens). 256 comfortably handles any
/// human-written query while preventing thread-stack overflow on adversarial
/// inputs like `(((((...)))))`.
pub const MAX_EXPR_DEPTH: usize = 256;

thread_local! {
    static EXPR_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// RAII guard that increments the thread-local expression-depth counter on
/// construction and decrements it on drop. Returns an error if the cap is
/// exceeded.
pub(in crate::cypher::parser) struct DepthGuard;

impl DepthGuard {
    pub(in crate::cypher::parser) fn enter() -> crate::types::Result<Self> {
        let depth = EXPR_DEPTH.with(|c| {
            let d = c.get() + 1;
            c.set(d);
            d
        });
        if depth > MAX_EXPR_DEPTH {
            EXPR_DEPTH.with(|c| c.set(c.get() - 1));
            return Err(GraphError::SizeLimit {
                what: "expression nesting depth".to_string(),
                limit: MAX_EXPR_DEPTH,
                actual: depth,
                hint: Some("simplify the query or split it into multiple statements".to_string()),
            });
        }
        Ok(DepthGuard)
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        EXPR_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Pre-scan the raw input for the maximum nesting depth of `(`, `[`, `{`
/// brackets (string literals and `//` line comments excluded). This guards
/// against pest's own recursive-descent stack overflow on adversarial inputs
/// like `RETURN ((((...))))` — pest would otherwise blow the stack before our
/// `parse_expr` depth guard could run.
/// Compute (line, column) for a byte offset. 1-indexed; column counts UTF-8 bytes.
pub(in crate::cypher::parser) fn line_col_at(input: &str, byte_pos: usize) -> (u32, u32) {
    let mut line: u32 = 1;
    let mut col: u32 = 1;
    for (i, b) in input.as_bytes().iter().enumerate() {
        if i >= byte_pos {
            break;
        }
        if *b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Detect an unclosed `'…` or `"…` string literal so the user gets a precise
/// diagnostic instead of pest's generic "expected …" list (which can't tell us
/// the token didn't close).
///
/// Walks the input byte-by-byte, skips over `//` line comments, and treats
/// `\X` inside a string as a two-byte escape. If a string opens and the input
/// ends before the matching quote, returns a `SyntaxError` whose span points
/// at the opening quote.
pub(in crate::cypher::parser) fn check_unclosed_string(input: &str) -> crate::types::Result<()> {
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => {
                let quote = b;
                let open_pos = i;
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i >= bytes.len() {
                    let (line, col) = line_col_at(input, open_pos);
                    let kind = if quote == b'\'' { "single" } else { "double" };
                    return Err(GraphError::Query(QueryError::SyntaxError {
                        phase: QueryPhase::Parse,
                        code: ErrorCode::UnexpectedSyntax,
                        message: format!(
                            "unclosed {kind}-quoted string literal — expected `{}` before end of input",
                            quote as char
                        ),
                        hint: Some(format!(
                            "add a closing `{}` to terminate the string",
                            quote as char
                        )),
                        span: Some(Span {
                            start: open_pos,
                            end: open_pos + 1,
                            line,
                            col,
                        }),
                    }));
                }
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

pub(in crate::cypher::parser) fn check_bracket_depth(input: &str) -> crate::types::Result<()> {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut depth: usize = 0;
    let mut max_depth: usize = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => {
                let quote = b;
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
                if max_depth > MAX_EXPR_DEPTH {
                    return Err(GraphError::SizeLimit {
                        what: "bracket nesting depth".to_string(),
                        limit: MAX_EXPR_DEPTH,
                        actual: max_depth,
                        hint: Some(
                            "simplify the query or split it into multiple statements".to_string(),
                        ),
                    });
                }
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// Parse a Cypher query string into a Statement AST.
pub fn parse(input: &str) -> crate::types::Result<Statement> {
    if input.len() > MAX_QUERY_BYTES {
        return Err(GraphError::SizeLimit {
            what: "Cypher query".to_string(),
            limit: MAX_QUERY_BYTES,
            actual: input.len(),
            hint: Some("split the query into smaller statements".to_string()),
        });
    }
    check_unclosed_string(input)?;
    check_bracket_depth(input)?;
    // Reset depth counter in case a prior parse on this thread aborted mid-recursion.
    EXPR_DEPTH.with(|c| c.set(0));
    let pairs = CypherParser::parse(Rule::statement, input).map_err(|e| {
        let span = pest_span(&e);
        GraphError::Query(QueryError::SyntaxError {
            phase: QueryPhase::Parse,
            code: ErrorCode::UnexpectedSyntax,
            message: humanize_pest_error(e),
            hint: None,
            span: Some(span),
        })
    })?;

    let union_pair = pairs
        .into_iter()
        .next()
        .unwrap()
        .into_inner()
        .find(|p| p.as_rule() == Rule::union_stmt)
        .ok_or_else(|| GraphError::syntax("empty statement".to_string()))?;

    parse_union_stmt(union_pair)
}

mod clause;
mod expr;
mod pattern;
mod resolve;
mod statement;

pub use resolve::validate_params;

use resolve::humanize_pest_error;
use statement::parse_union_stmt;

#[cfg(test)]
mod parse_tests {
    use super::parse;
    use crate::cypher::ast::*;
    use crate::types::{GraphError, QueryError, QueryPhase};

    /// A malformed query raises a structured `QueryError::SyntaxError` at the
    /// `Parse` phase — not a stringly-typed `GraphError::ParseError(String)`.
    #[test]
    fn parse_error_is_structured_syntax_error_at_parse_phase() {
        let err = parse("MATCH (n) RETURN n.").expect_err("expected parse error");
        match err {
            GraphError::Query(QueryError::SyntaxError { phase, message, .. }) => {
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
                assert_eq!(pat.elements.len(), 3);
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
                match m.where_clause.unwrap().kind {
                    ExprKind::BinaryOp { left, op, right } => {
                        assert_eq!(op, BinOp::Eq);
                        assert!(matches!(left.kind, ExprKind::Property(_, _)));
                        assert!(matches!(
                            right.kind,
                            ExprKind::Literal(LiteralValue::I64(30))
                        ));
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
                assert!(matches!(
                    m.limit.as_ref().map(|e| &e.kind),
                    Some(ExprKind::Literal(LiteralValue::I64(10)))
                ));
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
                match &item.expr.kind {
                    ExprKind::FunctionCall { name, args, .. } => {
                        assert_eq!(name, "count");
                        assert_eq!(args.len(), 1);
                        assert!(matches!(args[0].kind, ExprKind::Star));
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
                assert_eq!(d.exprs.len(), 1);
                assert!(matches!(
                    &d.exprs[0].kind,
                    ExprKind::Variable(v) if v == "n"
                ));
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
                    SetItem::Property(a) => {
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
            Statement::Match(m) => match m.where_clause.unwrap().kind {
                ExprKind::BinaryOp { op, .. } => assert_eq!(op, BinOp::And),
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
            Statement::Match(m) => match m.where_clause.unwrap().kind {
                ExprKind::BinaryOp { right, .. } => {
                    assert!(matches!(
                        right.kind,
                        ExprKind::Literal(LiteralValue::String(ref s)) if s == "Alice"
                    ));
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
        let stmt = parse("MATCH (x:X), (y:Y) CREATE (x)-[:R]->(y)").unwrap();
        assert!(matches!(stmt, Statement::MultiClause(_)));
    }

    #[test]
    fn parse_create_merge_uses_multi_clause() {
        let stmt = parse("CREATE (a), (b) MERGE (a)-[:X]->(b) RETURN count(a)").unwrap();
        assert!(matches!(stmt, Statement::MultiClause(_)));
    }

    #[test]
    fn parse_merge_merge_merge_uses_multi_clause() {
        let stmt = parse("MERGE (a:A) MERGE (b:B) MERGE (a)-[:FOO]->(b)").unwrap();
        assert!(matches!(stmt, Statement::MultiClause(_)));
    }

    #[test]
    fn parse_match_create_with_create_uses_multi_clause() {
        let stmt = parse("MATCH () CREATE () WITH * CREATE ()").unwrap();
        assert!(matches!(stmt, Statement::MultiClause(_)));
    }

    #[test]
    fn parses_regex_match_operator() {
        let stmt = parse("MATCH (n) WHERE n.name =~ 'h.*' RETURN n").unwrap();
        let dbg = format!("{stmt:?}");
        assert!(
            dbg.contains("RegexMatch"),
            "parsed AST missing RegexMatch: {dbg}"
        );
    }
}
