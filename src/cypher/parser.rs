use std::collections::HashMap;

use pest::Parser;
use pest_derive::Parser;

use crate::cypher::ast::*;
use crate::types::GraphError;

#[derive(Parser)]
#[grammar = "cypher/grammar.pest"]
struct CypherParser;

/// Parse a Cypher query string into a Statement AST.
pub fn parse(input: &str) -> crate::types::Result<Statement> {
    let pairs = CypherParser::parse(Rule::statement, input)
        .map_err(|e| GraphError::Serialization(format!("parse error: {e}")))?;

    let statement_pair = pairs
        .into_iter()
        .next()
        .unwrap()
        .into_inner()
        .find(|p| {
            matches!(
                p.as_rule(),
                Rule::match_stmt
                    | Rule::create_stmt
                    | Rule::match_create_stmt
                    | Rule::delete_stmt
                    | Rule::set_stmt
                    | Rule::merge_stmt
            )
        })
        .ok_or_else(|| GraphError::Serialization("empty statement".to_string()))?;

    match statement_pair.as_rule() {
        Rule::match_stmt => parse_match(statement_pair).map(Statement::Match),
        Rule::create_stmt => parse_create(statement_pair).map(Statement::Create),
        Rule::match_create_stmt => parse_match_create(statement_pair).map(Statement::MatchCreate),
        Rule::delete_stmt => parse_delete(statement_pair).map(Statement::Delete),
        Rule::set_stmt => parse_set(statement_pair).map(Statement::Set),
        Rule::merge_stmt => parse_merge(statement_pair).map(Statement::Merge),
        _ => Err(GraphError::Serialization(format!(
            "unexpected rule: {:?}",
            statement_pair.as_rule()
        ))),
    }
}

fn parse_match(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<MatchStatement> {
    let mut patterns = Vec::new();
    let mut where_clause = None;
    let mut return_clause = None;
    let mut order_by = Vec::new();
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
            Rule::return_clause => return_clause = Some(parse_return(inner)?),
            Rule::order_by_clause => order_by = parse_order_by(inner)?,
            Rule::limit_clause => limit = Some(parse_limit(inner)?),
            _ => {}
        }
    }

    Ok(MatchStatement {
        patterns,
        where_clause,
        return_clause: return_clause
            .ok_or_else(|| GraphError::Serialization("missing RETURN clause".to_string()))?,
        order_by,
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

fn parse_delete(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<DeleteStatement> {
    let mut patterns = Vec::new();
    let mut where_clause = None;
    let mut variables = Vec::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern_list => patterns = parse_pattern_list(inner)?,
            Rule::where_clause => where_clause = Some(parse_where(inner)?),
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
        where_clause,
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

fn parse_pattern_list(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Vec<Pattern>> {
    pair.into_inner()
        .filter(|p| p.as_rule() == Rule::pattern)
        .map(parse_pattern)
        .collect()
}

fn parse_pattern(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Pattern> {
    parse_pattern_inner(pair)
}

fn parse_pattern_inner(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Pattern> {
    let mut elements = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::node_pattern => elements.push(PatternElement::Node(parse_node_pattern(inner)?)),
            Rule::rel_pattern => elements.push(PatternElement::Relationship(parse_rel_pattern(inner)?)),
            _ => {}
        }
    }
    Ok(Pattern { elements })
}

fn parse_node_pattern(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<NodePattern> {
    let mut variable = None;
    let mut label = None;
    let mut properties = HashMap::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => variable = Some(inner.as_str().to_string()),
            Rule::label_spec => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::ident {
                        label = Some(child.as_str().to_string());
                    }
                }
            }
            Rule::property_map => properties = parse_property_map(inner)?,
            _ => {}
        }
    }

    Ok(NodePattern {
        variable,
        label,
        properties,
    })
}

fn parse_rel_pattern(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<RelPattern> {
    let inner = pair.into_inner().next().unwrap();
    let direction = match inner.as_rule() {
        Rule::rel_right => RelDirection::Outgoing,
        Rule::rel_left => RelDirection::Incoming,
        Rule::rel_undirected => RelDirection::Undirected,
        _ => unreachable!(),
    };

    let mut variable = None;
    let mut rel_type = None;
    let mut var_length = None;

    for child in inner.into_inner() {
        match child.as_rule() {
            Rule::rel_detail => {
                for detail in child.into_inner() {
                    match detail.as_rule() {
                        Rule::ident => variable = Some(detail.as_str().to_string()),
                        Rule::rel_type_spec => {
                            for rt in detail.into_inner() {
                                if rt.as_rule() == Rule::ident {
                                    rel_type = Some(rt.as_str().to_string());
                                }
                            }
                        }
                        Rule::var_length => {
                            var_length = Some(parse_var_length(detail)?);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(RelPattern {
        variable,
        rel_type,
        direction,
        var_length,
    })
}

fn parse_var_length(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<(u32, u32)> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::int_range {
            let nums: Vec<u32> = inner
                .into_inner()
                .filter(|p| p.as_rule() == Rule::integer)
                .map(|p| p.as_str().parse::<u32>().unwrap())
                .collect();
            if nums.len() == 2 {
                return Ok((nums[0], nums[1]));
            }
        }
    }
    // Bare * with no range — default to 1..max
    Ok((1, u32::MAX))
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

fn parse_return(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<ReturnClause> {
    let items_pair = pair
        .into_inner()
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

    Ok(ReturnClause { items })
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
        Rule::is_null_check => parse_is_null_check(inner, false),
        Rule::is_not_null_check => parse_is_null_check(inner, true),
        Rule::comparison => parse_comparison(inner),
        Rule::bool_expr => parse_bool_expr(inner),
        _ => Err(GraphError::Serialization(format!(
            "unexpected bool primary: {:?}",
            inner.as_rule()
        ))),
    }
}

fn parse_is_null_check(pair: pest::iterators::Pair<Rule>, negated: bool) -> crate::types::Result<Expr> {
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
    // expr = { function_call | property_access | literal | star | variable }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::function_call => parse_function_call(inner),
        Rule::property_access => {
            let mut parts = inner.into_inner();
            let var = parts.next().unwrap().as_str().to_string();
            let prop = parts.next().unwrap().as_str().to_string();
            Ok(Expr::Property(var, prop))
        }
        Rule::literal => parse_literal(inner),
        Rule::star => Ok(Expr::Star),
        Rule::variable => Ok(Expr::Variable(
            inner.into_inner().next().unwrap().as_str().to_string(),
        )),
        _ => Err(GraphError::Serialization(format!(
            "unexpected expr: {:?}",
            inner.as_rule()
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
            let s = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::string_inner)
                .unwrap()
                .as_str()
                .to_string();
            Ok(Expr::Literal(LiteralValue::String(s)))
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
