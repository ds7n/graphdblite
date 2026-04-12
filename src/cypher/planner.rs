use crate::cypher::ast::*;
use crate::cypher::ir::*;
use crate::types::{Direction, GraphError};

/// Compile a Cypher AST Statement into a LogicalOp plan.
pub fn plan(stmt: &Statement) -> crate::types::Result<LogicalOp> {
    match stmt {
        Statement::Match(m) => plan_match(m),
        Statement::Create(c) => plan_create(c),
        Statement::Delete(d) => plan_delete(d),
        Statement::Set(s) => plan_set(s),
        Statement::Merge(m) => plan_merge(m),
    }
}

fn plan_match(stmt: &MatchStatement) -> crate::types::Result<LogicalOp> {
    // Build scan + expand chain from patterns.
    let mut op = plan_patterns(&stmt.patterns)?;

    // Apply WHERE filter.
    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    // Check if RETURN contains aggregates.
    let has_aggregates = stmt.return_clause.items.iter().any(|item| {
        matches!(item.expr, Expr::FunctionCall { .. })
    });

    if has_aggregates {
        let (group_keys, aggregates) = split_aggregates(&stmt.return_clause)?;
        op = LogicalOp::Aggregate {
            input: Box::new(op),
            group_keys,
            aggregates,
        };
    }

    // Project (RETURN).
    op = LogicalOp::Project {
        input: Box::new(op),
        items: stmt.return_clause.items.clone(),
    };

    // ORDER BY.
    if !stmt.order_by.is_empty() {
        op = LogicalOp::Sort {
            input: Box::new(op),
            items: stmt.order_by.clone(),
        };
    }

    // LIMIT.
    if let Some(count) = stmt.limit {
        op = LogicalOp::Limit {
            input: Box::new(op),
            count,
        };
    }

    Ok(op)
}

fn plan_create(stmt: &CreateStatement) -> crate::types::Result<LogicalOp> {
    let mut ops = Vec::new();

    for pattern in &stmt.patterns {
        let pattern_ops = plan_create_pattern(pattern)?;
        ops.extend(pattern_ops);
    }

    if ops.len() == 1 {
        Ok(ops.remove(0))
    } else {
        Ok(LogicalOp::CreateSequence { ops })
    }
}

fn plan_delete(stmt: &DeleteStatement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_patterns(&stmt.patterns)?;

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    Ok(LogicalOp::Delete {
        input: Box::new(op),
        variables: stmt.variables.clone(),
    })
}

fn plan_set(stmt: &SetStatement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_patterns(&stmt.patterns)?;

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    Ok(LogicalOp::SetProperty {
        input: Box::new(op),
        assignments: stmt.assignments.clone(),
    })
}

fn plan_merge(stmt: &MergeStatement) -> crate::types::Result<LogicalOp> {
    Ok(LogicalOp::Merge {
        pattern: stmt.pattern.clone(),
        on_create: stmt.on_create.clone(),
        on_match: stmt.on_match.clone(),
    })
}

/// Plan the scan/expand chain for a list of patterns.
fn plan_patterns(patterns: &[Pattern]) -> crate::types::Result<LogicalOp> {
    if patterns.is_empty() {
        return Ok(LogicalOp::EmptyRow);
    }

    let mut op = plan_single_pattern(&patterns[0])?;

    // Multiple patterns are joined (simplified: sequential execution).
    for pattern in &patterns[1..] {
        let right = plan_single_pattern(pattern)?;
        // For now, treat multiple patterns as chained — proper hash join in future.
        op = right;
    }

    Ok(op)
}

/// Plan a single pattern: (a:Label)-[:TYPE]->(b:Label)
fn plan_single_pattern(pattern: &Pattern) -> crate::types::Result<LogicalOp> {
    let mut op: Option<LogicalOp> = None;

    let mut i = 0;
    while i < pattern.elements.len() {
        match &pattern.elements[i] {
            PatternElement::Node(node) => {
                if op.is_none() {
                    // First node — start a scan.
                    let alias = node
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("_anon_{i}"));
                    let mut scan = LogicalOp::Scan {
                        label: node.label.clone().unwrap_or_default(),
                        alias,
                    };

                    // If node has inline property filters, add a Filter.
                    if !node.properties.is_empty() {
                        let predicate = properties_to_filter(
                            node.variable.as_deref().unwrap_or("_anon_0"),
                            &node.properties,
                        );
                        scan = LogicalOp::Filter {
                            input: Box::new(scan),
                            predicate,
                        };
                    }

                    op = Some(scan);
                }
                // Subsequent nodes after a relationship are handled in the rel branch.
                i += 1;
            }
            PatternElement::Relationship(rel) => {
                // Must have a next node.
                let dst_node = match pattern.elements.get(i + 1) {
                    Some(PatternElement::Node(n)) => n,
                    _ => {
                        return Err(GraphError::Serialization(
                            "relationship must be followed by a node pattern".to_string(),
                        ))
                    }
                };

                let src_alias = get_last_alias(&op);
                let dst_alias = dst_node
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("_anon_{}", i + 1));

                let direction = match rel.direction {
                    RelDirection::Outgoing => Direction::Outgoing,
                    RelDirection::Incoming => Direction::Incoming,
                    RelDirection::Undirected => Direction::Both,
                };

                let (min_hops, max_hops) = rel.var_length.unwrap_or((1, 1));

                op = Some(LogicalOp::Expand {
                    input: Box::new(op.unwrap()),
                    src_alias,
                    dst_alias,
                    edge_type: rel.rel_type.clone(),
                    direction,
                    min_hops,
                    max_hops,
                });

                i += 2; // skip rel + dst node
            }
        }
    }

    op.ok_or_else(|| GraphError::Serialization("empty pattern".to_string()))
}

/// Plan CREATE pattern into individual CreateNode/CreateEdge operations.
fn plan_create_pattern(pattern: &Pattern) -> crate::types::Result<Vec<LogicalOp>> {
    let mut ops = Vec::new();
    let mut last_alias: Option<String> = None;

    let mut i = 0;
    while i < pattern.elements.len() {
        match &pattern.elements[i] {
            PatternElement::Node(node) => {
                let alias = node.variable.clone();
                ops.push(LogicalOp::CreateNode {
                    label: node.label.clone(),
                    alias: alias.clone(),
                    properties: node.properties.clone(),
                });
                last_alias = alias;
                i += 1;
            }
            PatternElement::Relationship(rel) => {
                let dst_node = match pattern.elements.get(i + 1) {
                    Some(PatternElement::Node(n)) => n,
                    _ => {
                        return Err(GraphError::Serialization(
                            "relationship must be followed by a node".to_string(),
                        ))
                    }
                };

                let dst_alias = dst_node.variable.clone();
                ops.push(LogicalOp::CreateNode {
                    label: dst_node.label.clone(),
                    alias: dst_alias.clone(),
                    properties: dst_node.properties.clone(),
                });

                let src = last_alias
                    .clone()
                    .ok_or_else(|| {
                        GraphError::Serialization("edge without source node".to_string())
                    })?;
                let dst = dst_alias
                    .clone()
                    .ok_or_else(|| {
                        GraphError::Serialization("edge target must have a variable".to_string())
                    })?;

                ops.push(LogicalOp::CreateEdge {
                    src_alias: src,
                    dst_alias: dst,
                    edge_type: rel.rel_type.clone().unwrap_or_default(),
                    properties: std::collections::HashMap::new(),
                });

                last_alias = dst_alias;
                i += 2;
            }
        }
    }

    Ok(ops)
}

/// Convert inline property filters to an AND expression.
fn properties_to_filter(
    variable: &str,
    properties: &std::collections::HashMap<String, Expr>,
) -> Expr {
    let mut exprs: Vec<Expr> = properties
        .iter()
        .map(|(key, value)| Expr::BinaryOp {
            left: Box::new(Expr::Property(variable.to_string(), key.clone())),
            op: BinOp::Eq,
            right: Box::new(value.clone()),
        })
        .collect();

    if exprs.len() == 1 {
        return exprs.remove(0);
    }

    // Chain with AND.
    let mut result = exprs.remove(0);
    for expr in exprs {
        result = Expr::BinaryOp {
            left: Box::new(result),
            op: BinOp::And,
            right: Box::new(expr),
        };
    }
    result
}

/// Extract the alias from the last operator in the chain.
fn get_last_alias(op: &Option<LogicalOp>) -> String {
    match op {
        Some(LogicalOp::Scan { alias, .. }) => alias.clone(),
        Some(LogicalOp::Expand { dst_alias, .. }) => dst_alias.clone(),
        Some(LogicalOp::Filter { input, .. }) => get_last_alias(&Some(*input.clone())),
        _ => "_unknown".to_string(),
    }
}

/// Split RETURN items into group keys (non-aggregate) and aggregate expressions.
fn split_aggregates(
    return_clause: &ReturnClause,
) -> crate::types::Result<(Vec<Expr>, Vec<AggregateExpr>)> {
    let mut group_keys = Vec::new();
    let mut aggregates = Vec::new();

    for item in &return_clause.items {
        match &item.expr {
            Expr::FunctionCall { name, args } => {
                let function = match name.as_str() {
                    "count" => AggregateFunction::Count,
                    "sum" => AggregateFunction::Sum,
                    "avg" => AggregateFunction::Avg,
                    "min" => AggregateFunction::Min,
                    "max" => AggregateFunction::Max,
                    "collect" => AggregateFunction::Collect,
                    _ => {
                        return Err(GraphError::Serialization(format!(
                            "unknown aggregate function: {name}"
                        )))
                    }
                };
                let input = args.first().cloned().unwrap_or(Expr::Star);
                aggregates.push(AggregateExpr {
                    function,
                    input,
                    alias: item.alias.clone(),
                });
            }
            _ => group_keys.push(item.expr.clone()),
        }
    }

    Ok((group_keys, aggregates))
}
