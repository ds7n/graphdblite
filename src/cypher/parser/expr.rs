//! Expression parsing — boolean/comparison/arithmetic chains, functions, literals, list and pattern comprehensions, CASE, EXISTS.

use crate::types::{ErrorCode, GraphError, QueryPhase};

use super::clause::*;
use super::pattern::*;
use super::*;

pub(in crate::cypher::parser) fn parse_bool_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    // bool_expr = { expr }
    parse_expr(pair.into_inner().next().unwrap())
}

pub(in crate::cypher::parser) fn parse_xor_term(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    // xor_term = { bool_term ~ (xor_op ~ bool_term)* }
    let mut children: Vec<pest::iterators::Pair<Rule>> = pair.into_inner().collect();

    if children.len() == 1 {
        return parse_bool_term(children.remove(0));
    }

    let mut left = parse_bool_term(children.remove(0))?;
    let mut i = 0;
    while i < children.len() {
        if children[i].as_rule() == Rule::xor_op {
            i += 1;
            let right = parse_bool_term(children.remove(i))?;
            children.remove(i - 1);
            i -= 1;
            let span = combine_spans(left.span, right.span);
            left = Expr::new(
                ExprKind::BinaryOp {
                    left: Box::new(left),
                    op: BinOp::Xor,
                    right: Box::new(right),
                },
                span,
            );
        } else {
            i += 1;
        }
    }
    Ok(left)
}

pub(in crate::cypher::parser) fn parse_bool_term(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    // bool_term = { bool_factor ~ (and_op ~ bool_factor)* }
    let mut children: Vec<pest::iterators::Pair<Rule>> = pair.into_inner().collect();

    if children.len() == 1 {
        return parse_bool_factor(children.remove(0));
    }

    let mut left = parse_bool_factor(children.remove(0))?;
    let mut i = 0;
    while i < children.len() {
        if children[i].as_rule() == Rule::and_op {
            i += 1;
            let right = parse_bool_factor(children.remove(i))?;
            children.remove(i - 1);
            i -= 1;
            let span = combine_spans(left.span, right.span);
            left = Expr::new(
                ExprKind::BinaryOp {
                    left: Box::new(left),
                    op: BinOp::And,
                    right: Box::new(right),
                },
                span,
            );
        } else {
            i += 1;
        }
    }
    Ok(left)
}

pub(in crate::cypher::parser) fn parse_bool_factor(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    // bool_factor = { not_op* ~ bool_primary }
    let outer_span = span_from_pair(&pair);
    let mut not_count = 0;
    let mut primary = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::not_op => not_count += 1,
            Rule::bool_primary => primary = Some(inner),
            _ => {}
        }
    }

    let mut expr = parse_bool_primary(primary.unwrap())?;
    for _ in 0..not_count {
        expr = Expr::new(ExprKind::Not(Box::new(expr)), outer_span);
    }
    Ok(expr)
}

pub(in crate::cypher::parser) fn parse_bool_primary(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::cmp_or_value => parse_cmp_or_value(inner),
        _ => Err(GraphError::syntax(format!(
            "unexpected bool primary: {:?}",
            inner.as_rule()
        ))),
    }
}

pub(in crate::cypher::parser) fn parse_cmp_or_value(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let mut children: Vec<_> = pair.into_inner().collect();

    // First child is always a predicate_expr.
    let first = parse_predicate_expr(children.remove(0))?;

    if children.is_empty() {
        return Ok(first);
    }

    // Parse comparison suffixes. For chained comparisons like `a < b < c`,
    // desugar into `a < b AND b < c`.
    let mut comparisons = Vec::new();
    let mut prev = first;

    for suffix in children {
        let mut inner = suffix.into_inner();
        let op = parse_comp_op(inner.next().unwrap())?;
        let right = parse_predicate_expr(inner.next().unwrap())?;
        let cmp_span = combine_spans(prev.span, right.span);
        comparisons.push(Expr::new(
            ExprKind::BinaryOp {
                left: Box::new(prev.clone()),
                op,
                right: Box::new(right.clone()),
            },
            cmp_span,
        ));
        prev = right;
    }

    if comparisons.len() == 1 {
        Ok(comparisons.into_iter().next().unwrap())
    } else {
        // Chain with AND: (a < b) AND (b < c) AND ...
        let mut result = comparisons.remove(0);
        for cmp in comparisons {
            let span = combine_spans(result.span, cmp.span);
            result = Expr::new(
                ExprKind::BinaryOp {
                    left: Box::new(result),
                    op: BinOp::And,
                    right: Box::new(cmp),
                },
                span,
            );
        }
        Ok(result)
    }
}

/// Parse a cmp_primary: case_expr | exists_subquery | "(" expr ")" | add_expr.
pub(in crate::cypher::parser) fn parse_cmp_primary(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::case_expr => parse_case_expr(inner),
        Rule::exists_subquery => parse_exists_subquery(inner),
        Rule::exists_full_subquery => parse_exists_full_subquery(inner),
        Rule::pattern_predicate => {
            let span = span_from_pair(&inner);
            let pattern = parse_pattern(inner)?;
            Ok(Expr::new(ExprKind::PatternPredicate(pattern), span))
        }
        Rule::expr => parse_expr(inner),
        Rule::add_expr => parse_add_expr(inner),
        _ => Err(GraphError::syntax(format!(
            "unexpected cmp_primary: {:?}",
            inner.as_rule()
        ))),
    }
}

/// Parse a predicate expression: cmp_primary with optional IS NULL / IS NOT NULL / IN / string predicate suffix.
pub(in crate::cypher::parser) fn parse_predicate_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = span_from_pair(&pair);
    let mut children = pair.into_inner();
    let left = parse_cmp_primary(children.next().unwrap())?;

    match children.next() {
        None => Ok(left),
        Some(suffix) => match suffix.as_rule() {
            Rule::is_not_null_suffix => {
                Ok(Expr::new(ExprKind::IsNotNull(Box::new(left)), outer_span))
            }
            Rule::is_null_suffix => Ok(Expr::new(ExprKind::IsNull(Box::new(left)), outer_span)),
            Rule::in_suffix => {
                let right_pair = suffix
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::add_expr)
                    .unwrap();
                let right = parse_add_expr(right_pair)?;
                let span = combine_spans(left.span, right.span);
                Ok(Expr::new(
                    ExprKind::BinaryOp {
                        left: Box::new(left),
                        op: BinOp::In,
                        right: Box::new(right),
                    },
                    span,
                ))
            }
            Rule::string_pred_suffix => {
                let mut inner = suffix.into_inner();
                let op_pair = inner.next().unwrap();
                let op = parse_string_pred_op(op_pair)?;
                let right = parse_add_expr(inner.next().unwrap())?;
                let span = combine_spans(left.span, right.span);
                Ok(Expr::new(
                    ExprKind::BinaryOp {
                        left: Box::new(left),
                        op,
                        right: Box::new(right),
                    },
                    span,
                ))
            }
            _ => Err(GraphError::syntax(format!(
                "unexpected predicate_expr suffix: {:?}",
                suffix.as_rule()
            ))),
        },
    }
}

/// Parse a string predicate operator (STARTS WITH, ENDS WITH, CONTAINS).
pub(in crate::cypher::parser) fn parse_string_pred_op(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<BinOp> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::starts_with_op => Ok(BinOp::StartsWith),
        Rule::ends_with_op => Ok(BinOp::EndsWith),
        Rule::contains_op => Ok(BinOp::Contains),
        _ => Err(GraphError::syntax(format!(
            "unexpected string pred op: {:?}",
            inner.as_rule()
        ))),
    }
}

pub(in crate::cypher::parser) fn parse_case_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = span_from_pair(&pair);
    let mut operand = None;
    let mut alternatives = Vec::new();
    let mut default = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::case_operand => {
                let expr = parse_expr(inner.into_inner().next().unwrap())?;
                operand = Some(Box::new(expr));
            }
            Rule::case_when_clause => {
                let mut children = inner.into_inner();
                let condition = parse_expr(children.next().unwrap())?;
                let result = parse_expr(children.next().unwrap())?;
                alternatives.push((Box::new(condition), Box::new(result)));
            }
            Rule::case_else_clause => {
                let expr = parse_expr(inner.into_inner().next().unwrap())?;
                default = Some(Box::new(expr));
            }
            _ => {}
        }
    }

    Ok(Expr::new(
        ExprKind::Case {
            operand,
            alternatives,
            default,
        },
        outer_span,
    ))
}

pub(in crate::cypher::parser) fn parse_exists_subquery(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = span_from_pair(&pair);
    let mut patterns = Vec::new();
    let mut where_clause = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::where_clause => where_clause = Some(Box::new(parse_where(inner)?)),
            _ => {}
        }
    }

    if patterns.is_empty() {
        return Err(GraphError::syntax(
            "EXISTS subquery requires at least one pattern".to_string(),
        ));
    }

    Ok(Expr::new(
        ExprKind::Exists {
            patterns,
            where_clause,
        },
        outer_span,
    ))
}

/// Parse EXISTS { MATCH ... [WITH ...] [RETURN ...] } full subquery.
pub(in crate::cypher::parser) fn parse_exists_full_subquery(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = span_from_pair(&pair);
    let mut patterns = Vec::new();
    let mut where_clause = None;
    let mut intermediate_clauses = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;
    let mut first_match = true;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => {
                if first_match {
                    patterns = parse_pattern_list(inner)?;
                    first_match = false;
                } else {
                    // Additional MATCH after WITH
                    let mp_patterns = parse_pattern_list(inner)?;
                    intermediate_clauses.push(IntermediateClause::Match(IntermediateMatch {
                        patterns: mp_patterns,
                        optional_patterns: vec![],
                        where_clause: None,
                    }));
                }
            }
            Rule::where_clause => {
                let w = parse_where(inner)?;
                // If we've already seen intermediate clauses, this WHERE belongs
                // to the last intermediate match.
                if let Some(IntermediateClause::Match(ref mut im)) = intermediate_clauses.last_mut()
                {
                    if im.where_clause.is_none() {
                        im.where_clause = Some(w);
                    } else {
                        where_clause = Some(w);
                    }
                } else {
                    where_clause = Some(w);
                }
            }
            Rule::with_clause => {
                intermediate_clauses.push(IntermediateClause::With(parse_with(inner)?))
            }
            Rule::unwind_clause => {
                intermediate_clauses.push(IntermediateClause::Unwind(parse_unwind_clause(inner)?))
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            Rule::multi_set_clause
            | Rule::multi_delete_clause
            | Rule::multi_remove_clause
            | Rule::multi_create_clause
            | Rule::multi_merge_clause => {
                return Err(GraphError::query(
                    QueryPhase::Parse,
                    ErrorCode::InvalidClauseComposition,
                    "InvalidClauseComposition: EXISTS subquery cannot contain updating clauses",
                ));
            }
            _ => {}
        }
    }

    if patterns.is_empty() {
        return Err(GraphError::syntax(
            "EXISTS subquery requires at least one MATCH pattern".to_string(),
        ));
    }

    // If there's no RETURN clause, synthesize one that returns true (the
    // existence check only cares whether rows are produced, not what).
    let rc = return_clause.unwrap_or_else(|| ReturnClause {
        distinct: false,
        items: vec![ReturnItem {
            expr: Expr::synthetic(ExprKind::Literal(LiteralValue::Bool(true))),
            alias: Some("__exists".to_string()),
        }],
    });

    let stmt = MatchStatement {
        patterns,
        optional_patterns: vec![],
        where_clause,
        intermediate_clauses,
        return_clause: rc,
        order_by,
        skip,
        limit,
    };
    Ok(Expr::new(
        ExprKind::ExistsSubquery(Box::new(Statement::Match(stmt))),
        outer_span,
    ))
}

pub(in crate::cypher::parser) fn parse_comp_op(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<BinOp> {
    let text = pair.as_str().trim();
    // Check for multi-word operators via sub-rules first.
    if let Some(sub) = pair.into_inner().next() {
        return match sub.as_rule() {
            Rule::starts_with_op => Ok(BinOp::StartsWith),
            Rule::ends_with_op => Ok(BinOp::EndsWith),
            Rule::contains_op => Ok(BinOp::Contains),
            _ => Err(GraphError::syntax(format!(
                "unexpected comp_op sub-rule: {:?}",
                sub.as_rule()
            ))),
        };
    }
    // Single-character/two-character operators.
    match text {
        "=" => Ok(BinOp::Eq),
        "<>" => Ok(BinOp::Neq),
        "<=" => Ok(BinOp::Lte),
        ">=" => Ok(BinOp::Gte),
        "<" => Ok(BinOp::Lt),
        ">" => Ok(BinOp::Gt),
        _ => Err(GraphError::syntax(format!(
            "unknown comparison operator: {text}"
        ))),
    }
}

pub(in crate::cypher::parser) fn parse_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let _guard = DepthGuard::enter()?;
    // expr = { xor_term ~ (or_op ~ xor_term)* }
    let mut children: Vec<pest::iterators::Pair<Rule>> = pair.into_inner().collect();

    if children.len() == 1 {
        return parse_xor_term(children.remove(0));
    }

    // Left-associative OR chain.
    let mut left = parse_xor_term(children.remove(0))?;
    let mut i = 0;
    while i < children.len() {
        if children[i].as_rule() == Rule::or_op {
            i += 1;
            let right = parse_xor_term(children.remove(i))?;
            children.remove(i - 1); // remove or_op
            i -= 1;
            let span = combine_spans(left.span, right.span);
            left = Expr::new(
                ExprKind::BinaryOp {
                    left: Box::new(left),
                    op: BinOp::Or,
                    right: Box::new(right),
                },
                span,
            );
        } else {
            i += 1;
        }
    }
    Ok(left)
}

pub(in crate::cypher::parser) fn parse_in_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let mut children: Vec<_> = pair.into_inner().collect();
    if children.len() == 1 {
        return parse_add_expr(children.remove(0));
    }
    // in_expr = { add_expr ~ (IN ~ add_expr)? }
    let left = parse_add_expr(children.remove(0))?;
    let right = parse_add_expr(children.remove(0))?;
    let span = combine_spans(left.span, right.span);
    Ok(Expr::new(
        ExprKind::BinaryOp {
            left: Box::new(left),
            op: BinOp::In,
            right: Box::new(right),
        },
        span,
    ))
}

pub(in crate::cypher::parser) fn parse_add_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let mut children: Vec<_> = pair.into_inner().collect();
    if children.len() == 1 {
        return parse_mul_expr(children.remove(0));
    }
    // Build left-associative: mul_expr (add_op mul_expr)*
    let mut iter = children.into_iter();
    let mut left = parse_mul_expr(iter.next().unwrap())?;
    while let Some(op_pair) = iter.next() {
        let op = match op_pair.as_str() {
            "+" => BinOp::Add,
            "-" => BinOp::Sub,
            _ => {
                return Err(GraphError::syntax(format!(
                    "unexpected add op: {}",
                    op_pair.as_str()
                )))
            }
        };
        let right = parse_mul_expr(iter.next().unwrap())?;
        let span = combine_spans(left.span, right.span);
        left = Expr::new(
            ExprKind::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            },
            span,
        );
    }
    Ok(left)
}

pub(in crate::cypher::parser) fn parse_mul_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let mut children: Vec<_> = pair.into_inner().collect();
    if children.len() == 1 {
        return parse_exp_expr(children.remove(0));
    }
    let mut iter = children.into_iter();
    let mut left = parse_exp_expr(iter.next().unwrap())?;
    while let Some(op_pair) = iter.next() {
        let op = match op_pair.as_str() {
            "*" => BinOp::Mul,
            "/" => BinOp::Div,
            "%" => BinOp::Mod,
            _ => {
                return Err(GraphError::syntax(format!(
                    "unexpected mul op: {}",
                    op_pair.as_str()
                )))
            }
        };
        let right = parse_exp_expr(iter.next().unwrap())?;
        let span = combine_spans(left.span, right.span);
        left = Expr::new(
            ExprKind::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            },
            span,
        );
    }
    Ok(left)
}

/// Parse exponentiation: atom_expr (^ atom_expr)*
pub(in crate::cypher::parser) fn parse_exp_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let mut children: Vec<_> = pair.into_inner().collect();
    if children.len() == 1 {
        return parse_atom_expr(children.remove(0));
    }
    let mut iter = children.into_iter();
    let mut left = parse_atom_expr(iter.next().unwrap())?;
    while let Some(op_pair) = iter.next() {
        if op_pair.as_rule() == Rule::exp_op {
            let right = parse_atom_expr(iter.next().unwrap())?;
            let span = combine_spans(left.span, right.span);
            left = Expr::new(
                ExprKind::BinaryOp {
                    left: Box::new(left),
                    op: BinOp::Pow,
                    right: Box::new(right),
                },
                span,
            );
        }
    }
    Ok(left)
}

pub(in crate::cypher::parser) fn parse_atom_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    match pair.as_rule() {
        Rule::atom_expr => {
            let mut children = pair.into_inner();
            let primary = children.next().unwrap();
            let mut expr = parse_atom_expr(primary)?;
            // Apply postfix subscript/slice/dot operators.
            for sub in children {
                if sub.as_rule() == Rule::subscript {
                    expr = parse_subscript(expr, sub)?;
                } else if sub.as_rule() == Rule::dot_access {
                    let dot_span = span_from_pair(&sub);
                    let prop =
                        strip_backticks(sub.into_inner().next().unwrap().as_str()).to_string();
                    let span = combine_spans(expr.span, dot_span);
                    // Chained dot access: wrap as DotAccess for correct column
                    // naming (m.a.b → "m.a.b" not "m.a['b']").
                    expr = Expr::new(
                        ExprKind::DotAccess {
                            expr: Box::new(expr),
                            key: prop,
                        },
                        span,
                    );
                }
            }
            Ok(expr)
        }
        Rule::case_expr => parse_case_expr(pair),
        Rule::quantifier_expr => parse_quantifier_expr(pair),
        Rule::dotted_function_call => parse_dotted_function_call(pair),
        Rule::function_call => parse_function_call(pair),
        Rule::unknown_function_call => parse_unknown_function_call(pair),
        Rule::property_access => {
            let span = span_from_pair(&pair);
            let mut parts = pair.into_inner();
            let var = strip_backticks(parts.next().unwrap().as_str()).to_string();
            let prop = strip_backticks(parts.next().unwrap().as_str()).to_string();
            Ok(Expr::new(ExprKind::Property(var, prop), span))
        }
        Rule::has_label_expr => {
            let span = span_from_pair(&pair);
            let mut parts = pair.into_inner();
            let var = parts.next().unwrap().as_str().to_string();
            let label_spec = parts.next().unwrap();
            let labels: Vec<String> = label_spec
                .into_inner()
                .map(|p| p.as_str().to_string())
                .collect();
            Ok(Expr::new(ExprKind::HasLabel(var, labels), span))
        }
        Rule::literal => parse_literal(pair),
        Rule::pattern_comprehension => parse_pattern_comprehension(pair),
        Rule::list_comprehension => parse_list_comprehension(pair),
        Rule::list_literal => {
            let span = span_from_pair(&pair);
            let items: crate::types::Result<Vec<Expr>> = pair
                .into_inner()
                .filter(|p| p.as_rule() == Rule::expr)
                .map(parse_expr)
                .collect();
            Ok(Expr::new(ExprKind::List(items?), span))
        }
        Rule::map_literal => {
            let span = span_from_pair(&pair);
            let mut pairs = Vec::new();
            for p in pair.into_inner() {
                if p.as_rule() == Rule::map_pair {
                    let mut parts = p.into_inner();
                    let key = strip_backticks(parts.next().unwrap().as_str()).to_string();
                    let value_pair = parts.next().unwrap();
                    let value = parse_expr(value_pair)?;
                    pairs.push((key, value));
                }
            }
            Ok(Expr::new(ExprKind::MapLiteral(pairs), span))
        }
        Rule::star => Ok(Expr::new(ExprKind::Star, span_from_pair(&pair))),
        Rule::parameter => {
            let span = span_from_pair(&pair);
            let name = pair.into_inner().next().unwrap().as_str().to_string();
            Ok(Expr::new(ExprKind::Parameter(name), span))
        }
        Rule::variable => {
            let span = span_from_pair(&pair);
            Ok(Expr::new(
                ExprKind::Variable(pair.into_inner().next().unwrap().as_str().to_string()),
                span,
            ))
        }
        Rule::expr => parse_expr(pair),
        Rule::in_expr => parse_in_expr(pair),
        Rule::add_expr => parse_add_expr(pair),
        Rule::mul_expr => parse_mul_expr(pair),
        Rule::exp_expr => parse_exp_expr(pair),
        Rule::unary_minus_expr => {
            let span = span_from_pair(&pair);
            let inner = pair.into_inner().next().unwrap();
            let expr = parse_atom_expr(inner)?;
            Ok(Expr::new(
                ExprKind::BinaryOp {
                    left: Box::new(Expr::new(ExprKind::Literal(LiteralValue::I64(0)), span)),
                    op: BinOp::Sub,
                    right: Box::new(expr),
                },
                span,
            ))
        }
        _ => Err(GraphError::syntax(format!(
            "unexpected expr: {:?}",
            pair.as_rule()
        ))),
    }
}

pub(in crate::cypher::parser) fn parse_subscript(
    base: Expr,
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = combine_spans(base.span, span_from_pair(&pair));
    let inner = pair
        .into_inner()
        .next()
        .unwrap() // subscript_inner
        .into_inner()
        .next()
        .unwrap();
    match inner.as_rule() {
        Rule::slice_full => {
            let mut exprs = inner.into_inner().filter(|p| p.as_rule() == Rule::expr);
            let start = parse_expr(exprs.next().unwrap())?;
            let end = parse_expr(exprs.next().unwrap())?;
            Ok(Expr::new(
                ExprKind::Slice {
                    expr: Box::new(base),
                    start: Some(Box::new(start)),
                    end: Some(Box::new(end)),
                },
                outer_span,
            ))
        }
        Rule::slice_from => {
            let start_pair = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::expr)
                .unwrap();
            Ok(Expr::new(
                ExprKind::Slice {
                    expr: Box::new(base),
                    start: Some(Box::new(parse_expr(start_pair)?)),
                    end: None,
                },
                outer_span,
            ))
        }
        Rule::slice_to => {
            let end_pair = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::expr)
                .unwrap();
            Ok(Expr::new(
                ExprKind::Slice {
                    expr: Box::new(base),
                    start: None,
                    end: Some(Box::new(parse_expr(end_pair)?)),
                },
                outer_span,
            ))
        }
        Rule::expr => Ok(Expr::new(
            ExprKind::Index {
                expr: Box::new(base),
                index: Box::new(parse_expr(inner)?),
            },
            outer_span,
        )),
        _ => Err(GraphError::syntax(format!(
            "unexpected subscript: {:?}",
            inner.as_rule()
        ))),
    }
}

/// Parse a dotted function call: `datetime.fromepoch(args)`, `duration.between(args)`, etc.
pub(in crate::cypher::parser) fn parse_dotted_function_call(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let original_text = pair.as_str().to_string();
    let span = span_from_pair(&pair);
    let mut name = String::new();
    let mut args = Vec::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::dotted_function_name => name = inner.as_str().to_lowercase(),
            Rule::function_args => {
                for arg in inner.into_inner() {
                    match arg.as_rule() {
                        Rule::star => args.push(Expr::new(ExprKind::Star, span_from_pair(&arg))),
                        Rule::expr_list => {
                            for expr_pair in arg.into_inner() {
                                if expr_pair.as_rule() == Rule::expr {
                                    args.push(parse_expr(expr_pair)?);
                                }
                            }
                        }
                        Rule::expr => args.push(parse_expr(arg)?),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(Expr::new(
        ExprKind::FunctionCall {
            name,
            args,
            distinct: false,
            original_text: Some(original_text),
        },
        span,
    ))
}

pub(in crate::cypher::parser) fn parse_function_call(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let original_text = pair.as_str().to_string();
    let span = span_from_pair(&pair);
    let mut name = String::new();
    let mut args = Vec::new();
    let mut distinct = false;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::function_name => name = inner.as_str().to_string(),
            Rule::function_args => {
                for arg in inner.into_inner() {
                    match arg.as_rule() {
                        Rule::star => args.push(Expr::new(ExprKind::Star, span_from_pair(&arg))),
                        Rule::distinct_keyword => distinct = true,
                        Rule::expr_list => {
                            for expr_pair in arg.into_inner() {
                                if expr_pair.as_rule() == Rule::expr {
                                    args.push(parse_expr(expr_pair)?);
                                }
                            }
                        }
                        Rule::expr => args.push(parse_expr(arg)?),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(Expr::new(
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text: Some(original_text),
        },
        span,
    ))
}

/// Parse `ident(args)` where the ident is not a recognized function name.
/// Builds a FunctionCall AST node so the planner can raise `UnknownFunction`
/// with a `did you mean ...?` hint.
pub(in crate::cypher::parser) fn parse_unknown_function_call(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let original_text = pair.as_str().to_string();
    let span = span_from_pair(&pair);
    let mut name = String::new();
    let mut args = Vec::new();
    let mut distinct = false;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::plain_ident => name = inner.as_str().to_string(),
            Rule::function_args => {
                for arg in inner.into_inner() {
                    match arg.as_rule() {
                        Rule::star => args.push(Expr::new(ExprKind::Star, span_from_pair(&arg))),
                        Rule::distinct_keyword => distinct = true,
                        Rule::expr_list => {
                            for expr_pair in arg.into_inner() {
                                if expr_pair.as_rule() == Rule::expr {
                                    args.push(parse_expr(expr_pair)?);
                                }
                            }
                        }
                        Rule::expr => args.push(parse_expr(arg)?),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(Expr::new(
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text: Some(original_text),
        },
        span,
    ))
}

/// Parse an integer literal string, handling decimal, hex (0x), and octal (0o) formats.
pub(in crate::cypher::parser) fn parse_integer_literal(s: &str) -> Result<i64, String> {
    let (negative, digits) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s)
    };
    if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        let abs = u64::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
        negate_unsigned(abs, negative)
    } else if let Some(oct) = digits
        .strip_prefix("0o")
        .or_else(|| digits.strip_prefix("0O"))
    {
        let abs = u64::from_str_radix(oct, 8).map_err(|e| e.to_string())?;
        negate_unsigned(abs, negative)
    } else {
        // Parse the full string (including sign) to correctly handle i64::MIN.
        s.parse::<i64>().map_err(|e| e.to_string())
    }
}

/// Convert unsigned value with sign to i64, handling the i64::MIN edge case.
pub(in crate::cypher::parser) fn negate_unsigned(abs: u64, negative: bool) -> Result<i64, String> {
    if negative {
        if abs == (i64::MAX as u64) + 1 {
            Ok(i64::MIN)
        } else if abs <= i64::MAX as u64 {
            Ok(-(abs as i64))
        } else {
            Err("integer overflow".to_string())
        }
    } else {
        i64::try_from(abs).map_err(|e| e.to_string())
    }
}

pub(in crate::cypher::parser) fn parse_literal(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = span_from_pair(&pair);
    let inner = pair.into_inner().next().unwrap();
    let span = combine_spans(outer_span, span_from_pair(&inner));
    match inner.as_rule() {
        Rule::integer_literal => {
            let s = inner.as_str();
            let n: i64 = parse_integer_literal(s)
                .map_err(|e| GraphError::syntax(format!("invalid integer literal: {e}")))?;
            Ok(Expr::new(ExprKind::Literal(LiteralValue::I64(n)), span))
        }
        Rule::float_literal => {
            let mut n: f64 = inner
                .as_str()
                .parse()
                .map_err(|e| GraphError::syntax(format!("invalid float: {e}")))?;
            if n.is_infinite() {
                return Err(GraphError::syntax(
                    "floating point value overflow".to_string(),
                ));
            }
            // Normalize negative zero to positive zero.
            if n == 0.0 && n.is_sign_negative() {
                n = 0.0;
            }
            Ok(Expr::new(ExprKind::Literal(LiteralValue::F64(n)), span))
        }
        Rule::string_literal => {
            let quoted = inner.into_inner().next().unwrap();
            let raw = quoted.into_inner().next().unwrap().as_str();
            Ok(Expr::new(
                ExprKind::Literal(LiteralValue::String(unescape_string(raw)?)),
                span,
            ))
        }
        Rule::bool_literal => {
            let b = inner.as_str().to_uppercase() == "TRUE";
            Ok(Expr::new(ExprKind::Literal(LiteralValue::Bool(b)), span))
        }
        Rule::null_literal => Ok(Expr::new(ExprKind::Literal(LiteralValue::Null), span)),
        _ => Err(GraphError::syntax(format!(
            "unexpected literal: {:?}",
            inner.as_rule()
        ))),
    }
}

pub(in crate::cypher::parser) fn parse_list_comprehension(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = span_from_pair(&pair);
    let mut variable = None;
    let mut list_expr = None;
    let mut filter = None;
    let mut map_expr = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => variable = Some(strip_backticks(inner.as_str()).to_string()),
            Rule::expr => list_expr = Some(Box::new(parse_expr(inner)?)),
            Rule::list_comp_where => {
                let bool_expr = inner.into_inner().next().unwrap();
                filter = Some(Box::new(parse_bool_expr(bool_expr)?));
            }
            Rule::list_comp_map => {
                let expr = inner.into_inner().next().unwrap();
                map_expr = Some(Box::new(parse_expr(expr)?));
            }
            _ => {}
        }
    }

    Ok(Expr::new(
        ExprKind::ListComprehension {
            variable: variable.ok_or_else(|| {
                GraphError::syntax("missing variable in list comprehension".to_string())
            })?,
            list_expr: list_expr.ok_or_else(|| {
                GraphError::syntax("missing list expression in list comprehension".to_string())
            })?,
            filter,
            map_expr,
        },
        outer_span,
    ))
}

/// Parse a pattern comprehension: [(p = )? pattern (WHERE pred)? | expr].
pub(in crate::cypher::parser) fn parse_pattern_comprehension(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = span_from_pair(&pair);
    let mut path_variable: Option<String> = None;
    let mut pattern = None;
    let mut where_clause = None;
    let mut map_expr = None;

    // Track whether the first ident is the path variable (before `=`).
    // Grammar: "[" ~ (ident ~ "=")? ~ pattern ~ (WHERE ~ bool_expr)? ~ "|" ~ expr ~ "]"
    // The inner pairs will be: (ident)? pattern (bool_expr)? expr
    let mut inner_pairs: Vec<_> = pair.into_inner().collect();

    // The last pair is always the map_expr (the expr after `|`).
    // The grammar guarantees at least a pattern and an expr.
    let mut i = 0;
    while i < inner_pairs.len() {
        let rule = inner_pairs[i].as_rule();
        match rule {
            Rule::ident => {
                // This is the path variable (the `p = ` part).
                path_variable = Some(strip_backticks(inner_pairs[i].as_str()).to_string());
                i += 1;
            }
            Rule::pattern => {
                pattern = Some(parse_pattern(inner_pairs.remove(i))?);
            }
            Rule::bool_expr => {
                where_clause = Some(Box::new(parse_bool_expr(inner_pairs.remove(i))?));
            }
            Rule::expr => {
                map_expr = Some(Box::new(parse_expr(inner_pairs.remove(i))?));
            }
            _ => {
                i += 1;
            }
        }
    }

    Ok(Expr::new(
        ExprKind::PatternComprehension {
            path_variable,
            pattern: pattern.ok_or_else(|| {
                GraphError::syntax("missing pattern in pattern comprehension".to_string())
            })?,
            where_clause,
            map_expr: map_expr.ok_or_else(|| {
                GraphError::syntax("missing map expression in pattern comprehension".to_string())
            })?,
        },
        outer_span,
    ))
}

/// Parse a quantifier predicate: none/single/any/all(x IN list WHERE pred).
pub(in crate::cypher::parser) fn parse_quantifier_expr(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let outer_span = span_from_pair(&pair);
    let mut kind = None;
    let mut variable = None;
    let mut list_expr = None;
    let mut predicate = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::quantifier_name => {
                kind = Some(match inner.as_str().to_lowercase().as_str() {
                    "none" => QuantifierKind::None,
                    "single" => QuantifierKind::Single,
                    "any" => QuantifierKind::Any,
                    "all" => QuantifierKind::All,
                    other => {
                        return Err(GraphError::syntax(format!("unknown quantifier: {other}")))
                    }
                });
            }
            Rule::ident => variable = Some(strip_backticks(inner.as_str()).to_string()),
            Rule::expr => list_expr = Some(Box::new(parse_expr(inner)?)),
            Rule::where_clause => predicate = Some(Box::new(parse_where(inner)?)),
            _ => {}
        }
    }

    Ok(Expr::new(
        ExprKind::Quantifier {
            kind: kind.ok_or_else(|| GraphError::syntax("missing quantifier name".to_string()))?,
            variable: variable
                .ok_or_else(|| GraphError::syntax("missing variable in quantifier".to_string()))?,
            list_expr: list_expr.ok_or_else(|| {
                GraphError::syntax("missing list expression in quantifier".to_string())
            })?,
            predicate: predicate.ok_or_else(|| {
                GraphError::syntax("missing WHERE predicate in quantifier".to_string())
            })?,
        },
        outer_span,
    ))
}
