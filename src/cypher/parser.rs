use std::collections::HashMap;

use pest::Parser;
use pest_derive::Parser;

use crate::cypher::ast::*;
use crate::types::GraphError;

#[derive(Parser)]
#[grammar = "cypher/grammar.pest"]
struct CypherParser;

/// Process backslash escape sequences in a string literal.
fn unescape_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('\'') => out.push('\''),
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
    out
}

/// Parse a Cypher query string into a Statement AST.
pub fn parse(input: &str) -> crate::types::Result<Statement> {
    let pairs = CypherParser::parse(Rule::statement, input)
        .map_err(|e| GraphError::ParseError(humanize_pest_error(e)))?;

    let union_pair = pairs
        .into_iter()
        .next()
        .unwrap()
        .into_inner()
        .find(|p| p.as_rule() == Rule::union_stmt)
        .ok_or_else(|| GraphError::Serialization("empty statement".to_string()))?;

    parse_union_stmt(union_pair)
}

fn parse_union_stmt(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Statement> {
    let mut statements = Vec::new();
    let mut all = true; // UNION ALL by default; plain UNION sets to false.

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::single_stmt => {
                let inner = child.into_inner().next().unwrap();
                statements.push(parse_single_stmt(inner)?);
            }
            Rule::union_op => {
                // Check if "ALL" is present in the union_op text.
                let text = child.as_str().to_uppercase();
                if !text.contains("ALL") {
                    all = false;
                }
            }
            _ => {}
        }
    }

    if statements.len() == 1 {
        Ok(statements.into_iter().next().unwrap())
    } else {
        Ok(Statement::Union { statements, all })
    }
}

fn parse_single_stmt(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Statement> {
    match pair.as_rule() {
        Rule::explain_stmt => parse_explain(pair),
        Rule::match_stmt => parse_match(pair).map(Statement::Match),
        Rule::create_stmt => parse_create(pair).map(Statement::Create),
        Rule::match_create_stmt => parse_match_create(pair).map(Statement::MatchCreate),
        Rule::match_merge_stmt => parse_match_merge(pair).map(Statement::MatchMerge),
        Rule::delete_stmt => parse_delete(pair).map(Statement::Delete),
        Rule::set_stmt => parse_set(pair).map(Statement::Set),
        Rule::merge_stmt => parse_merge(pair).map(Statement::Merge),
        Rule::unwind_stmt => parse_unwind(pair).map(Statement::Unwind),
        _ => Err(GraphError::Serialization(format!(
            "unexpected rule: {:?}",
            pair.as_rule()
        ))),
    }
}

fn parse_explain(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Statement> {
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| GraphError::Serialization("EXPLAIN requires a statement".to_string()))?;
    let stmt = match inner.as_rule() {
        Rule::match_stmt => parse_match(inner).map(Statement::Match)?,
        Rule::match_create_stmt => parse_match_create(inner).map(Statement::MatchCreate)?,
        Rule::unwind_stmt => parse_unwind(inner).map(Statement::Unwind)?,
        _ => {
            return Err(GraphError::Serialization(format!(
                "EXPLAIN not supported for {:?}",
                inner.as_rule()
            )))
        }
    };
    Ok(Statement::Explain(Box::new(stmt)))
}

fn parse_match(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<MatchStatement> {
    let mut patterns = Vec::new();
    let mut optional_patterns = Vec::new();
    let mut where_clause = None;
    let mut intermediate_clauses = Vec::new();
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::optional_match_clause => {
                // Each OPTIONAL MATCH clause has its own pattern_list.
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::pattern_list {
                        optional_patterns.push(parse_pattern_list(child)?);
                    }
                }
            }
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
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
            .ok_or_else(|| GraphError::Serialization("missing RETURN clause".to_string()))?,
        order_by,
        skip,
        limit,
    })
}

fn parse_create(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<CreateStatement> {
    let mut patterns = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::create_pattern_list {
            for pat in inner.into_inner() {
                if pat.as_rule() == Rule::create_pattern {
                    patterns.push(parse_pattern_inner(pat)?);
                }
            }
        }
    }
    Ok(CreateStatement { patterns })
}

fn parse_match_create(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MatchCreateStatement> {
    let mut patterns = Vec::new();
    let mut where_clause = None;
    let mut create_patterns = Vec::new();

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
            _ => {}
        }
    }

    Ok(MatchCreateStatement {
        patterns,
        where_clause,
        create_patterns,
    })
}

fn parse_match_merge(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<MatchMergeStatement> {
    let mut patterns = Vec::new();
    let mut where_clause = None;
    let mut merge_pattern = None;
    let mut on_create = Vec::new();
    let mut on_match = Vec::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            Rule::pattern => merge_pattern = Some(parse_pattern(inner)?),
            Rule::on_create_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        on_create = parse_assignment_list(child)?;
                    }
                }
            }
            Rule::on_match_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        on_match = parse_assignment_list(child)?;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(MatchMergeStatement {
        patterns,
        where_clause,
        merge_pattern: merge_pattern
            .ok_or_else(|| GraphError::Serialization("missing MERGE pattern".to_string()))?,
        on_create,
        on_match,
    })
}

fn parse_delete(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<DeleteStatement> {
    let mut patterns = Vec::new();
    let mut optional_patterns = Vec::new();
    let mut where_clause = None;
    let mut detach = false;
    let mut variables = Vec::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::optional_match_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::pattern_list {
                        optional_patterns.push(parse_pattern_list(child)?);
                    }
                }
            }
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            Rule::detach_keyword => detach = true,
            Rule::ident_list => {
                for id in inner.into_inner() {
                    if id.as_rule() == Rule::ident {
                        variables.push(id.as_str().to_string());
                    }
                }
            }
            _ => {}
        }
    }

    Ok(DeleteStatement {
        patterns,
        optional_patterns,
        where_clause,
        detach,
        variables,
    })
}

fn parse_set(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<SetStatement> {
    let mut patterns = Vec::new();
    let mut where_clause = None;
    let mut assignments = Vec::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            Rule::assignment_list => assignments = parse_assignment_list(inner)?,
            _ => {}
        }
    }

    Ok(SetStatement {
        patterns,
        where_clause,
        assignments,
    })
}

fn parse_merge(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<MergeStatement> {
    let mut pattern = None;
    let mut on_create = Vec::new();
    let mut on_match = Vec::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern => pattern = Some(parse_pattern(inner)?),
            Rule::on_create_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        on_create = parse_assignment_list(child)?;
                    }
                }
            }
            Rule::on_match_clause => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::assignment_list {
                        on_match = parse_assignment_list(child)?;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(MergeStatement {
        pattern: pattern
            .ok_or_else(|| GraphError::Serialization("missing MERGE pattern".to_string()))?,
        on_create,
        on_match,
    })
}

// === Pattern parsing ===

fn parse_pattern_list(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Vec<Pattern>> {
    pair.into_inner()
        .filter(|p| matches!(p.as_rule(), Rule::pattern_item))
        .map(parse_pattern_item)
        .collect()
}

fn parse_pattern_item(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Pattern> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::path_pattern => parse_path_pattern(inner),
        Rule::pattern => parse_pattern(inner),
        _ => Err(GraphError::Serialization(format!(
            "unexpected pattern item: {:?}",
            inner.as_rule()
        ))),
    }
}

fn parse_path_pattern(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Pattern> {
    let mut path_variable = None;
    let mut pattern = None;
    let mut mode = ShortestPathMode::None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => path_variable = Some(inner.as_str().to_string()),
            Rule::shortest_path_fn => {
                mode = ShortestPathMode::Single;
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::pattern {
                        pattern = Some(parse_pattern(child)?);
                    }
                }
            }
            Rule::all_shortest_paths_fn => {
                mode = ShortestPathMode::All;
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::pattern {
                        pattern = Some(parse_pattern(child)?);
                    }
                }
            }
            _ => {}
        }
    }

    let mut pat = pattern.ok_or_else(|| {
        GraphError::Serialization("missing pattern in path assignment".to_string())
    })?;
    pat.path_variable = path_variable;
    pat.shortest_path_mode = mode;
    Ok(pat)
}

fn parse_pattern(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Pattern> {
    parse_pattern_inner(pair)
}

fn parse_pattern_inner(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Pattern> {
    let mut elements = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::node_pattern => elements.push(PatternElement::Node(parse_node_pattern(inner)?)),
            Rule::rel_pattern => {
                elements.push(PatternElement::Relationship(parse_rel_pattern(inner)?))
            }
            _ => {}
        }
    }
    Ok(Pattern {
        elements,
        path_variable: None,
        shortest_path_mode: ShortestPathMode::None,
    })
}

fn parse_node_pattern(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<NodePattern> {
    let mut variable = None;
    let mut labels = Vec::new();
    let mut properties = HashMap::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => variable = Some(inner.as_str().to_string()),
            Rule::label_spec => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::ident {
                        labels.push(child.as_str().to_string());
                    }
                }
            }
            Rule::property_map => properties = parse_property_map(inner)?,
            _ => {}
        }
    }

    Ok(NodePattern {
        variable,
        labels,
        properties,
    })
}

fn parse_rel_pattern(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<RelPattern> {
    let inner = pair.into_inner().next().unwrap();
    let direction = match inner.as_rule() {
        Rule::rel_right => RelDirection::Outgoing,
        Rule::rel_left => RelDirection::Incoming,
        Rule::rel_undirected => RelDirection::Undirected,
        _ => unreachable!(),
    };

    let mut variable = None;
    let mut rel_types = Vec::new();
    let mut properties = HashMap::new();
    let mut var_length = None;

    for child in inner.into_inner() {
        if child.as_rule() == Rule::rel_detail {
            for detail in child.into_inner() {
                match detail.as_rule() {
                    Rule::ident => variable = Some(detail.as_str().to_string()),
                    Rule::rel_type_spec => {
                        for rt in detail.into_inner() {
                            if rt.as_rule() == Rule::ident {
                                rel_types.push(rt.as_str().to_string());
                            }
                        }
                    }
                    Rule::property_map => {
                        properties = parse_property_map(detail)?;
                    }
                    Rule::var_length => {
                        var_length = Some(parse_var_length(detail)?);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(RelPattern {
        variable,
        rel_types,
        properties,
        direction,
        var_length,
    })
}

fn parse_var_length(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<(u32, u32)> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::int_range {
            let nums: Vec<u32> = inner
                .into_inner()
                .filter(|p| p.as_rule() == Rule::integer)
                .map(|p| p.as_str().parse::<u32>().unwrap())
                .collect();
            if nums.len() == 2 {
                return Ok((nums[0], nums[1]));
            } else if nums.len() == 1 {
                // Open-ended range like *1.. — use default max traversal depth.
                return Ok((nums[0], 15));
            }
        }
    }
    // Bare * with no range — default to 1..15 to prevent unbounded traversal.
    Ok((1, 15))
}

fn parse_property_map(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<HashMap<String, Expr>> {
    let mut map = HashMap::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::property_pair {
            let mut children = inner.into_inner();
            let key = children.next().unwrap().as_str().to_string();
            let value = parse_expr(children.next().unwrap())?;
            map.insert(key, value);
        }
    }
    Ok(map)
}

// === Clause parsing ===

fn parse_where(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let bool_expr = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::bool_expr)
        .unwrap();
    parse_bool_expr(bool_expr)
}

fn parse_with(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<WithClause> {
    let mut items = Vec::new();
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;
    let mut where_clause = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::return_items => {
                items = inner
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::return_item)
                    .map(|p| {
                        let mut expr = None;
                        let mut alias = None;
                        for child in p.into_inner() {
                            match child.as_rule() {
                                Rule::expr => expr = Some(parse_expr(child).unwrap()),
                                Rule::alias => {
                                    for a in child.into_inner() {
                                        if a.as_rule() == Rule::ident {
                                            alias = Some(a.as_str().to_string());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        ReturnItem {
                            expr: expr.unwrap(),
                            alias,
                        }
                    })
                    .collect();
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
        order_by,
        skip,
        limit,
        where_clause,
    })
}

fn parse_unwind(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<UnwindStatement> {
    let mut expr = None;
    let mut alias = None;
    let mut body = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => expr = Some(parse_expr(inner)?),
            Rule::ident => alias = Some(inner.as_str().to_string()),
            Rule::unwind_return => {
                let mut where_clause = None;
                let mut return_clause = None;
                let mut order_by = Vec::new();
                let mut skip = None;
                let mut limit = None;
                for child in inner.into_inner() {
                    match child.as_rule() {
                        Rule::where_clause => where_clause = Some(parse_where(child)?),
                        Rule::return_clause => return_clause = Some(parse_return(child)?),
                        Rule::order_by_clause => order_by = parse_order_by(child)?,
                        Rule::skip_clause => skip = Some(parse_skip(child)?),
                        Rule::limit_clause => limit = Some(parse_limit(child)?),
                        _ => {}
                    }
                }
                body = Some(UnwindBody::Return {
                    where_clause,
                    return_clause: return_clause.ok_or_else(|| {
                        GraphError::Serialization("missing RETURN clause in UNWIND".to_string())
                    })?,
                    order_by,
                    skip,
                    limit,
                });
            }
            Rule::unwind_create => {
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
                body = Some(UnwindBody::Create { patterns });
            }
            _ => {}
        }
    }

    Ok(UnwindStatement {
        expr: expr
            .ok_or_else(|| GraphError::Serialization("missing UNWIND expression".to_string()))?,
        alias: alias
            .ok_or_else(|| GraphError::Serialization("missing UNWIND alias".to_string()))?,
        body: body.ok_or_else(|| GraphError::Serialization("missing UNWIND body".to_string()))?,
    })
}

fn parse_unwind_clause(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<UnwindClause> {
    let mut expr = None;
    let mut alias = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => expr = Some(parse_expr(inner)?),
            Rule::ident => alias = Some(inner.as_str().to_string()),
            _ => {}
        }
    }

    Ok(UnwindClause {
        expr: expr
            .ok_or_else(|| GraphError::Serialization("missing UNWIND expression".to_string()))?,
        alias: alias
            .ok_or_else(|| GraphError::Serialization("missing UNWIND alias".to_string()))?,
    })
}

fn parse_return(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<ReturnClause> {
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
                    Rule::expr => expr = Some(parse_expr(inner).unwrap()),
                    Rule::alias => {
                        for child in inner.into_inner() {
                            if child.as_rule() == Rule::ident {
                                alias = Some(child.as_str().to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            ReturnItem {
                expr: expr.unwrap(),
                alias,
            }
        })
        .collect();

    Ok(ReturnClause { items, distinct })
}

fn parse_order_by(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Vec<SortItem>> {
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
                        descending = inner.as_str().to_uppercase() == "DESC";
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

fn parse_skip(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<u64> {
    let int_str = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::integer)
        .unwrap()
        .as_str();
    int_str
        .parse()
        .map_err(|e| GraphError::Serialization(format!("invalid skip: {e}")))
}

fn parse_limit(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<u64> {
    let int_str = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::integer)
        .unwrap()
        .as_str();
    int_str
        .parse()
        .map_err(|e| GraphError::Serialization(format!("invalid limit: {e}")))
}

fn parse_assignment_list(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Vec<Assignment>> {
    pair.into_inner()
        .filter(|p| p.as_rule() == Rule::assignment)
        .map(|p| {
            let mut children = p.into_inner();
            let prop_access = children.next().unwrap();
            let mut prop_parts = prop_access.into_inner();
            let variable = prop_parts.next().unwrap().as_str().to_string();
            let property = prop_parts.next().unwrap().as_str().to_string();
            let value = parse_expr(children.next().unwrap())?;
            Ok(Assignment {
                variable,
                property,
                value,
            })
        })
        .collect()
}

// === Expression parsing ===

fn parse_bool_expr(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    // bool_expr = { bool_term ~ (or_op ~ bool_term)* }
    let mut children: Vec<pest::iterators::Pair<Rule>> = pair.into_inner().collect();

    if children.len() == 1 {
        return parse_bool_term(children.remove(0));
    }

    // Left-associative OR chain.
    let mut left = parse_bool_term(children.remove(0))?;
    let mut i = 0;
    while i < children.len() {
        if children[i].as_rule() == Rule::or_op {
            i += 1;
            let right = parse_bool_term(children.remove(i))?;
            children.remove(i - 1); // remove or_op
            i -= 1;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::Or,
                right: Box::new(right),
            };
        } else {
            i += 1;
        }
    }
    Ok(left)
}

fn parse_bool_term(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
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
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::And,
                right: Box::new(right),
            };
        } else {
            i += 1;
        }
    }
    Ok(left)
}

fn parse_bool_factor(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    // bool_factor = { not_op? ~ bool_primary }
    let mut negated = false;
    let mut primary = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::not_op => negated = true,
            Rule::bool_primary => primary = Some(inner),
            _ => {}
        }
    }

    let expr = parse_bool_primary(primary.unwrap())?;
    if negated {
        Ok(Expr::Not(Box::new(expr)))
    } else {
        Ok(expr)
    }
}

fn parse_bool_primary(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    // bool_primary = { is_null_check | comparison | "(" ~ bool_expr ~ ")" }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::case_expr => parse_case_expr(inner),
        Rule::exists_subquery => parse_exists_subquery(inner),
        Rule::is_null_check => parse_is_null_check(inner, false),
        Rule::is_not_null_check => parse_is_null_check(inner, true),
        Rule::in_check => parse_in_check(inner),
        Rule::comparison => parse_comparison(inner),
        Rule::bool_expr => parse_bool_expr(inner),
        _ => Err(GraphError::Serialization(format!(
            "unexpected bool primary: {:?}",
            inner.as_rule()
        ))),
    }
}

fn parse_in_check(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let mut children = pair.into_inner();
    let left = parse_expr(children.next().unwrap())?;
    let right = parse_expr(children.next().unwrap())?;
    Ok(Expr::BinaryOp {
        left: Box::new(left),
        op: BinOp::In,
        right: Box::new(right),
    })
}

fn parse_case_expr(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let mut alternatives = Vec::new();
    let mut default = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::case_when_clause => {
                let mut children = inner.into_inner();
                let condition = parse_bool_expr(children.next().unwrap())?;
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

    Ok(Expr::Case {
        alternatives,
        default,
    })
}

fn parse_exists_subquery(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
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
        return Err(GraphError::Serialization(
            "EXISTS subquery requires at least one pattern".to_string(),
        ));
    }

    Ok(Expr::Exists {
        patterns,
        where_clause,
    })
}

fn parse_is_null_check(
    pair: pest::iterators::Pair<Rule>,
    negated: bool,
) -> crate::types::Result<Expr> {
    let expr = parse_expr(pair.into_inner().next().unwrap())?;
    if negated {
        Ok(Expr::IsNotNull(Box::new(expr)))
    } else {
        Ok(Expr::IsNull(Box::new(expr)))
    }
}

fn parse_comparison(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let mut children = pair.into_inner();
    let left = parse_expr(children.next().unwrap())?;
    let op_pair = children.next().unwrap();
    let right = parse_expr(children.next().unwrap())?;

    let op = parse_comp_op(op_pair)?;

    Ok(Expr::BinaryOp {
        left: Box::new(left),
        op,
        right: Box::new(right),
    })
}

fn parse_comp_op(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<BinOp> {
    let text = pair.as_str().trim();
    // Check for multi-word operators via sub-rules first.
    if let Some(sub) = pair.into_inner().next() {
        return match sub.as_rule() {
            Rule::starts_with_op => Ok(BinOp::StartsWith),
            Rule::ends_with_op => Ok(BinOp::EndsWith),
            Rule::contains_op => Ok(BinOp::Contains),
            _ => Err(GraphError::Serialization(format!(
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
        _ => Err(GraphError::Serialization(format!(
            "unknown comparison operator: {text}"
        ))),
    }
}

fn parse_expr(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    // expr = { add_expr }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::add_expr => parse_add_expr(inner),
        // Fallback for cases where expr directly contains an atom.
        _ => parse_atom_expr(inner),
    }
}

fn parse_add_expr(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
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
                return Err(GraphError::Serialization(format!(
                    "unexpected add op: {}",
                    op_pair.as_str()
                )))
            }
        };
        let right = parse_mul_expr(iter.next().unwrap())?;
        left = Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_mul_expr(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let mut children: Vec<_> = pair.into_inner().collect();
    if children.len() == 1 {
        return parse_atom_expr(children.remove(0));
    }
    let mut iter = children.into_iter();
    let mut left = parse_atom_expr(iter.next().unwrap())?;
    while let Some(op_pair) = iter.next() {
        let op = match op_pair.as_str() {
            "*" => BinOp::Mul,
            "/" => BinOp::Div,
            _ => {
                return Err(GraphError::Serialization(format!(
                    "unexpected mul op: {}",
                    op_pair.as_str()
                )))
            }
        };
        let right = parse_atom_expr(iter.next().unwrap())?;
        left = Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_atom_expr(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    match pair.as_rule() {
        Rule::atom_expr => {
            let inner = pair.into_inner().next().unwrap();
            parse_atom_expr(inner)
        }
        Rule::case_expr => parse_case_expr(pair),
        Rule::function_call => parse_function_call(pair),
        Rule::property_access => {
            let mut parts = pair.into_inner();
            let var = parts.next().unwrap().as_str().to_string();
            let prop = parts.next().unwrap().as_str().to_string();
            Ok(Expr::Property(var, prop))
        }
        Rule::literal => parse_literal(pair),
        Rule::list_comprehension => parse_list_comprehension(pair),
        Rule::list_literal => {
            let items: crate::types::Result<Vec<Expr>> = pair
                .into_inner()
                .filter(|p| p.as_rule() == Rule::expr)
                .map(parse_expr)
                .collect();
            Ok(Expr::List(items?))
        }
        Rule::map_literal => {
            let mut pairs = Vec::new();
            for p in pair.into_inner() {
                if p.as_rule() == Rule::map_pair {
                    let mut parts = p.into_inner();
                    let key = parts.next().unwrap().as_str().to_string();
                    let value_pair = parts.next().unwrap();
                    let value = parse_expr(value_pair)?;
                    pairs.push((key, value));
                }
            }
            Ok(Expr::MapLiteral(pairs))
        }
        Rule::star => Ok(Expr::Star),
        Rule::parameter => {
            let name = pair.into_inner().next().unwrap().as_str().to_string();
            Ok(Expr::Parameter(name))
        }
        Rule::variable => Ok(Expr::Variable(
            pair.into_inner().next().unwrap().as_str().to_string(),
        )),
        Rule::add_expr => parse_add_expr(pair),
        Rule::mul_expr => parse_mul_expr(pair),
        _ => Err(GraphError::Serialization(format!(
            "unexpected expr: {:?}",
            pair.as_rule()
        ))),
    }
}

fn parse_function_call(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let mut name = String::new();
    let mut args = Vec::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::function_name => name = inner.as_str().to_lowercase(),
            Rule::function_args => {
                for arg in inner.into_inner() {
                    match arg.as_rule() {
                        Rule::star => args.push(Expr::Star),
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

    Ok(Expr::FunctionCall { name, args })
}

fn parse_literal(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::integer_literal => {
            let n: i64 = inner
                .as_str()
                .parse()
                .map_err(|e| GraphError::Serialization(format!("invalid integer: {e}")))?;
            Ok(Expr::Literal(LiteralValue::I64(n)))
        }
        Rule::float_literal => {
            let n: f64 = inner
                .as_str()
                .parse()
                .map_err(|e| GraphError::Serialization(format!("invalid float: {e}")))?;
            Ok(Expr::Literal(LiteralValue::F64(n)))
        }
        Rule::string_literal => {
            let raw = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::string_inner)
                .unwrap()
                .as_str();
            Ok(Expr::Literal(LiteralValue::String(unescape_string(raw))))
        }
        Rule::bool_literal => {
            let b = inner.as_str().to_uppercase() == "TRUE";
            Ok(Expr::Literal(LiteralValue::Bool(b)))
        }
        Rule::null_literal => Ok(Expr::Literal(LiteralValue::Null)),
        _ => Err(GraphError::Serialization(format!(
            "unexpected literal: {:?}",
            inner.as_rule()
        ))),
    }
}

fn parse_list_comprehension(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let mut variable = None;
    let mut list_expr = None;
    let mut filter = None;
    let mut map_expr = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => variable = Some(inner.as_str().to_string()),
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

    Ok(Expr::ListComprehension {
        variable: variable.ok_or_else(|| {
            GraphError::Serialization("missing variable in list comprehension".to_string())
        })?,
        list_expr: list_expr.ok_or_else(|| {
            GraphError::Serialization("missing list expression in list comprehension".to_string())
        })?,
        filter,
        map_expr,
    })
}

// === Error humanization ===

/// Map pest grammar rule names to user-friendly descriptions.
fn humanize_rule_name(rule: &str) -> &str {
    match rule {
        "statement" | "union_stmt" | "single_stmt" | "explain_stmt" => {
            "a Cypher statement (MATCH, CREATE, DELETE, MERGE, EXPLAIN ...)"
        }
        "union_op" => "UNION or UNION ALL",
        "expr" | "add_expr" | "mul_expr" | "atom_expr" => {
            "an expression (property, literal, or function call)"
        }
        "bool_expr" | "bool_primary" | "bool_factor" | "bool_term" => "a condition",
        "comparison" => "a comparison (=, <>, <, >, <=, >=)",
        "ident" => "an identifier",
        "pattern" | "pattern_list" | "pattern_item" => "a graph pattern like (n:Label)",
        "path_pattern" | "shortest_path_fn" | "all_shortest_paths_fn" => {
            "a path pattern like p = shortestPath((a)-[*]->(b))"
        }
        "node_pattern" => "a node pattern like (n:Label {prop: value})",
        "rel_pattern" | "rel_right" | "rel_left" | "rel_undirected" => {
            "a relationship pattern like -[:TYPE]->"
        }
        "distinct_keyword" => "DISTINCT",
        "return_clause" => "a RETURN clause",
        "return_items" | "return_item" => "a RETURN expression",
        "where_clause" => "a WHERE clause",
        "with_clause" => "a WITH clause",
        "order_by_clause" => "an ORDER BY clause",
        "skip_clause" => "a SKIP clause",
        "limit_clause" => "a LIMIT clause",
        "literal" => "a value (string, number, boolean, or null)",
        "integer_literal" | "integer" => "an integer",
        "float_literal" => "a number",
        "string_literal" => "a string (e.g. 'hello')",
        "property_access" => "a property access like n.name",
        "property_map" => "a property map like {name: 'Alice'}",
        "label_spec" => "a label like :Person",
        "function_call" => "a function call like count(*)",
        "case_when_clause" => "a WHEN condition",
        "case_else_clause" => "an ELSE value",
        "case_expr" => "a CASE expression",
        "exists_subquery" => "an EXISTS { } subquery",
        "list_comprehension" => "a list comprehension like [x IN list | expr]",
        "comp_op" => "a comparison operator (=, <>, <, >)",
        "alias" => "an alias (AS name)",
        "assignment" | "assignment_list" => "a property assignment like n.prop = value",
        "detach_keyword" => "DETACH",
        "EOI" => "end of query",
        _ => rule,
    }
}

/// Convert a pest parse error into a human-friendly error message.
fn humanize_pest_error(err: pest::error::Error<Rule>) -> String {
    let renamed = err.renamed_rules(|rule| humanize_rule_name(&format!("{rule:?}")).to_string());
    format!("{renamed}")
}

// ---------------------------------------------------------------------------
// Parameter resolution: substitute $name with literal values before planning.
// ---------------------------------------------------------------------------

use crate::types::Value;

fn value_to_literal(val: &Value) -> crate::types::Result<LiteralValue> {
    match val {
        Value::Null => Ok(LiteralValue::Null),
        Value::Bool(b) => Ok(LiteralValue::Bool(*b)),
        Value::I64(n) => Ok(LiteralValue::I64(*n)),
        Value::F64(n) => Ok(LiteralValue::F64(*n)),
        Value::String(s) => Ok(LiteralValue::String(s.clone())),
        _ => Err(GraphError::ParseError(
            "unsupported parameter type (only scalar values allowed)".to_string(),
        )),
    }
}

fn resolve_expr(expr: &Expr, params: &HashMap<String, Value>) -> crate::types::Result<Expr> {
    match expr {
        Expr::Parameter(name) => {
            let val = params
                .get(name)
                .ok_or_else(|| GraphError::ParseError(format!("missing parameter: ${name}")))?;
            Ok(Expr::Literal(value_to_literal(val)?))
        }
        Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
            left: Box::new(resolve_expr(left, params)?),
            op: *op,
            right: Box::new(resolve_expr(right, params)?),
        }),
        Expr::Not(inner) => Ok(Expr::Not(Box::new(resolve_expr(inner, params)?))),
        Expr::IsNull(inner) => Ok(Expr::IsNull(Box::new(resolve_expr(inner, params)?))),
        Expr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(resolve_expr(inner, params)?))),
        Expr::FunctionCall { name, args } => {
            let resolved: crate::types::Result<Vec<Expr>> =
                args.iter().map(|a| resolve_expr(a, params)).collect();
            Ok(Expr::FunctionCall {
                name: name.clone(),
                args: resolved?,
            })
        }
        Expr::Case {
            alternatives,
            default,
        } => {
            let mut resolved_alts = Vec::new();
            for (cond, result) in alternatives {
                resolved_alts.push((
                    Box::new(resolve_expr(cond, params)?),
                    Box::new(resolve_expr(result, params)?),
                ));
            }
            let resolved_default = default
                .as_ref()
                .map(|d| resolve_expr(d, params).map(Box::new))
                .transpose()?;
            Ok(Expr::Case {
                alternatives: resolved_alts,
                default: resolved_default,
            })
        }
        Expr::List(items) => {
            let resolved: crate::types::Result<Vec<Expr>> =
                items.iter().map(|e| resolve_expr(e, params)).collect();
            Ok(Expr::List(resolved?))
        }
        Expr::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => Ok(Expr::ListComprehension {
            variable: variable.clone(),
            list_expr: Box::new(resolve_expr(list_expr, params)?),
            filter: filter
                .as_ref()
                .map(|f| resolve_expr(f, params).map(Box::new))
                .transpose()?,
            map_expr: map_expr
                .as_ref()
                .map(|m| resolve_expr(m, params).map(Box::new))
                .transpose()?,
        }),
        Expr::Exists {
            patterns,
            where_clause,
        } => Ok(Expr::Exists {
            patterns: resolve_patterns(patterns, params)?,
            where_clause: where_clause
                .as_ref()
                .map(|w| resolve_expr(w, params).map(Box::new))
                .transpose()?,
        }),
        Expr::MapLiteral(pairs) => {
            let resolved: crate::types::Result<Vec<(String, Expr)>> = pairs
                .iter()
                .map(|(k, v)| resolve_expr(v, params).map(|r| (k.clone(), r)))
                .collect();
            Ok(Expr::MapLiteral(resolved?))
        }
        // Leaf nodes that contain no sub-expressions.
        Expr::Literal(_) | Expr::Property(_, _) | Expr::Variable(_) | Expr::Star => {
            Ok(expr.clone())
        }
    }
}

fn resolve_props(
    properties: &HashMap<String, Expr>,
    params: &HashMap<String, Value>,
) -> crate::types::Result<HashMap<String, Expr>> {
    properties
        .iter()
        .map(|(k, v)| Ok((k.clone(), resolve_expr(v, params)?)))
        .collect()
}

fn resolve_pattern(
    pattern: &Pattern,
    params: &HashMap<String, Value>,
) -> crate::types::Result<Pattern> {
    let elements: crate::types::Result<Vec<PatternElement>> = pattern
        .elements
        .iter()
        .map(|el| match el {
            PatternElement::Node(n) => Ok(PatternElement::Node(NodePattern {
                variable: n.variable.clone(),
                labels: n.labels.clone(),
                properties: resolve_props(&n.properties, params)?,
            })),
            PatternElement::Relationship(r) => Ok(PatternElement::Relationship(RelPattern {
                variable: r.variable.clone(),
                rel_types: r.rel_types.clone(),
                properties: resolve_props(&r.properties, params)?,
                direction: r.direction,
                var_length: r.var_length,
            })),
        })
        .collect();
    Ok(Pattern {
        elements: elements?,
        path_variable: pattern.path_variable.clone(),
        shortest_path_mode: pattern.shortest_path_mode,
    })
}

fn resolve_patterns(
    patterns: &[Pattern],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<Pattern>> {
    patterns
        .iter()
        .map(|p| resolve_pattern(p, params))
        .collect()
}

fn resolve_assignments(
    assignments: &[Assignment],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<Assignment>> {
    assignments
        .iter()
        .map(|a| {
            Ok(Assignment {
                variable: a.variable.clone(),
                property: a.property.clone(),
                value: resolve_expr(&a.value, params)?,
            })
        })
        .collect()
}

fn resolve_return_items(
    items: &[ReturnItem],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<ReturnItem>> {
    items
        .iter()
        .map(|item| {
            Ok(ReturnItem {
                expr: resolve_expr(&item.expr, params)?,
                alias: item.alias.clone(),
            })
        })
        .collect()
}

fn resolve_sort_items(
    items: &[SortItem],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<SortItem>> {
    items
        .iter()
        .map(|item| {
            Ok(SortItem {
                expr: resolve_expr(&item.expr, params)?,
                descending: item.descending,
            })
        })
        .collect()
}

fn resolve_intermediate_clauses(
    clauses: &[IntermediateClause],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<IntermediateClause>> {
    clauses
        .iter()
        .map(|c| match c {
            IntermediateClause::With(w) => Ok(IntermediateClause::With(WithClause {
                items: resolve_return_items(&w.items, params)?,
                order_by: w.order_by.clone(),
                skip: w.skip,
                limit: w.limit,
                where_clause: w
                    .where_clause
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
            })),
            IntermediateClause::Unwind(u) => Ok(IntermediateClause::Unwind(UnwindClause {
                expr: resolve_expr(&u.expr, params)?,
                alias: u.alias.clone(),
            })),
        })
        .collect()
}

/// Substitute all `$name` parameters in a parsed statement with literal values.
///
/// This must be called before planning so the planner can use literal values
/// for index selection decisions.
pub fn resolve_params(
    stmt: &Statement,
    params: &HashMap<String, Value>,
) -> crate::types::Result<Statement> {
    match stmt {
        Statement::Match(m) => Ok(Statement::Match(MatchStatement {
            patterns: resolve_patterns(&m.patterns, params)?,
            optional_patterns: m
                .optional_patterns
                .iter()
                .map(|ps| resolve_patterns(ps, params))
                .collect::<crate::types::Result<Vec<_>>>()?,
            where_clause: m
                .where_clause
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            intermediate_clauses: resolve_intermediate_clauses(&m.intermediate_clauses, params)?,
            return_clause: ReturnClause {
                items: resolve_return_items(&m.return_clause.items, params)?,
                distinct: m.return_clause.distinct,
            },
            order_by: resolve_sort_items(&m.order_by, params)?,
            skip: m.skip,
            limit: m.limit,
        })),
        Statement::Create(c) => Ok(Statement::Create(CreateStatement {
            patterns: resolve_patterns(&c.patterns, params)?,
        })),
        Statement::MatchCreate(mc) => Ok(Statement::MatchCreate(MatchCreateStatement {
            patterns: resolve_patterns(&mc.patterns, params)?,
            where_clause: mc
                .where_clause
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            create_patterns: resolve_patterns(&mc.create_patterns, params)?,
        })),
        Statement::MatchMerge(mm) => Ok(Statement::MatchMerge(MatchMergeStatement {
            patterns: resolve_patterns(&mm.patterns, params)?,
            where_clause: mm
                .where_clause
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            merge_pattern: resolve_pattern(&mm.merge_pattern, params)?,
            on_create: resolve_assignments(&mm.on_create, params)?,
            on_match: resolve_assignments(&mm.on_match, params)?,
        })),
        Statement::Delete(d) => Ok(Statement::Delete(DeleteStatement {
            patterns: resolve_patterns(&d.patterns, params)?,
            optional_patterns: d
                .optional_patterns
                .iter()
                .map(|ps| resolve_patterns(ps, params))
                .collect::<crate::types::Result<Vec<_>>>()?,
            where_clause: d
                .where_clause
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            detach: d.detach,
            variables: d.variables.clone(),
        })),
        Statement::Set(s) => Ok(Statement::Set(SetStatement {
            patterns: resolve_patterns(&s.patterns, params)?,
            where_clause: s
                .where_clause
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            assignments: resolve_assignments(&s.assignments, params)?,
        })),
        Statement::Merge(m) => Ok(Statement::Merge(MergeStatement {
            pattern: resolve_pattern(&m.pattern, params)?,
            on_create: resolve_assignments(&m.on_create, params)?,
            on_match: resolve_assignments(&m.on_match, params)?,
        })),
        Statement::Unwind(u) => {
            let body = match &u.body {
                UnwindBody::Return {
                    where_clause,
                    return_clause,
                    order_by,
                    skip,
                    limit,
                } => UnwindBody::Return {
                    where_clause: where_clause
                        .as_ref()
                        .map(|e| resolve_expr(e, params))
                        .transpose()?,
                    return_clause: ReturnClause {
                        items: resolve_return_items(&return_clause.items, params)?,
                        distinct: return_clause.distinct,
                    },
                    order_by: resolve_sort_items(order_by, params)?,
                    skip: *skip,
                    limit: *limit,
                },
                UnwindBody::Create { patterns } => UnwindBody::Create {
                    patterns: resolve_patterns(patterns, params)?,
                },
            };
            Ok(Statement::Unwind(UnwindStatement {
                expr: resolve_expr(&u.expr, params)?,
                alias: u.alias.clone(),
                body,
            }))
        }
        Statement::Explain(inner) => {
            Ok(Statement::Explain(Box::new(resolve_params(inner, params)?)))
        }
        Statement::Union { statements, all } => {
            let resolved: crate::types::Result<Vec<Statement>> = statements
                .iter()
                .map(|s| resolve_params(s, params))
                .collect();
            Ok(Statement::Union {
                statements: resolved?,
                all: *all,
            })
        }
    }
}
