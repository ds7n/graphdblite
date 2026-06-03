//! Statement parsing — UNION, single statements, MATCH/CREATE/DELETE/SET/REMOVE/MERGE/UNWIND, multi-clause sequencing.

use crate::types::{ErrorCode, GraphError, QueryPhase};

use super::clause::*;
use super::expr::*;
use super::pattern::*;
use super::*;

pub(in crate::cypher::parser) fn parse_union_stmt(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Statement> {
    let mut statements = Vec::new();
    let mut all = true; // UNION ALL by default; plain UNION sets to false.
    let mut has_all = false;
    let mut has_plain = false;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::single_stmt => {
                let inner = child.into_inner().next().unwrap();
                statements.push(parse_single_stmt(inner)?);
            }
            Rule::union_op => {
                let text = child.as_str().to_uppercase();
                if text.contains("ALL") {
                    has_all = true;
                } else {
                    has_plain = true;
                    all = false;
                }
            }
            _ => {}
        }
    }

    if statements.len() == 1 {
        Ok(statements.into_iter().next().unwrap())
    } else {
        // Reject mixing UNION and UNION ALL.
        if has_all && has_plain {
            return Err(GraphError::query(
                QueryPhase::Parse,
                ErrorCode::InvalidClauseComposition,
                "InvalidClauseComposition: cannot mix UNION and UNION ALL",
            ));
        }
        Ok(Statement::Union { statements, all })
    }
}

pub(in crate::cypher::parser) fn parse_single_stmt(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Statement> {
    match pair.as_rule() {
        Rule::explain_stmt => parse_explain(pair),
        Rule::multi_clause_stmt => parse_multi_clause(pair).map(Statement::MultiClause),
        Rule::match_stmt => parse_match(pair).map(Statement::Match),
        Rule::create_stmt => parse_create(pair).map(Statement::Create),
        Rule::match_create_stmt => parse_match_create(pair).map(Statement::MatchCreate),
        Rule::match_merge_stmt => parse_match_merge(pair).map(Statement::MatchMerge),
        Rule::delete_stmt => parse_delete(pair).map(Statement::Delete),
        Rule::set_stmt => parse_set(pair).map(Statement::Set),
        Rule::remove_stmt => parse_remove(pair).map(Statement::Remove),
        Rule::merge_stmt => parse_merge(pair).map(Statement::Merge),
        Rule::unwind_stmt => parse_unwind(pair).map(Statement::Unwind),
        Rule::with_stmt => parse_with_stmt(pair).map(Statement::Match),
        Rule::return_stmt => parse_return_stmt(pair).map(Statement::Return),
        Rule::call_stmt => parse_call(pair),
        _ => Err(GraphError::syntax(format!(
            "unexpected rule: {:?}",
            pair.as_rule()
        ))),
    }
}

pub(in crate::cypher::parser) fn parse_explain(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Statement> {
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| GraphError::syntax("EXPLAIN requires a statement".to_string()))?;
    let stmt = match inner.as_rule() {
        Rule::match_stmt => parse_match(inner).map(Statement::Match)?,
        Rule::match_create_stmt => parse_match_create(inner).map(Statement::MatchCreate)?,
        Rule::unwind_stmt => parse_unwind(inner).map(Statement::Unwind)?,
        Rule::with_stmt => parse_with_stmt(inner).map(Statement::Match)?,
        Rule::multi_clause_stmt => parse_multi_clause(inner).map(Statement::MultiClause)?,
        _ => {
            return Err(GraphError::syntax(format!(
                "EXPLAIN not supported for {:?}",
                inner.as_rule()
            )))
        }
    };
    Ok(Statement::Explain(Box::new(stmt)))
}

/// Parse YIELD clause items from a yield_clause rule pair.
#[allow(clippy::type_complexity)]
pub(in crate::cypher::parser) fn parse_yield_clause(
    pair: pest::iterators::Pair<Rule>,
) -> (Option<Vec<(String, Option<String>)>>, bool) {
    let mut yield_items = None;
    let mut yield_star = false;
    for yc in pair.into_inner() {
        match yc.as_rule() {
            Rule::yield_star => {
                yield_star = true;
            }
            Rule::yield_items => {
                let mut items = Vec::new();
                for yi in yc.into_inner() {
                    if yi.as_rule() == Rule::yield_item {
                        let mut idents = yi.into_inner();
                        let col = idents
                            .next()
                            .expect("yield_item must have ident")
                            .as_str()
                            .to_string();
                        let alias = idents.next().map(|a| a.as_str().to_string());
                        items.push((col, alias));
                    }
                }
                yield_items = Some(items);
            }
            _ => {}
        }
    }
    (yield_items, yield_star)
}

pub(in crate::cypher::parser) fn parse_call(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Statement> {
    let mut procedure_name = String::new();
    let mut args = Vec::new();
    let mut yield_items: Option<Vec<(String, Option<String>)>> = None;
    let mut yield_star = false;
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    // Check raw text for parentheses to distinguish explicit vs implicit args.
    let raw = pair.as_str();
    let has_parens = {
        if let Some(proc_end) = raw.find('(') {
            let yield_pos = raw.to_ascii_uppercase().find("YIELD");
            yield_pos.is_none() || proc_end < yield_pos.unwrap()
        } else {
            false
        }
    };

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::procedure_name => {
                procedure_name = inner.as_str().to_string();
            }
            Rule::expr => {
                args.push(parse_expr(inner)?);
            }
            Rule::yield_clause => {
                let (items, star) = parse_yield_clause(inner);
                yield_items = items;
                yield_star = star;
            }
            Rule::return_clause => {
                return_clause = Some(parse_return(inner)?);
            }
            Rule::order_by_clause => {
                order_by = parse_order_by(inner)?;
            }
            Rule::skip_clause => {
                skip = Some(Box::new(parse_skip(inner)?));
            }
            Rule::limit_clause => {
                limit = Some(Box::new(parse_limit(inner)?));
            }
            _ => {}
        }
    }

    Ok(Statement::Call {
        procedure_name,
        args,
        implicit_args: !has_parens,
        yield_items,
        yield_star,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

/// Parsed output of a MATCH clause: required patterns, optional pattern groups, and WHERE filter.
type MatchParts = (Vec<Pattern>, Vec<OptionalMatch>, Option<Expr>);

/// Parse a CALL clause within a multi-clause statement.
pub(in crate::cypher::parser) fn parse_multi_call_clause(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Clause> {
    let mut procedure_name = String::new();
    let mut args = Vec::new();
    let mut yield_items: Option<Vec<(String, Option<String>)>> = None;
    let mut yield_star = false;

    // Check raw text for parentheses to distinguish explicit vs implicit args.
    let raw = pair.as_str();
    let has_parens = {
        if let Some(proc_end) = raw.find('(') {
            let yield_pos = raw.to_ascii_uppercase().find("YIELD");
            yield_pos.is_none() || proc_end < yield_pos.unwrap()
        } else {
            false
        }
    };

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::procedure_name => {
                procedure_name = inner.as_str().to_string();
            }
            Rule::expr => {
                args.push(parse_expr(inner)?);
            }
            Rule::yield_clause => {
                let (items, star) = parse_yield_clause(inner);
                yield_items = items;
                yield_star = star;
            }
            _ => {}
        }
    }

    Ok(Clause::Call {
        procedure_name,
        args,
        implicit_args: !has_parens,
        yield_items,
        yield_star,
    })
}

/// Extract patterns, optional patterns, and WHERE from a `match_part` rule.
pub(in crate::cypher::parser) fn parse_match_part(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MatchParts> {
    let mut patterns = Vec::new();
    let mut optional_patterns = Vec::new();
    let mut where_clause = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::optional_match_clause => {
                optional_patterns.push(parse_optional_match_clause(inner)?);
            }
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            _ => {}
        }
    }

    Ok((patterns, optional_patterns, where_clause))
}

/// Parse an optional_match_clause into an OptionalMatch with patterns and optional WHERE.
pub(in crate::cypher::parser) fn parse_optional_match_clause(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<OptionalMatch> {
    let mut opt_patterns = Vec::new();
    let mut opt_where = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::pattern_list => opt_patterns = parse_pattern_list(child)?,
            Rule::where_clause => opt_where = Some(parse_where(child)?),
            _ => {}
        }
    }
    Ok(OptionalMatch {
        patterns: opt_patterns,
        where_clause: opt_where,
    })
}

pub(in crate::cypher::parser) fn parse_match(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MatchStatement> {
    let mut patterns = Vec::new();
    let mut optional_patterns = Vec::new();
    let mut where_clause = None;
    let mut intermediate_clauses = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;
    let mut first_match_part = true;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::match_part => {
                let (mp_patterns, mp_optional, mp_where) = parse_match_part(inner)?;
                if first_match_part {
                    patterns = mp_patterns;
                    optional_patterns = mp_optional;
                    where_clause = mp_where;
                    first_match_part = false;
                } else {
                    intermediate_clauses.push(IntermediateClause::Match(IntermediateMatch {
                        patterns: mp_patterns,
                        optional_patterns: mp_optional,
                        where_clause: mp_where,
                    }));
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
            _ => {}
        }
    }

    Ok(MatchStatement {
        patterns,
        optional_patterns,
        where_clause,
        intermediate_clauses,
        return_clause: return_clause
            .ok_or_else(|| GraphError::syntax("missing RETURN clause".to_string()))?,
        order_by,
        skip,
        limit,
    })
}

/// Parse a standalone `WITH ... RETURN` statement.
///
/// Produces a MatchStatement with empty patterns — the leading WITH clause
/// is the first intermediate clause.
pub(in crate::cypher::parser) fn parse_with_stmt(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MatchStatement> {
    let mut intermediate_clauses = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::with_clause => {
                intermediate_clauses.push(IntermediateClause::With(parse_with(inner)?))
            }
            Rule::match_part => {
                let (mp_patterns, mp_optional, mp_where) = parse_match_part(inner)?;
                intermediate_clauses.push(IntermediateClause::Match(IntermediateMatch {
                    patterns: mp_patterns,
                    optional_patterns: mp_optional,
                    where_clause: mp_where,
                }));
            }
            Rule::unwind_clause => {
                intermediate_clauses.push(IntermediateClause::Unwind(parse_unwind_clause(inner)?))
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(MatchStatement {
        patterns: Vec::new(),
        optional_patterns: Vec::new(),
        where_clause: None,
        intermediate_clauses,
        return_clause: return_clause
            .ok_or_else(|| GraphError::syntax("missing RETURN clause".to_string()))?,
        order_by,
        skip,
        limit,
    })
}

pub(in crate::cypher::parser) fn parse_return_stmt(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<ReturnStatement> {
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(ReturnStatement {
        return_clause: return_clause
            .ok_or_else(|| GraphError::syntax("missing RETURN clause".to_string()))?,
        order_by,
        skip,
        limit,
    })
}

pub(in crate::cypher::parser) fn parse_multi_clause(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MultiClauseStatement> {
    let mut clauses = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::multi_match_clause => {
                let (patterns, optional_patterns, where_clause) = parse_match_part(inner)?;
                clauses.push(Clause::Match {
                    patterns,
                    optional_patterns,
                    where_clause,
                });
            }
            Rule::multi_create_clause => {
                let mut patterns = Vec::new();
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::create_pattern_list {
                        for pat in child.into_inner() {
                            if pat.as_rule() == Rule::create_pattern {
                                patterns.push(parse_pattern_inner(pat)?);
                            }
                        }
                    }
                }
                clauses.push(Clause::Create { patterns });
            }
            Rule::multi_merge_clause => {
                let mut merge_pattern = None;
                let mut on_create = Vec::new();
                let mut on_match = Vec::new();
                for child in inner.into_inner() {
                    match child.as_rule() {
                        Rule::pattern_item => merge_pattern = Some(parse_pattern_item(child)?),
                        Rule::on_create_clause => {
                            for grandchild in child.into_inner() {
                                if grandchild.as_rule() == Rule::assignment_list {
                                    on_create = parse_set_item_list(grandchild)?;
                                }
                            }
                        }
                        Rule::on_match_clause => {
                            for grandchild in child.into_inner() {
                                if grandchild.as_rule() == Rule::assignment_list {
                                    on_match = parse_set_item_list(grandchild)?;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(pattern) = merge_pattern {
                    clauses.push(Clause::Merge {
                        pattern,
                        on_create,
                        on_match,
                    });
                }
            }
            Rule::multi_unwind_clause | Rule::unwind_clause => {
                let uc = parse_unwind_clause(inner)?;
                clauses.push(Clause::Unwind(uc));
            }
            Rule::with_clause => {
                clauses.push(Clause::With(parse_with(inner)?));
            }
            Rule::multi_set_clause => {
                let mut items = Vec::new();
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        items = parse_set_item_list(child)?;
                    }
                }
                clauses.push(Clause::Set { items });
            }
            Rule::multi_remove_clause => {
                let mut remove_items = Vec::new();
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::remove_item_list {
                        remove_items = parse_remove_item_list(child)?;
                    }
                }
                clauses.push(Clause::Remove {
                    items: remove_items,
                });
            }
            Rule::multi_delete_clause => {
                let mut detach = false;
                let mut exprs = Vec::new();
                for child in inner.into_inner() {
                    match child.as_rule() {
                        Rule::detach_keyword => detach = true,
                        Rule::delete_expr_list => {
                            for expr_pair in child.into_inner() {
                                if expr_pair.as_rule() == Rule::expr {
                                    exprs.push(parse_expr(expr_pair)?);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                clauses.push(Clause::Delete { exprs, detach });
            }
            Rule::multi_call_clause => {
                let call_clause = parse_multi_call_clause(inner)?;
                clauses.push(call_clause);
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(MultiClauseStatement {
        clauses,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

pub(in crate::cypher::parser) fn parse_create(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<CreateStatement> {
    let mut patterns = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::create_pattern_list => {
                for pat in inner.into_inner() {
                    if pat.as_rule() == Rule::create_pattern {
                        patterns.push(parse_pattern_inner(pat)?);
                    }
                }
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }
    Ok(CreateStatement {
        patterns,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

pub(in crate::cypher::parser) fn parse_match_create(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MatchCreateStatement> {
    let mut patterns = Vec::new();
    let mut where_clause = None;
    let mut create_patterns = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            Rule::create_pattern_list => {
                for pat in inner.into_inner() {
                    if pat.as_rule() == Rule::create_pattern {
                        create_patterns.push(parse_pattern_inner(pat)?);
                    }
                }
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(MatchCreateStatement {
        patterns,
        where_clause,
        create_patterns,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

pub(in crate::cypher::parser) fn parse_match_merge(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MatchMergeStatement> {
    let mut patterns = Vec::new();
    let mut where_clause = None;
    let mut merge_pattern = None;
    let mut on_create = Vec::new();
    let mut on_match = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            Rule::pattern_item => merge_pattern = Some(parse_pattern_item(inner)?),
            Rule::on_create_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        on_create = parse_set_item_list(child)?;
                    }
                }
            }
            Rule::on_match_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        on_match = parse_set_item_list(child)?;
                    }
                }
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(MatchMergeStatement {
        patterns,
        where_clause,
        merge_pattern: merge_pattern
            .ok_or_else(|| GraphError::syntax("missing MERGE pattern".to_string()))?,
        on_create,
        on_match,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

pub(in crate::cypher::parser) fn parse_delete(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<DeleteStatement> {
    let mut patterns = Vec::new();
    let mut optional_patterns = Vec::new();
    let mut where_clause = None;
    let mut detach = false;
    let mut exprs = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::optional_match_clause => {
                optional_patterns.push(parse_optional_match_clause(inner)?);
            }
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            Rule::detach_keyword => detach = true,
            Rule::delete_expr_list => {
                for expr_pair in inner.into_inner() {
                    if expr_pair.as_rule() == Rule::expr {
                        exprs.push(parse_expr(expr_pair)?);
                    }
                }
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(DeleteStatement {
        patterns,
        optional_patterns,
        where_clause,
        detach,
        exprs,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

pub(in crate::cypher::parser) fn parse_set(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<SetStatement> {
    let mut patterns = Vec::new();
    let mut optional_patterns = Vec::new();
    let mut where_clause = None;
    let mut items = Vec::new();
    let mut intermediate_clauses = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;
    let mut seen_set = false;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list if !seen_set => patterns = parse_pattern_list(inner)?,
            Rule::optional_match_clause if !seen_set => {
                optional_patterns.push(parse_optional_match_clause(inner)?);
            }
            Rule::where_clause if !seen_set => where_clause = Some(parse_where(inner)?),
            Rule::assignment_list => {
                items = parse_set_item_list(inner)?;
                seen_set = true;
            }
            Rule::with_clause => {
                intermediate_clauses.push(IntermediateClause::With(parse_with(inner)?));
            }
            Rule::match_part => {
                let (mp_patterns, mp_optional, mp_where) = parse_match_part(inner)?;
                intermediate_clauses.push(IntermediateClause::Match(IntermediateMatch {
                    patterns: mp_patterns,
                    optional_patterns: mp_optional,
                    where_clause: mp_where,
                }));
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(SetStatement {
        patterns,
        optional_patterns,
        where_clause,
        items,
        intermediate_clauses,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

/// Parse the extended SET item list (labels, map overwrite, map merge, or property assignment).
pub(in crate::cypher::parser) fn parse_set_item_list(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Vec<SetItem>> {
    let mut items = Vec::new();
    for child in pair.into_inner() {
        if child.as_rule() == Rule::set_item {
            let inner = child.into_inner().next().unwrap();
            match inner.as_rule() {
                Rule::set_label => {
                    let mut variable = String::new();
                    let mut labels = Vec::new();
                    for part in inner.into_inner() {
                        match part.as_rule() {
                            Rule::ident => variable = strip_backticks(part.as_str()).to_string(),
                            Rule::label_spec => {
                                for label in part.into_inner() {
                                    labels.push(label.as_str().to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                    items.push(SetItem::Label { variable, labels });
                }
                Rule::set_map_merge => {
                    let mut variable = String::new();
                    let mut value = None;
                    for part in inner.into_inner() {
                        match part.as_rule() {
                            Rule::ident => variable = strip_backticks(part.as_str()).to_string(),
                            Rule::expr => value = Some(parse_expr(part)?),
                            _ => {}
                        }
                    }
                    items.push(SetItem::MapMerge {
                        variable,
                        value: value.unwrap(),
                    });
                }
                Rule::set_map => {
                    let mut variable = String::new();
                    let mut value = None;
                    for part in inner.into_inner() {
                        match part.as_rule() {
                            Rule::ident => variable = strip_backticks(part.as_str()).to_string(),
                            Rule::expr => value = Some(parse_expr(part)?),
                            _ => {}
                        }
                    }
                    items.push(SetItem::MapOverwrite {
                        variable,
                        value: value.unwrap(),
                    });
                }
                Rule::assignment => {
                    let mut children = inner.into_inner();
                    let prop_access = children.next().unwrap();
                    let value = parse_expr(children.next().unwrap())?;
                    if prop_access.as_rule() == Rule::expr_property_access {
                        // (expr).property = value — resolve expr to get variable
                        let mut prop_parts = prop_access.into_inner();
                        let expr = parse_expr(prop_parts.next().unwrap())?;
                        let property = prop_parts.next().unwrap().as_str().to_string();
                        // Extract variable name from the expression
                        if let ExprKind::Variable(var) = expr.kind {
                            items.push(SetItem::Property(Assignment {
                                variable: var,
                                property,
                                value,
                            }));
                        } else {
                            return Err(GraphError::syntax(
                                "SET target expression must be a variable".to_string(),
                            ));
                        }
                    } else {
                        let mut prop_parts = prop_access.into_inner();
                        let variable = prop_parts.next().unwrap().as_str().to_string();
                        let property = prop_parts.next().unwrap().as_str().to_string();
                        items.push(SetItem::Property(Assignment {
                            variable,
                            property,
                            value,
                        }));
                    }
                }
                _ => {
                    return Err(GraphError::syntax(format!(
                        "unexpected set item: {:?}",
                        inner.as_rule()
                    )));
                }
            }
        }
    }
    Ok(items)
}

pub(in crate::cypher::parser) fn parse_remove(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<RemoveStatement> {
    let mut patterns = Vec::new();
    let mut optional_patterns = Vec::new();
    let mut where_clause = None;
    let mut items = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::optional_match_clause => {
                optional_patterns.push(parse_optional_match_clause(inner)?);
            }
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            Rule::remove_item_list => {
                for item in inner.into_inner() {
                    if item.as_rule() == Rule::remove_item {
                        items.push(parse_remove_item(item)?);
                    }
                }
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(RemoveStatement {
        patterns,
        optional_patterns,
        where_clause,
        items,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

/// Parse a remove_item_list into a Vec<RemoveItem>.
pub(in crate::cypher::parser) fn parse_remove_item_list(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Vec<RemoveItem>> {
    let mut items = Vec::new();
    for item in pair.into_inner() {
        if item.as_rule() == Rule::remove_item {
            items.push(parse_remove_item(item)?);
        }
    }
    Ok(items)
}

/// Parse a single remove item: either `n.prop` or `n:Label`.
pub(in crate::cypher::parser) fn parse_remove_item(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<RemoveItem> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::remove_property => {
            let mut parts = inner.into_inner().next().unwrap().into_inner();
            let variable = parts.next().unwrap().as_str().to_string();
            let property = parts.next().unwrap().as_str().to_string();
            Ok(RemoveItem::Property { variable, property })
        }
        Rule::remove_label => {
            let mut variable = String::new();
            let mut labels = Vec::new();
            for child in inner.into_inner() {
                match child.as_rule() {
                    Rule::ident => variable = strip_backticks(child.as_str()).to_string(),
                    Rule::label_spec => {
                        for label in child.into_inner() {
                            labels.push(label.as_str().to_string());
                        }
                    }
                    _ => {}
                }
            }
            Ok(RemoveItem::Label { variable, labels })
        }
        _ => Err(GraphError::syntax(format!(
            "unexpected remove item: {:?}",
            inner.as_rule()
        ))),
    }
}

pub(in crate::cypher::parser) fn parse_merge(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MergeStatement> {
    let mut pattern = None;
    let mut on_create = Vec::new();
    let mut on_match = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_item => pattern = Some(parse_pattern_item(inner)?),
            Rule::on_create_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        on_create = parse_set_item_list(child)?;
                    }
                }
            }
            Rule::on_match_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        on_match = parse_set_item_list(child)?;
                    }
                }
            }
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::skip_clause => skip = Some(parse_skip(inner)?),
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(MergeStatement {
        pattern: pattern.ok_or_else(|| GraphError::syntax("missing MERGE pattern".to_string()))?,
        on_create,
        on_match,
        return_clause,
        order_by,
        skip,
        limit,
    })
}

// === Pattern parsing ===
