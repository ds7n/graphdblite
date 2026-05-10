//! Pattern planning — plan_patterns, plan_shortest_path_pattern, plan_optional_match, plan_single_pattern, plan_node_scan, plan_create_pattern.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::types::*;

use super::helpers::*;
use super::validation::*;
use super::*;

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

    // Build join chain from regular patterns.
    // Use CorrelatedJoin when patterns share variables (e.g.
    // `(a)-[:A]->(b), (b)-[:B]->(a)`) so the shared variables are
    // bound from the left side. Use CrossProduct for independent patterns.
    let mut op: Option<LogicalOp> = None;
    let mut bound_vars: HashSet<String> = HashSet::new();
    for pattern in &regular {
        let right = plan_single_pattern(conn, pattern)?;
        let pattern_vars = collect_pattern_variables(std::slice::from_ref(*pattern));
        let shared = pattern_vars.intersection(&bound_vars).count() > 0;
        op = Some(match op.take() {
            None => right,
            Some(left) => {
                if shared {
                    LogicalOp::CorrelatedJoin {
                        input: Box::new(left),
                        right: Box::new(right),
                        same_match: true,
                    }
                } else {
                    LogicalOp::CrossProduct {
                        left: Box::new(left),
                        right: Box::new(right),
                        same_match: true,
                    }
                }
            }
        });
        bound_vars.extend(pattern_vars);
    }

    // Apply shortest-path patterns last (they need bound variables).
    for pattern in &shortest {
        let right = plan_shortest_path_pattern(conn, pattern, op.take())?;
        op = Some(right);
    }

    op.ok_or_else(|| GraphError::semantic("empty patterns".to_string()))
}

/// Plan a shortestPath / allShortestPaths pattern.
///
/// The pattern must be: (src_node)-[rel*..N]->(dst_node).
/// Both endpoint nodes need scans; the shortest path operator runs BFS between them.
pub(in crate::cypher::planner) fn plan_shortest_path_pattern(
    conn: &Connection,
    pattern: &Pattern,
    existing_input: Option<LogicalOp>,
) -> crate::types::Result<LogicalOp> {
    // Validate structure: must be exactly (node)-[rel]->(node).
    if pattern.elements.len() != 3 {
        return Err(GraphError::semantic(
            "shortestPath pattern must be (a)-[*..N]->(b)".to_string(),
        ));
    }

    let src_node = match &pattern.elements[0] {
        PatternElement::Node(n) => n,
        _ => {
            return Err(GraphError::semantic(
                "shortestPath pattern must start with a node".to_string(),
            ))
        }
    };
    let rel = match &pattern.elements[1] {
        PatternElement::Relationship(r) => r,
        _ => {
            return Err(GraphError::semantic(
                "shortestPath pattern must have a relationship".to_string(),
            ))
        }
    };
    let dst_node = match &pattern.elements[2] {
        PatternElement::Node(n) => n,
        _ => {
            return Err(GraphError::semantic(
                "shortestPath pattern must end with a node".to_string(),
            ))
        }
    };

    let src_alias = src_node
        .variable
        .clone()
        .unwrap_or_else(|| "_sp_src".to_string());
    let dst_alias = dst_node
        .variable
        .clone()
        .unwrap_or_else(|| "_sp_dst".to_string());
    let path_alias = pattern
        .path_variable
        .clone()
        .unwrap_or_else(|| "_path".to_string());

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
            same_match: false,
        }
    };

    Ok(LogicalOp::ShortestPath {
        input: Box::new(input),
        src_alias,
        dst_alias,
        path_alias,
        edge_type: rel.rel_types.first().cloned(),
        direction,
        max_hops,
        all_paths: pattern.shortest_path_mode == ShortestPathMode::All,
    })
}

/// Validate that all variables referenced in RETURN items are bound in scope.
pub(in crate::cypher::planner) fn plan_optional_match(
    conn: &Connection,
    opt_match: &OptionalMatch,
    bound_vars: &HashSet<String>,
) -> crate::types::Result<(LogicalOp, Vec<String>, Option<Expr>)> {
    let mut new_aliases = Vec::new();
    let op = plan_patterns(conn, &opt_match.patterns)?;

    for pattern in &opt_match.patterns {
        // Include path variable (p = ...) in optional aliases so it gets null-filled.
        if let Some(ref path_var) = pattern.path_variable {
            if !bound_vars.contains(path_var) && !new_aliases.contains(path_var) {
                new_aliases.push(path_var.clone());
            }
        }
        for elem in &pattern.elements {
            let var = match elem {
                PatternElement::Node(n) => n.variable.as_ref(),
                PatternElement::Relationship(r) => r.variable.as_ref(),
            };
            if let Some(var) = var {
                if !bound_vars.contains(var) && !new_aliases.contains(var) {
                    new_aliases.push(var.clone());
                }
            }
        }
    }

    Ok((op, new_aliases, opt_match.where_clause.clone()))
}

/// Plan a single pattern: (a:Label)-[:TYPE]->(b:Label)
pub(in crate::cypher::planner) fn plan_single_pattern(
    conn: &Connection,
    pattern: &Pattern,
) -> crate::types::Result<LogicalOp> {
    let mut op: Option<LogicalOp> = None;
    // Track node aliases already introduced so we can add identity filters
    // when a variable reappears (e.g. cyclic pattern `(a)-[:R]->(b)-[:S]->(a)`).
    let mut seen_node_aliases: HashSet<String> = HashSet::new();
    // Track actual aliases assigned to each element position for MaterializePath.
    let mut element_aliases: HashMap<usize, String> = HashMap::new();

    let mut i = 0;
    while i < pattern.elements.len() {
        match &pattern.elements[i] {
            PatternElement::Node(node) => {
                if op.is_none() {
                    // First node — start a scan.
                    let alias = node.variable.clone().unwrap_or_else(|| {
                        let n = ANON_COUNTER.fetch_add(1, Ordering::Relaxed);
                        format!("_anon_{n}")
                    });

                    let scan = plan_node_scan(conn, node, &alias)?;
                    op = Some(scan);
                    seen_node_aliases.insert(alias.clone());
                    element_aliases.insert(i, alias);
                }
                // Subsequent nodes after a relationship are handled in the rel branch.
                i += 1;
            }
            PatternElement::Relationship(rel) => {
                // Must have a next node.
                let dst_node = match pattern.elements.get(i + 1) {
                    Some(PatternElement::Node(n)) => n,
                    _ => {
                        return Err(GraphError::semantic(
                            "relationship must be followed by a node pattern".to_string(),
                        ))
                    }
                };

                let src_alias = get_last_alias(&op);
                let dst_alias = dst_node.variable.clone().unwrap_or_else(|| {
                    let n = ANON_COUNTER.fetch_add(1, Ordering::Relaxed);
                    format!("_anon_{n}")
                });

                let direction = match rel.direction {
                    RelDirection::Outgoing => Direction::Outgoing,
                    RelDirection::Incoming => Direction::Incoming,
                    RelDirection::Undirected => Direction::Both,
                };

                let (min_hops, max_hops) = rel.var_length.unwrap_or((1, 1));

                // Always assign a synthetic alias for anonymous relationships so
                // edge identity is tracked for relationship uniqueness within a
                // MATCH pattern. Named paths use a path-specific prefix.
                let effective_rel_alias = if pattern.path_variable.is_some() {
                    Some(
                        rel.variable
                            .clone()
                            .unwrap_or_else(|| format!("_path_rel_{i}")),
                    )
                } else {
                    Some(rel.variable.clone().unwrap_or_else(|| {
                        let n = ANON_COUNTER.fetch_add(1, Ordering::Relaxed);
                        format!("_anon_rel_{n}")
                    }))
                };

                op = Some(LogicalOp::Expand {
                    input: Box::new(op.unwrap()),
                    src_alias,
                    dst_alias: dst_alias.clone(),
                    rel_alias: effective_rel_alias.clone(),
                    edge_types: rel.rel_types.clone(),
                    direction,
                    min_hops,
                    max_hops,
                    var_length: rel.var_length.is_some(),
                    var_length_prop_filters: if rel.var_length.is_some() {
                        rel.properties.clone()
                    } else {
                        HashMap::new()
                    },
                    result_cap: None,
                });

                // Apply destination node's label filters.
                for dst_label in &dst_node.labels {
                    if !dst_label.is_empty() {
                        let predicate = Expr::synthetic(ExprKind::BinaryOp {
                            left: Box::new(Expr::synthetic(ExprKind::Literal(
                                LiteralValue::String(dst_label.clone()),
                            ))),
                            op: BinOp::In,
                            right: Box::new(Expr::synthetic(ExprKind::Property(
                                dst_alias.clone(),
                                "__labels".to_string(),
                            ))),
                        });
                        op = Some(LogicalOp::Filter {
                            input: Box::new(op.unwrap()),
                            predicate,
                        });
                    }
                }

                // Apply destination node's inline property filters.
                if !dst_node.properties.is_empty() {
                    let predicate = properties_to_filter(&dst_alias, &dst_node.properties);
                    op = Some(LogicalOp::Filter {
                        input: Box::new(op.unwrap()),
                        predicate,
                    });
                }

                // Apply relationship inline property filters.
                // For var-length patterns, these are passed through the Expand
                // node and applied at each hop inside traverse_paths().
                if !rel.properties.is_empty() && rel.var_length.is_none() {
                    if let Some(ref r_alias) = effective_rel_alias {
                        let predicate = properties_to_filter(r_alias, &rel.properties);
                        op = Some(LogicalOp::Filter {
                            input: Box::new(op.unwrap()),
                            predicate,
                        });
                    }
                }

                seen_node_aliases.insert(dst_alias.clone());
                element_aliases.insert(i + 1, dst_alias);
                if let Some(ref ra) = effective_rel_alias {
                    element_aliases.insert(i, ra.clone());
                }

                i += 2; // skip rel + dst node
            }
        }
    }

    let mut result = op.ok_or_else(|| GraphError::semantic("empty pattern".to_string()))?;

    // If this pattern has a path variable binding, wrap with MaterializePath.
    if let Some(ref path_var) = pattern.path_variable {
        let mut node_aliases = Vec::new();
        let mut rel_aliases = Vec::new();
        for (idx, elem) in pattern.elements.iter().enumerate() {
            match elem {
                PatternElement::Node(n) => {
                    let alias = element_aliases
                        .get(&idx)
                        .cloned()
                        .or_else(|| n.variable.clone())
                        .unwrap_or_else(|| format!("_anon_{idx}"));
                    node_aliases.push(alias);
                }
                PatternElement::Relationship(r) => {
                    let alias = element_aliases
                        .get(&idx)
                        .cloned()
                        .or_else(|| r.variable.clone())
                        .unwrap_or_else(|| format!("_path_rel_{idx}"));
                    rel_aliases.push(alias);
                }
            }
        }
        result = LogicalOp::MaterializePath {
            input: Box::new(result),
            path_alias: path_var.clone(),
            node_aliases,
            rel_aliases,
        };
    }

    Ok(result)
}

/// Plan the scan for a single node pattern, using an index lookup if available.
pub(in crate::cypher::planner) fn plan_node_scan(
    conn: &Connection,
    node: &NodePattern,
    alias: &str,
) -> crate::types::Result<LogicalOp> {
    // Use the first label for scanning/indexing. Additional labels become filters.
    let label = node.labels.first().cloned().unwrap_or_default();

    // Try to find an indexed property for this label.
    if !node.properties.is_empty() && !label.is_empty() {
        let indexes = index::list_indexes_for_label(conn, &label).unwrap_or_default();
        let indexed_props: Vec<&str> = indexes.iter().map(|(_, p)| p.as_str()).collect();

        // Collect all inline properties that have an index and a literal value.
        let mut candidates: Vec<(&String, &Expr)> = node
            .properties
            .iter()
            .filter(|(key, val)| {
                indexed_props.contains(&key.as_str())
                    && matches!(val.kind, ExprKind::Literal(_) | ExprKind::Parameter(_))
            })
            .collect();

        // Pick the most selective index: lowest cardinality, ties broken
        // alphabetically for determinism.
        if candidates.len() > 1 {
            // Pre-compute literal values up front so the sort comparator can be
            // infallible; if any candidate is somehow not a literal (would
            // indicate a planner/grammar bug), fall back to lexicographic
            // ordering rather than panic.
            let mut indexed: Vec<(&String, &Expr, Option<crate::types::Value>)> = candidates
                .iter()
                .map(|(k, e)| {
                    let v = match &e.kind {
                        ExprKind::Literal(l) => Some(crate::cypher::executor::literal_to_value(l)),
                        _ => None,
                    };
                    (*k, *e, v)
                })
                .collect();
            indexed.sort_by(|(key_a, _, val_a), (key_b, _, val_b)| {
                let count_a = match val_a {
                    Some(v) => {
                        index::index_count_for_value(conn, &label, key_a, v).unwrap_or(usize::MAX)
                    }
                    None => usize::MAX,
                };
                let count_b = match val_b {
                    Some(v) => {
                        index::index_count_for_value(conn, &label, key_b, v).unwrap_or(usize::MAX)
                    }
                    None => usize::MAX,
                };
                count_a.cmp(&count_b).then_with(|| key_a.cmp(key_b))
            });
            candidates = indexed.into_iter().map(|(k, e, _)| (k, e)).collect();
        }

        let indexed_match = candidates.into_iter().next();

        if let Some((prop, expr)) = indexed_match {
            let lookup_key = match &expr.kind {
                ExprKind::Literal(l) => LookupKey::Literal(l.clone()),
                ExprKind::Parameter(name) => LookupKey::Param(name.clone()),
                // Defensive: candidates were filtered upstream.
                _ => {
                    return Err(GraphError::query(
                        crate::types::QueryPhase::SemanticAnalysis,
                        ErrorCode::Other,
                        "internal: indexed match candidate is not a literal or parameter"
                            .to_string(),
                    ));
                }
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
                value: lookup_key,
                remaining_filters,
            });
        }
    }

    // Fallback: full label scan + filter.
    let mut scan = LogicalOp::Scan {
        label,
        alias: alias.to_string(),
    };

    // Add filters for additional labels (multi-label nodes).
    for extra_label in node.labels.iter().skip(1) {
        let predicate = Expr::synthetic(ExprKind::BinaryOp {
            left: Box::new(Expr::synthetic(ExprKind::Literal(LiteralValue::String(
                extra_label.clone(),
            )))),
            op: BinOp::In,
            right: Box::new(Expr::synthetic(ExprKind::Property(
                alias.to_string(),
                "__labels".to_string(),
            ))),
        });
        scan = LogicalOp::Filter {
            input: Box::new(scan),
            predicate,
        };
    }

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
///
/// `seen` tracks named variables that already have a `CreateNode` op emitted
/// (across all patterns in the same CREATE statement) to avoid creating
/// duplicate nodes for reused variables like `CREATE (a), (a)-[:R]->(b)`.
pub(in crate::cypher::planner) fn plan_create_pattern(
    pattern: &Pattern,
    seen: &mut HashSet<String>,
) -> crate::types::Result<Vec<LogicalOp>> {
    let mut anon_counter = 0usize;
    plan_create_pattern_with_counter(pattern, seen, &mut anon_counter)
}

pub(in crate::cypher::planner) fn plan_create_pattern_with_counter(
    pattern: &Pattern,
    seen: &mut HashSet<String>,
    anon_counter: &mut usize,
) -> crate::types::Result<Vec<LogicalOp>> {
    let mut ops = Vec::new();
    let mut last_alias: Option<String> = None;

    let mut i = 0;
    while i < pattern.elements.len() {
        match &pattern.elements[i] {
            PatternElement::Node(node) => {
                let is_named = node.variable.is_some();
                let alias = node.variable.clone().or_else(|| {
                    *anon_counter += 1;
                    Some(format!("__anon_{}", anon_counter))
                });
                // Only dedup named variables; anonymous nodes are always new.
                let already_seen =
                    is_named && alias.as_ref().is_some_and(|n| !seen.insert(n.clone()));
                if already_seen && !node.labels.is_empty() {
                    // Rebinding a variable with new labels is a VariableAlreadyBound error.
                    return Err(GraphError::syntax(format!(
                        "variable `{}` already bound",
                        alias.as_deref().unwrap_or("?")
                    ))
                    .with_code(ErrorCode::VariableAlreadyBound));
                }
                if !already_seen {
                    ops.push(LogicalOp::CreateNode {
                        labels: node.labels.clone(),
                        alias: alias.clone(),
                        properties: node.properties.clone(),
                    });
                }
                last_alias = alias;
                i += 1;
            }
            PatternElement::Relationship(rel) => {
                // Variable-length relationships are not allowed in CREATE patterns.
                if rel.var_length.is_some() {
                    return Err(GraphError::syntax(
                        "variable-length relationships are not allowed in CREATE".to_string(),
                    )
                    .with_code(ErrorCode::CreatingVarLength));
                }

                let dst_node = match pattern.elements.get(i + 1) {
                    Some(PatternElement::Node(n)) => n,
                    _ => {
                        return Err(GraphError::semantic(
                            "relationship must be followed by a node".to_string(),
                        ))
                    }
                };

                let dst_is_named = dst_node.variable.is_some();
                let dst_alias = dst_node.variable.clone().or_else(|| {
                    *anon_counter += 1;
                    Some(format!("__anon_{}", anon_counter))
                });
                let dst_already_seen =
                    dst_is_named && dst_alias.as_ref().is_some_and(|n| !seen.insert(n.clone()));
                if dst_already_seen && !dst_node.labels.is_empty() {
                    return Err(GraphError::syntax(format!(
                        "variable `{}` already bound",
                        dst_alias.as_deref().unwrap_or("?")
                    ))
                    .with_code(ErrorCode::VariableAlreadyBound));
                }
                if !dst_already_seen {
                    ops.push(LogicalOp::CreateNode {
                        labels: dst_node.labels.clone(),
                        alias: dst_alias.clone(),
                        properties: dst_node.properties.clone(),
                    });
                }

                let left = last_alias
                    .clone()
                    .ok_or_else(|| GraphError::semantic("edge without source node".to_string()))?;
                let right = dst_alias.clone().ok_or_else(|| {
                    GraphError::semantic("edge target must have a variable".to_string())
                })?;

                // Respect relationship direction: `(a)<-[:T]-(b)` means
                // the edge goes FROM b TO a.
                let (src, dst) = if rel.direction == RelDirection::Incoming {
                    (right, left)
                } else {
                    (left, right)
                };

                ops.push(LogicalOp::CreateEdge {
                    src_alias: src,
                    dst_alias: dst,
                    edge_type: rel.rel_types.first().cloned().unwrap_or_default(),
                    rel_alias: rel.variable.clone(),
                    properties: rel.properties.clone(),
                });

                last_alias = dst_alias;
                i += 2;
            }
        }
    }

    Ok(ops)
}
