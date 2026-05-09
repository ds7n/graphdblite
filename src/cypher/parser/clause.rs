//! Clause parsing — WHERE, WITH, UNWIND, RETURN, ORDER BY, SKIP, LIMIT, assignment lists.

use crate::types::GraphError;

use super::expr::*;
use super::pattern::*;
use super::statement::*;
use super::*;

pub(in crate::cypher::parser) fn parse_where(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let bool_expr = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::bool_expr)
        .unwrap();
    parse_bool_expr(bool_expr)
}

pub(in crate::cypher::parser) fn parse_with(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<WithClause> {
    let mut items = Vec::new();
    let mut distinct = false;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;
    let mut where_clause = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::distinct_keyword => distinct = true,
            Rule::return_items => {
                items = inner
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::return_item)
                    .map(|p| {
                        let mut expr = None;
                        let mut alias = None;
                        for child in p.into_inner() {
                            match child.as_rule() {
                                Rule::expr => expr = Some(parse_expr(child)?),
                                Rule::alias => {
                                    for a in child.into_inner() {
                                        if matches!(
                                            a.as_rule(),
                                            Rule::ident
                                                | Rule::backtick_ident
                                                | Rule::symbolic_name
                                        ) {
                                            alias = Some(strip_backticks(a.as_str()).to_string());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(ReturnItem {
                            expr: expr.unwrap(),
                            alias,
                        })
                    })
                    .collect::<crate::types::Result<Vec<_>>>()?;
            }
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            _ => {}
        }
    }

    Ok(WithClause {
        items,
        distinct,
        order_by,
        skip,
        limit,
        where_clause,
    })
}

pub(in crate::cypher::parser) fn parse_unwind(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<UnwindStatement> {
    let mut expr = None;
    let mut alias = None;
    let mut body = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => expr = Some(parse_expr(inner)?),
            Rule::ident => alias = Some(strip_backticks(inner.as_str()).to_string()),
            Rule::unwind_return => {
                let mut where_clause = None;
                let mut intermediate_clauses = Vec::new();
                let mut return_clause = None;
                let mut order_by = Vec::new();
                let mut skip = None;
                let mut limit = None;
                for child in inner.into_inner() {
                    match child.as_rule() {
                        Rule::where_clause => where_clause = Some(parse_where(child)?),
                        Rule::with_clause => {
                            intermediate_clauses.push(IntermediateClause::With(parse_with(child)?));
                        }
                        Rule::match_part => {
                            let (mp_patterns, mp_optional, mp_where) = parse_match_part(child)?;
                            intermediate_clauses.push(IntermediateClause::Match(
                                IntermediateMatch {
                                    patterns: mp_patterns,
                                    optional_patterns: mp_optional,
                                    where_clause: mp_where,
                                },
                            ));
                        }
                        Rule::unwind_clause => {
                            intermediate_clauses
                                .push(IntermediateClause::Unwind(parse_unwind_clause(child)?));
                        }
                        Rule::return_clause => return_clause = Some(parse_return(child)?),
                        Rule::order_by_clause => order_by = parse_order_by(child)?,
                        Rule::skip_clause => skip = Some(parse_skip(child)?),
                        Rule::limit_clause => limit = Some(parse_limit(child)?),
                        _ => {}
                    }
                }
                body = Some(UnwindBody::Return {
                    where_clause,
                    intermediate_clauses,
                    return_clause: return_clause.ok_or_else(|| {
                        GraphError::syntax("missing RETURN clause in UNWIND".to_string())
                    })?,
                    order_by,
                    skip,
                    limit,
                });
            }
            Rule::unwind_create => {
                let mut patterns = Vec::new();
                let mut intermediate_clauses = Vec::new();
                let mut return_clause = None;
                let mut order_by = Vec::new();
                let mut skip = None;
                let mut limit = None;
                for child in inner.into_inner() {
                    match child.as_rule() {
                        Rule::create_pattern_list => {
                            for pat in child.into_inner() {
                                if pat.as_rule() == Rule::create_pattern {
                                    patterns.push(parse_pattern_inner(pat)?);
                                }
                            }
                        }
                        Rule::with_clause => {
                            intermediate_clauses.push(IntermediateClause::With(parse_with(child)?));
                        }
                        Rule::match_part => {
                            let (mp_patterns, mp_optional, mp_where) = parse_match_part(child)?;
                            intermediate_clauses.push(IntermediateClause::Match(
                                IntermediateMatch {
                                    patterns: mp_patterns,
                                    optional_patterns: mp_optional,
                                    where_clause: mp_where,
                                },
                            ));
                        }
                        Rule::unwind_clause => {
                            intermediate_clauses
                                .push(IntermediateClause::Unwind(parse_unwind_clause(child)?));
                        }
                        Rule::return_clause => return_clause = Some(parse_return(child)?),
                        Rule::order_by_clause => order_by = parse_order_by(child)?,
                        Rule::skip_clause => skip = Some(parse_skip(child)?),
                        Rule::limit_clause => limit = Some(parse_limit(child)?),
                        _ => {}
                    }
                }
                body = Some(UnwindBody::Create {
                    patterns,
                    intermediate_clauses,
                    return_clause,
                    order_by,
                    skip,
                    limit,
                });
            }
            _ => {}
        }
    }

    Ok(UnwindStatement {
        expr: expr.ok_or_else(|| GraphError::syntax("missing UNWIND expression".to_string()))?,
        alias: alias.ok_or_else(|| GraphError::syntax("missing UNWIND alias".to_string()))?,
        body: body.ok_or_else(|| GraphError::syntax("missing UNWIND body".to_string()))?,
    })
}

pub(in crate::cypher::parser) fn parse_unwind_clause(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<UnwindClause> {
    let mut expr = None;
    let mut alias = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => expr = Some(parse_expr(inner)?),
            Rule::ident => alias = Some(strip_backticks(inner.as_str()).to_string()),
            _ => {}
        }
    }

    Ok(UnwindClause {
        expr: expr.ok_or_else(|| GraphError::syntax("missing UNWIND expression".to_string()))?,
        alias: alias.ok_or_else(|| GraphError::syntax("missing UNWIND alias".to_string()))?,
    })
}

pub(in crate::cypher::parser) fn parse_return(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<ReturnClause> {
    let inner_pairs: Vec<_> = pair.into_inner().collect();
    let distinct = inner_pairs
        .iter()
        .any(|p| p.as_rule() == Rule::distinct_keyword);
    let items_pair = inner_pairs
        .into_iter()
        .find(|p| p.as_rule() == Rule::return_items)
        .unwrap();

    let items: Vec<ReturnItem> = items_pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::return_item)
        .map(|p| {
            let mut expr = None;
            let mut alias = None;
            for inner in p.into_inner() {
                match inner.as_rule() {
                    Rule::expr => expr = Some(parse_expr(inner)?),
                    Rule::alias => {
                        for child in inner.into_inner() {
                            if matches!(
                                child.as_rule(),
                                Rule::ident | Rule::backtick_ident | Rule::symbolic_name
                            ) {
                                alias = Some(strip_backticks(child.as_str()).to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(ReturnItem {
                expr: expr.unwrap(),
                alias,
            })
        })
        .collect::<crate::types::Result<Vec<_>>>()?;

    Ok(ReturnClause { items, distinct })
}

pub(in crate::cypher::parser) fn parse_order_by(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Vec<SortItem>> {
    let items_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::sort_items)
        .unwrap();

    items_pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::sort_item)
        .map(|p| {
            let mut expr = None;
            let mut descending = false;
            for inner in p.into_inner() {
                match inner.as_rule() {
                    Rule::expr => expr = Some(parse_expr(inner)?),
                    Rule::sort_direction => {
                        let dir = inner.as_str().to_uppercase();
                        descending = dir == "DESC" || dir == "DESCENDING";
                    }
                    _ => {}
                }
            }
            Ok(SortItem {
                expr: expr.unwrap(),
                descending,
            })
        })
        .collect()
}

pub(in crate::cypher::parser) fn parse_skip(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let expr_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::expr)
        .unwrap();
    parse_expr(expr_pair)
}

pub(in crate::cypher::parser) fn parse_limit(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Expr> {
    let expr_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::expr)
        .unwrap();
    parse_expr(expr_pair)
}

// === Expression parsing ===
