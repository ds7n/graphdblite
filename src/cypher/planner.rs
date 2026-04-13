use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::ir::*;
use crate::index;
use crate::types::{Direction, GraphError};

/// Compile a Cypher AST Statement into a LogicalOp plan.
pub fn plan(conn: &Connection, stmt: &Statement) -> crate::types::Result<LogicalOp> {
    match stmt {
        Statement::Match(m) => plan_match(conn, m),
        Statement::Create(c) => plan_create(c),
        Statement::MatchCreate(mc) => plan_match_create(conn, mc),
        Statement::Delete(d) => plan_delete(conn, d),
        Statement::Set(s) => plan_set(conn, s),
        Statement::Merge(m) => plan_merge(m),
        Statement::Unwind(u) => plan_unwind(conn, u),
        Statement::Explain(inner) => plan(conn, inner),
    }
}

fn plan_match(conn: &Connection, stmt: &MatchStatement) -> crate::types::Result<LogicalOp> {
    // Build scan + expand chain from patterns.
    let mut op = plan_patterns(conn, &stmt.patterns)?;

    // Apply OPTIONAL MATCH clauses as LeftOuterJoins.
    for opt_patterns in &stmt.optional_patterns {
        let (right, new_aliases) = plan_optional_patterns(conn, opt_patterns)?;
        op = LogicalOp::LeftOuterJoin {
            input: Box::new(op),
            right: Box::new(right),
            optional_aliases: new_aliases,
        };
    }

    // Apply WHERE filter with predicate pushdown.
    if let Some(ref predicate) = stmt.where_clause {
        let conjuncts = decompose_conjuncts(predicate);
        let mut remaining = Vec::new();

        for conj in conjuncts {
            if let Some(pushed) = try_push_predicate(conn, &mut op, &conj) {
                op = pushed;
            } else {
                remaining.push(conj);
            }
        }

        // Wrap any remaining (non-pushable) conjuncts as a Filter.
        if let Some(filter_pred) = rebuild_conjunction(remaining) {
            op = LogicalOp::Filter {
                input: Box::new(op),
                predicate: filter_pred,
            };
        }
    }

    // Apply intermediate clauses (WITH/UNWIND).
    for clause in &stmt.intermediate_clauses {
        match clause {
            IntermediateClause::With(with) => op = plan_with(op, with)?,
            IntermediateClause::Unwind(unwind) => {
                op = LogicalOp::Unwind {
                    input: Box::new(op),
                    expr: unwind.expr.clone(),
                    alias: unwind.alias.clone(),
                };
            }
        }
    }

    // Check if RETURN contains aggregates.
    let has_aggregates = stmt.return_clause.items.iter().any(|item| {
        is_aggregate_fn(&item.expr)
    });

    if has_aggregates {
        let (group_keys, aggregates) = split_aggregates(&stmt.return_clause.items)?;
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

fn plan_match_create(conn: &Connection, stmt: &MatchCreateStatement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_patterns(conn, &stmt.patterns)?;

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    let mut create_ops = Vec::new();
    for pattern in &stmt.create_patterns {
        create_ops.extend(plan_create_pattern(pattern)?);
    }

    Ok(LogicalOp::MatchCreate {
        input: Box::new(op),
        create_ops,
    })
}

fn plan_delete(conn: &Connection, stmt: &DeleteStatement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_patterns(conn, &stmt.patterns)?;

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    Ok(LogicalOp::Delete {
        input: Box::new(op),
        variables: stmt.variables.clone(),
        detach: stmt.detach,
    })
}

fn plan_set(conn: &Connection, stmt: &SetStatement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_patterns(conn, &stmt.patterns)?;

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

fn plan_unwind(_conn: &Connection, stmt: &UnwindStatement) -> crate::types::Result<LogicalOp> {
    let mut op = LogicalOp::Unwind {
        input: Box::new(LogicalOp::EmptyRow),
        expr: stmt.expr.clone(),
        alias: stmt.alias.clone(),
    };

    match &stmt.body {
        UnwindBody::Return {
            where_clause,
            return_clause,
            order_by,
            limit,
        } => {
            if let Some(ref predicate) = where_clause {
                op = LogicalOp::Filter {
                    input: Box::new(op),
                    predicate: predicate.clone(),
                };
            }

            let has_aggregates = return_clause.items.iter().any(|item| {
                is_aggregate_fn(&item.expr)
            });

            if has_aggregates {
                let (group_keys, aggregates) = split_aggregates(&return_clause.items)?;
                op = LogicalOp::Aggregate {
                    input: Box::new(op),
                    group_keys,
                    aggregates,
                };
            }

            op = LogicalOp::Project {
                input: Box::new(op),
                items: return_clause.items.clone(),
            };

            if !order_by.is_empty() {
                op = LogicalOp::Sort {
                    input: Box::new(op),
                    items: order_by.clone(),
                };
            }

            if let Some(count) = limit {
                op = LogicalOp::Limit {
                    input: Box::new(op),
                    count: *count,
                };
            }
        }
        UnwindBody::Create { patterns } => {
            let mut create_ops = Vec::new();
            for pattern in patterns {
                create_ops.extend(plan_create_pattern(pattern)?);
            }
            op = LogicalOp::MatchCreate {
                input: Box::new(op),
                create_ops,
            };
        }
    }

    Ok(op)
}

fn plan_merge(stmt: &MergeStatement) -> crate::types::Result<LogicalOp> {
    // Validate: MERGE only supports single node patterns.
    match stmt.pattern.elements.first() {
        Some(crate::cypher::ast::PatternElement::Node(_)) if stmt.pattern.elements.len() == 1 => {}
        _ => {
            return Err(crate::types::GraphError::ParseError(
                "MERGE only supports single node patterns".to_string(),
            ));
        }
    }
    Ok(LogicalOp::Merge {
        pattern: stmt.pattern.clone(),
        on_create: stmt.on_create.clone(),
        on_match: stmt.on_match.clone(),
    })
}

/// Plan a WITH clause as an intermediate projection (+aggregation) and optional filter.
fn plan_with(input: LogicalOp, with: &WithClause) -> crate::types::Result<LogicalOp> {
    let mut op = input;

    // Check if WITH items contain aggregates.
    let has_aggregates = with.items.iter().any(|item| {
        is_aggregate_fn(&item.expr)
    });

    if has_aggregates {
        let (group_keys, aggregates) = split_aggregates(&with.items)?;
        op = LogicalOp::Aggregate {
            input: Box::new(op),
            group_keys,
            aggregates,
        };
    }

    // Project the WITH items.
    op = LogicalOp::Project {
        input: Box::new(op),
        items: with.items.clone(),
    };

    // Apply WITH's WHERE filter.
    if let Some(ref predicate) = with.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    Ok(op)
}

/// Plan the scan/expand chain for a list of patterns.
///
/// Regular patterns are reordered by estimated cardinality (smallest first)
/// to minimize cross-product intermediate sizes. Shortest-path patterns are
/// processed after regular patterns since they depend on bound variables.
pub fn plan_patterns(conn: &Connection, patterns: &[Pattern]) -> crate::types::Result<LogicalOp> {
    use crate::cypher::cost;

    if patterns.is_empty() {
        return Ok(LogicalOp::EmptyRow);
    }

    // Separate regular and shortest-path patterns.
    let mut regular: Vec<&Pattern> = Vec::new();
    let mut shortest: Vec<&Pattern> = Vec::new();
    for pat in patterns {
        if pat.shortest_path_mode != ShortestPathMode::None {
            shortest.push(pat);
        } else {
            regular.push(pat);
        }
    }

    // Reorder regular patterns by estimated cost (smallest first).
    // Pre-compute costs to avoid re-planning inside the sort comparator.
    if regular.len() > 1 {
        let mut indexed: Vec<(usize, f64)> = regular
            .iter()
            .enumerate()
            .map(|(i, pat)| {
                let cost = plan_single_pattern(conn, pat)
                    .ok()
                    .map(|p| cost::estimate(conn, &p).estimated_rows)
                    .unwrap_or(f64::MAX);
                (i, cost)
            })
            .collect();
        indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let reordered: Vec<&Pattern> = indexed.into_iter().map(|(i, _)| regular[i]).collect();
        regular = reordered;
    }

    // Build cross-product chain from regular patterns.
    let mut op: Option<LogicalOp> = None;
    for pattern in &regular {
        let right = plan_single_pattern(conn, pattern)?;
        op = Some(match op.take() {
            None => right,
            Some(left) => LogicalOp::CrossProduct {
                left: Box::new(left),
                right: Box::new(right),
            },
        });
    }

    // Apply shortest-path patterns last (they need bound variables).
    for pattern in &shortest {
        let right = plan_shortest_path_pattern(conn, pattern, op.take())?;
        op = Some(right);
    }

    op.ok_or_else(|| GraphError::Serialization("empty patterns".to_string()))
}

/// Plan a shortestPath / allShortestPaths pattern.
///
/// The pattern must be: (src_node)-[rel*..N]->(dst_node).
/// Both endpoint nodes need scans; the shortest path operator runs BFS between them.
fn plan_shortest_path_pattern(
    conn: &Connection,
    pattern: &Pattern,
    existing_input: Option<LogicalOp>,
) -> crate::types::Result<LogicalOp> {
    // Validate structure: must be exactly (node)-[rel]->(node).
    if pattern.elements.len() != 3 {
        return Err(GraphError::Serialization(
            "shortestPath pattern must be (a)-[*..N]->(b)".to_string(),
        ));
    }

    let src_node = match &pattern.elements[0] {
        PatternElement::Node(n) => n,
        _ => return Err(GraphError::Serialization(
            "shortestPath pattern must start with a node".to_string(),
        )),
    };
    let rel = match &pattern.elements[1] {
        PatternElement::Relationship(r) => r,
        _ => return Err(GraphError::Serialization(
            "shortestPath pattern must have a relationship".to_string(),
        )),
    };
    let dst_node = match &pattern.elements[2] {
        PatternElement::Node(n) => n,
        _ => return Err(GraphError::Serialization(
            "shortestPath pattern must end with a node".to_string(),
        )),
    };

    let src_alias = src_node.variable.clone().unwrap_or_else(|| "_sp_src".to_string());
    let dst_alias = dst_node.variable.clone().unwrap_or_else(|| "_sp_dst".to_string());
    let path_alias = pattern.path_variable.clone().unwrap_or_else(|| "_path".to_string());

    let direction = match rel.direction {
        RelDirection::Outgoing => Direction::Outgoing,
        RelDirection::Incoming => Direction::Incoming,
        RelDirection::Undirected => Direction::Both,
    };

    let (_, max_hops) = rel.var_length.unwrap_or((1, u32::MAX));

    // Build input: scan both endpoints and cross-product them.
    let input = if let Some(existing) = existing_input {
        // If we already have bound variables, use the existing pipeline.
        // Plan additional scans only for unbound nodes.
        existing
    } else {
        let src_scan = plan_node_scan(conn, src_node, &src_alias)?;
        let dst_scan = plan_node_scan(conn, dst_node, &dst_alias)?;
        LogicalOp::CrossProduct {
            left: Box::new(src_scan),
            right: Box::new(dst_scan),
        }
    };

    Ok(LogicalOp::ShortestPath {
        input: Box::new(input),
        src_alias,
        dst_alias,
        path_alias,
        edge_type: rel.rel_type.clone(),
        direction,
        max_hops,
        all_paths: pattern.shortest_path_mode == ShortestPathMode::All,
    })
}

/// Plan OPTIONAL MATCH patterns. Returns (plan, new_aliases) where the plan is
/// an expansion chain and new_aliases lists variables introduced by the optional
/// patterns (not shared with the required MATCH).
///
/// The plan expects to be executed per-input-record inside a LeftOuterJoin:
/// - Shared aliases (already bound) become the starting point for expands
/// - New aliases are the ones that get NULL-filled on no match
fn plan_optional_patterns(
    conn: &Connection,
    patterns: &[Pattern],
) -> crate::types::Result<(LogicalOp, Vec<String>)> {
    let mut new_aliases = Vec::new();
    let op = plan_patterns(conn, patterns)?;

    // Walk the patterns to collect aliases.
    // The first node in each pattern is assumed shared with the required MATCH.
    // Subsequent nodes (destinations of relationships) are new.
    for pattern in patterns {
        let mut first = true;
        for elem in &pattern.elements {
            if let PatternElement::Node(n) = elem {
                if let Some(ref var) = n.variable {
                    if first {
                        first = false;
                        // First node is shared — skip.
                    } else if !new_aliases.contains(var) {
                        new_aliases.push(var.clone());
                    }
                }
            }
        }
    }

    Ok((op, new_aliases))
}

/// Plan a single pattern: (a:Label)-[:TYPE]->(b:Label)
fn plan_single_pattern(conn: &Connection, pattern: &Pattern) -> crate::types::Result<LogicalOp> {
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

                    let scan = plan_node_scan(conn, node, &alias)?;
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

/// Plan the scan for a single node pattern, using an index lookup if available.
fn plan_node_scan(
    conn: &Connection,
    node: &NodePattern,
    alias: &str,
) -> crate::types::Result<LogicalOp> {
    let label = node.label.clone().unwrap_or_default();

    // Try to find an indexed property for this label.
    if !node.properties.is_empty() && !label.is_empty() {
        let indexes = index::list_indexes_for_label(conn, &label).unwrap_or_default();
        let indexed_props: Vec<&str> = indexes.iter().map(|(_, p)| p.as_str()).collect();

        // Collect all inline properties that have an index and a literal value.
        let mut candidates: Vec<(&String, &Expr)> = node
            .properties
            .iter()
            .filter(|(key, val)| {
                indexed_props.contains(&key.as_str()) && matches!(val, Expr::Literal(_))
            })
            .collect();

        // Pick the most selective index: lowest cardinality, ties broken
        // alphabetically for determinism.
        if candidates.len() > 1 {
            candidates.sort_by(|(key_a, expr_a), (key_b, expr_b)| {
                let val_a = match expr_a {
                    Expr::Literal(l) => crate::cypher::executor::literal_to_value(l),
                    _ => unreachable!(),
                };
                let val_b = match expr_b {
                    Expr::Literal(l) => crate::cypher::executor::literal_to_value(l),
                    _ => unreachable!(),
                };
                let count_a = index::index_count_for_value(conn, &label, key_a, &val_a)
                    .unwrap_or(usize::MAX);
                let count_b = index::index_count_for_value(conn, &label, key_b, &val_b)
                    .unwrap_or(usize::MAX);
                count_a.cmp(&count_b).then_with(|| key_a.cmp(key_b))
            });
        }

        let indexed_match = candidates.into_iter().next();

        if let Some((prop, expr)) = indexed_match {
            let lit = match expr {
                Expr::Literal(l) => l.clone(),
                _ => unreachable!(),
            };

            // Build remaining filters from non-indexed properties.
            let remaining: std::collections::HashMap<String, Expr> = node
                .properties
                .iter()
                .filter(|(k, _)| k != &prop)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let remaining_filters = if remaining.is_empty() {
                None
            } else {
                Some(properties_to_filter(alias, &remaining))
            };

            return Ok(LogicalOp::IndexLookup {
                label,
                alias: alias.to_string(),
                property: prop.clone(),
                value: lit,
                remaining_filters,
            });
        }
    }

    // Fallback: full label scan + filter.
    let mut scan = LogicalOp::Scan {
        label,
        alias: alias.to_string(),
    };

    if !node.properties.is_empty() {
        let predicate = properties_to_filter(alias, &node.properties);
        scan = LogicalOp::Filter {
            input: Box::new(scan),
            predicate,
        };
    }

    Ok(scan)
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
        Some(LogicalOp::IndexLookup { alias, .. }) => alias.clone(),
        Some(LogicalOp::Expand { dst_alias, .. }) => dst_alias.clone(),
        Some(LogicalOp::Filter { input, .. }) => get_last_alias(&Some(*input.clone())),
        _ => "_unknown".to_string(),
    }
}

/// Returns true if the expression is an aggregate function call (count, sum, avg, etc.).
fn is_aggregate_fn(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::FunctionCall { name, .. }
            if matches!(name.as_str(), "count" | "sum" | "avg" | "min" | "max" | "collect")
    )
}

/// Split RETURN/WITH items into group keys (non-aggregate) and aggregate expressions.
fn split_aggregates(
    items: &[ReturnItem],
) -> crate::types::Result<(Vec<Expr>, Vec<AggregateExpr>)> {
    let mut group_keys = Vec::new();
    let mut aggregates = Vec::new();

    for item in items {
        match &item.expr {
            Expr::FunctionCall { name, args } => {
                let function = match name.as_str() {
                    "count" => Some(AggregateFunction::Count),
                    "sum" => Some(AggregateFunction::Sum),
                    "avg" => Some(AggregateFunction::Avg),
                    "min" => Some(AggregateFunction::Min),
                    "max" => Some(AggregateFunction::Max),
                    "collect" => Some(AggregateFunction::Collect),
                    _ => None, // Scalar function — treat as regular expression.
                };
                if let Some(function) = function {
                    let input = args.first().cloned().unwrap_or(Expr::Star);
                    aggregates.push(AggregateExpr {
                        function,
                        input,
                        alias: item.alias.clone(),
                    });
                } else {
                    group_keys.push(item.expr.clone());
                }
            }
            _ => group_keys.push(item.expr.clone()),
        }
    }

    Ok((group_keys, aggregates))
}

// ── Predicate pushdown helpers ──────────────────────────────────────────

/// Flatten a predicate into AND-connected conjuncts.
fn decompose_conjuncts(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp { left, op: BinOp::And, right } => {
            let mut out = decompose_conjuncts(left);
            out.extend(decompose_conjuncts(right));
            out
        }
        _ => vec![expr.clone()],
    }
}

/// Rebuild a conjunction from a list of conjuncts. Returns None if empty.
fn rebuild_conjunction(conjuncts: Vec<Expr>) -> Option<Expr> {
    conjuncts.into_iter().reduce(|acc, c| Expr::BinaryOp {
        left: Box::new(acc),
        op: BinOp::And,
        right: Box::new(c),
    })
}

/// Try to push a single equality predicate (`alias.prop = literal`) into an
/// existing Scan node in the plan tree, converting it to an IndexLookup.
///
/// Returns `Some(modified_op)` if the predicate was pushed, `None` if it
/// cannot be pushed (no matching scan, no index, non-eligible predicate).
fn try_push_predicate(
    conn: &Connection,
    op: &mut LogicalOp,
    predicate: &Expr,
) -> Option<LogicalOp> {
    // Only handle: Property(alias, prop) = Literal(val)
    let (alias, prop, lit) = match predicate {
        Expr::BinaryOp {
            left,
            op: BinOp::Eq,
            right,
        } => {
            match (left.as_ref(), right.as_ref()) {
                (Expr::Property(a, p), Expr::Literal(l)) => (a.clone(), p.clone(), l.clone()),
                (Expr::Literal(l), Expr::Property(a, p)) => (a.clone(), p.clone(), l.clone()),
                _ => return None,
            }
        }
        _ => return None,
    };

    // Check if there's an index for this label+property.
    // Walk the plan tree to find the Scan for this alias.
    try_replace_scan(conn, op, &alias, &prop, &lit)
}

/// Recursively search the plan tree for a `Scan` with the given alias and
/// replace it with an `IndexLookup` if an index exists. Returns the new
/// root op if a replacement was made.
fn try_replace_scan(
    conn: &Connection,
    op: &mut LogicalOp,
    alias: &str,
    prop: &str,
    lit: &LiteralValue,
) -> Option<LogicalOp> {
    match op {
        LogicalOp::Scan { label, alias: scan_alias } if scan_alias == alias => {
            if label.is_empty() {
                return None;
            }
            let indexes = index::list_indexes_for_label(conn, label).unwrap_or_default();
            let has_index = indexes.iter().any(|(_, p)| p == prop);
            if !has_index {
                return None;
            }
            Some(LogicalOp::IndexLookup {
                label: label.clone(),
                alias: alias.to_string(),
                property: prop.to_string(),
                value: lit.clone(),
                remaining_filters: None,
            })
        }

        // Walk through wrapper operators that preserve the scan.
        LogicalOp::Filter { input, predicate: existing } => {
            if let Some(new_input) = try_replace_scan(conn, input, alias, prop, lit) {
                Some(LogicalOp::Filter {
                    input: Box::new(new_input),
                    predicate: existing.clone(),
                })
            } else {
                None
            }
        }

        LogicalOp::Expand {
            input, src_alias, dst_alias, edge_type, direction, min_hops, max_hops,
        } => {
            if let Some(new_input) = try_replace_scan(conn, input, alias, prop, lit) {
                Some(LogicalOp::Expand {
                    input: Box::new(new_input),
                    src_alias: src_alias.clone(),
                    dst_alias: dst_alias.clone(),
                    edge_type: edge_type.clone(),
                    direction: *direction,
                    min_hops: *min_hops,
                    max_hops: *max_hops,
                })
            } else {
                None
            }
        }

        LogicalOp::CrossProduct { left, right } => {
            if let Some(new_left) = try_replace_scan(conn, left, alias, prop, lit) {
                Some(LogicalOp::CrossProduct {
                    left: Box::new(new_left),
                    right: right.clone(),
                })
            } else if let Some(new_right) = try_replace_scan(conn, right, alias, prop, lit) {
                Some(LogicalOp::CrossProduct {
                    left: left.clone(),
                    right: Box::new(new_right),
                })
            } else {
                None
            }
        }

        _ => None,
    }
}
