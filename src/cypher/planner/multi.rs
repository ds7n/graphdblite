//! Multi-clause sequencing — plan_multi_clause, plan_with, plan_with_scoped, alias substitution, intermediate-match scoping.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::types::*;

use super::helpers::*;
use super::pattern::*;
use super::validation::*;
use super::*;

pub(in crate::cypher::planner) fn plan_multi_clause(
    conn: &Connection,
    stmt: &MultiClauseStatement,
) -> crate::types::Result<LogicalOp> {
    use crate::cypher::ast::Clause;

    let mut op: Option<LogicalOp> = None;
    let mut seen_create_vars: HashSet<String> = HashSet::new();
    let mut anon_counter: usize = 0;
    let mut scope_vars: HashSet<String> = HashSet::new();
    let mut var_types: std::collections::HashMap<String, VarKind> =
        std::collections::HashMap::new();
    // Track WITH-bound variable value kinds for VariableTypeConflict detection.
    let mut with_value_kinds: HashMap<String, WithValueKind> = HashMap::new();

    for clause in &stmt.clauses {
        match clause {
            Clause::Match {
                patterns,
                optional_patterns,
                where_clause,
            } => {
                // Check VariableTypeConflict: WITH-bound scalars used as node/rel/path.
                // Also check VariableAlreadyBound: path variable re-assignment.
                for pat in patterns.iter() {
                    if let Some(ref path_var) = pat.path_variable {
                        if with_value_kinds.contains_key(path_var) {
                            // Path variable re-assigned — VariableAlreadyBound.
                            return Err(GraphError::syntax(format!(
                                "variable `{path_var}` already bound"
                            ))
                            .with_code(ErrorCode::VariableAlreadyBound));
                        }
                    }
                    for elem in &pat.elements {
                        match elem {
                            PatternElement::Node(n) => {
                                if let Some(ref var) = n.variable {
                                    if let Some(&kind) = with_value_kinds.get(var) {
                                        if kind == WithValueKind::Scalar {
                                            return Err(GraphError::syntax(format!(
                                                "variable `{var}` already defined as a scalar value"
                                            ))
                                            .with_code(ErrorCode::VariableTypeConflict));
                                        }
                                    }
                                }
                            }
                            PatternElement::Relationship(r) => {
                                if let Some(ref var) = r.variable {
                                    if let Some(&kind) = with_value_kinds.get(var) {
                                        if kind == WithValueKind::Scalar {
                                            return Err(GraphError::syntax(format!(
                                                "variable `{var}` already defined as a scalar value"
                                            ))
                                            .with_code(ErrorCode::VariableTypeConflict));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Validate variable-type consistency (node vs rel vs path) across all clauses.
                validate_variable_types_with_map(patterns, &mut var_types)?;
                let match_op = plan_patterns(conn, patterns)?;
                op = Some(if let Some(input) = op.take() {
                    // For standalone OPTIONAL MATCH (empty required patterns),
                    // skip the CorrelatedJoin with EmptyRow and just keep the input.
                    let mut joined = if patterns.is_empty() {
                        input
                    } else {
                        // Correlated join: thread prior rows into MATCH.
                        LogicalOp::CorrelatedJoin {
                            input: Box::new(input),
                            right: Box::new(match_op),
                            same_match: false,
                        }
                    };
                    // Optional matches — include scope_vars so variables from
                    // prior clauses are recognized as bound (not new).
                    let mut bound_vars = collect_pattern_variables(patterns);
                    bound_vars.extend(scope_vars.iter().cloned());
                    for opt in optional_patterns {
                        let (right, new_aliases, opt_filter) =
                            plan_optional_match(conn, opt, &bound_vars)?;
                        joined = LogicalOp::LeftOuterJoin {
                            input: Box::new(joined),
                            right: Box::new(right),
                            optional_aliases: new_aliases.clone(),
                            opt_filter,
                        };
                        bound_vars.extend(new_aliases);
                    }
                    if let Some(ref pred) = where_clause {
                        joined = LogicalOp::Filter {
                            input: Box::new(joined),
                            predicate: pred.clone(),
                        };
                    }
                    joined
                } else {
                    let mut m = match_op;
                    let mut bound_vars = collect_pattern_variables(patterns);
                    for opt in optional_patterns {
                        let (right, new_aliases, opt_filter) =
                            plan_optional_match(conn, opt, &bound_vars)?;
                        m = LogicalOp::LeftOuterJoin {
                            input: Box::new(m),
                            right: Box::new(right),
                            optional_aliases: new_aliases.clone(),
                            opt_filter,
                        };
                        bound_vars.extend(new_aliases);
                    }
                    if let Some(ref pred) = where_clause {
                        m = LogicalOp::Filter {
                            input: Box::new(m),
                            predicate: pred.clone(),
                        };
                    }
                    m
                });
                scope_vars.extend(collect_pattern_variables(patterns));
                for opt in optional_patterns {
                    scope_vars.extend(collect_pattern_variables(&opt.patterns));
                }
            }
            Clause::Create { patterns } => {
                // Validate CREATE property expressions, relationship types, and direction.
                validate_create_patterns(patterns, &scope_vars)?;
                // Validate VariableAlreadyBound: in a CREATE pattern, if a node
                // variable is already in scope AND the pattern tries to create a
                // new node (standalone node or node with labels/properties), that's an error.
                // But if it's used as an endpoint in a relationship pattern where
                // it's already bound, it references the existing node.
                for pattern in patterns {
                    // Check standalone node patterns (single-element patterns).
                    if pattern.elements.len() == 1 {
                        if let PatternElement::Node(n) = &pattern.elements[0] {
                            if let Some(ref var) = n.variable {
                                if scope_vars.contains(var) || seen_create_vars.contains(var) {
                                    return Err(GraphError::syntax(format!(
                                        "variable `{var}` already bound"
                                    ))
                                    .with_code(ErrorCode::VariableAlreadyBound));
                                }
                            }
                        }
                    }
                    // Check relationship variables for rebinding.
                    for elem in &pattern.elements {
                        if let PatternElement::Relationship(r) = elem {
                            if let Some(ref var) = r.variable {
                                if scope_vars.contains(var) || seen_create_vars.contains(var) {
                                    return Err(GraphError::syntax(format!(
                                        "variable `{var}` already bound"
                                    ))
                                    .with_code(ErrorCode::VariableAlreadyBound));
                                }
                            }
                        }
                    }
                }
                let mut create_ops = Vec::new();
                for pattern in patterns {
                    create_ops.extend(plan_create_pattern_with_counter(
                        pattern,
                        &mut seen_create_vars,
                        &mut anon_counter,
                    )?);
                }
                op = Some(if let Some(input) = op.take() {
                    LogicalOp::MatchCreate {
                        input: Box::new(input),
                        create_ops,
                    }
                } else {
                    // Standalone CREATE (no preceding clause).
                    if create_ops.len() == 1 {
                        create_ops.remove(0)
                    } else {
                        LogicalOp::CreateSequence { ops: create_ops }
                    }
                });
                scope_vars.extend(collect_pattern_variables(patterns));
            }
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => {
                // Validate MERGE pattern: relationship types, ON CREATE/ON MATCH variables.
                validate_merge_pattern(pattern, &scope_vars, on_create, on_match)?;
                // Validate VariableAlreadyBound for MERGE.
                validate_merge_variable_rebinding(pattern, &scope_vars)?;
                // Reject variable-length relationships in MERGE patterns.
                for el in &pattern.elements {
                    if let PatternElement::Relationship(rel) = el {
                        if rel.var_length.is_some() {
                            return Err(GraphError::syntax(
                                "variable-length relationships are not allowed in MERGE"
                                    .to_string(),
                            )
                            .with_code(ErrorCode::CreatingVarLength));
                        }
                    }
                }
                op = Some(if let Some(input) = op.take() {
                    LogicalOp::MatchMerge {
                        input: Box::new(input),
                        merge_pattern: pattern.clone(),
                        on_create: on_create.clone(),
                        on_match: on_match.clone(),
                    }
                } else {
                    LogicalOp::Merge {
                        pattern: pattern.clone(),
                        on_create: on_create.clone(),
                        on_match: on_match.clone(),
                    }
                });
                scope_vars.extend(collect_pattern_variables(std::slice::from_ref(pattern)));
            }
            Clause::With(with) => {
                // Validate WITH ORDER BY references only in-scope variables.
                // The ORDER BY is evaluated BEFORE projection, so it can reference
                // the input scope (scope_vars) plus the WITH projection itself.
                if !with.order_by.is_empty() && !scope_vars.is_empty() {
                    // Build combined scope: input scope + WITH projection aliases.
                    let mut order_scope = scope_vars.clone();
                    for item in &with.items {
                        if let Some(ref alias) = item.alias {
                            order_scope.insert(alias.clone());
                        }
                        order_scope.insert(crate::cypher::eval::expr_to_column_name(&item.expr));
                    }
                    for sort_item in &with.order_by {
                        if !is_aggregate_fn(&sort_item.expr) {
                            check_expr_variables(&sort_item.expr, &order_scope).map_err(|_| {
                                GraphError::syntax("ORDER BY references a variable not in scope")
                                    .with_code(ErrorCode::UndefinedVariable)
                            })?;
                        }
                    }
                }

                let input = op.take().unwrap_or(LogicalOp::EmptyRow);
                op = Some(plan_with(conn, input, with)?);
                // WITH resets scope.
                let old_scope = scope_vars.clone();
                scope_vars.clear();
                with_value_kinds.clear();
                for item in &with.items {
                    if let ExprKind::Star = &item.expr.kind {
                        scope_vars = old_scope.clone();
                        // WITH * passes through existing kinds.
                    } else {
                        let var_name = if let Some(ref alias) = item.alias {
                            scope_vars.insert(alias.clone());
                            alias.clone()
                        } else if let ExprKind::Variable(var) = &item.expr.kind {
                            scope_vars.insert(var.clone());
                            var.clone()
                        } else if let ExprKind::Property(var, prop) = &item.expr.kind {
                            let col = format!("{var}.{prop}");
                            scope_vars.insert(col.clone());
                            col
                        } else {
                            continue;
                        };
                        let kind = infer_with_value_kind(&item.expr);
                        with_value_kinds.insert(var_name, kind);
                    }
                }
            }
            Clause::Unwind(unwind) => {
                let input = op.take().unwrap_or(LogicalOp::EmptyRow);
                op = Some(LogicalOp::Unwind {
                    input: Box::new(input),
                    expr: unwind.expr.clone(),
                    alias: unwind.alias.clone(),
                });
                scope_vars.insert(unwind.alias.clone());
            }
            Clause::Set { items } => {
                // Validate SET variable references are in scope.
                validate_set_variables(items, &scope_vars)?;
                let input = op
                    .take()
                    .ok_or_else(|| GraphError::semantic("SET requires preceding MATCH"))?;
                let mut current = input;
                for item in items {
                    current = match item {
                        SetItem::Property(a) => LogicalOp::SetProperty {
                            input: Box::new(current),
                            assignments: vec![a.clone()],
                        },
                        SetItem::Label { variable, labels } => LogicalOp::SetLabel {
                            input: Box::new(current),
                            variable: variable.clone(),
                            labels: labels.clone(),
                        },
                        SetItem::MapOverwrite { variable, value } => LogicalOp::SetProperties {
                            input: Box::new(current),
                            variable: variable.clone(),
                            value: value.clone(),
                            merge: false,
                        },
                        SetItem::MapMerge { variable, value } => LogicalOp::SetProperties {
                            input: Box::new(current),
                            variable: variable.clone(),
                            value: value.clone(),
                            merge: true,
                        },
                    };
                }
                op = Some(current);
            }
            Clause::Remove { items } => {
                let input = op
                    .take()
                    .ok_or_else(|| GraphError::semantic("REMOVE requires preceding MATCH"))?;
                op = Some(LogicalOp::Remove {
                    input: Box::new(input),
                    items: items.clone(),
                });
            }
            Clause::Call {
                procedure_name,
                args,
                implicit_args,
                yield_items,
                yield_star,
            } => {
                // In multi-clause context, CALL acts as a pipeline operator.
                // It takes input from prior clauses (or SingleRow if first).
                let input = op.take().unwrap_or(LogicalOp::SingleRow);

                // Resolve yield items: for now, just pass them through.
                // Full validation happens at runtime via ExecContext procedures.
                let resolved_yields: Vec<(String, Option<String>)> = if *yield_star {
                    // YIELD * — will be resolved at runtime.
                    Vec::new()
                } else if let Some(items) = yield_items {
                    // Check for duplicate yield aliases.
                    let mut bound_names: HashSet<String> = HashSet::new();
                    for (col, alias) in items {
                        let bind_name = alias.as_deref().unwrap_or(col.as_str());
                        // VariableAlreadyBound: yield alias conflicts with prior scope.
                        if scope_vars.contains(bind_name) {
                            return Err(GraphError::syntax(format!(
                                "variable `{bind_name}` already declared",
                            ))
                            .with_code(ErrorCode::VariableAlreadyBound));
                        }
                        if !bound_names.insert(bind_name.to_string()) {
                            return Err(GraphError::syntax(format!(
                                "variable `{bind_name}` already declared",
                            ))
                            .with_code(ErrorCode::VariableAlreadyBound));
                        }
                    }
                    items.clone()
                } else {
                    // No YIELD — outputs not in scope for downstream.
                    Vec::new()
                };

                // InvalidArgumentPassingMode: implicit args with YIELD in multi-clause.
                if *implicit_args && (yield_items.is_some() || *yield_star) {
                    return Err(GraphError::syntax(
                        "implicit argument passing is not allowed with YIELD",
                    )
                    .with_code(ErrorCode::InvalidArgumentPassingMode));
                }

                // InvalidAggregation: aggregate function in CALL argument.
                for arg in args {
                    if is_aggregate_fn(arg) {
                        return Err(GraphError::syntax(
                            "aggregation functions are not allowed in CALL arguments",
                        )
                        .with_code(ErrorCode::InvalidAggregation));
                    }
                }

                // Add yielded columns to scope.
                for (col, alias) in &resolved_yields {
                    let bind_name = alias.as_ref().unwrap_or(col);
                    scope_vars.insert(bind_name.clone());
                }

                op = Some(LogicalOp::Call {
                    input: Box::new(input),
                    procedure_name: procedure_name.clone(),
                    args: args.clone(),
                    yield_items: resolved_yields,
                    yield_star: *yield_star,
                });
            }
            Clause::Delete { exprs, detach } => {
                // Validate DELETE expression references are in scope.
                validate_delete_exprs(exprs, &scope_vars)?;
                let input = op
                    .take()
                    .ok_or_else(|| GraphError::semantic("DELETE requires preceding MATCH"))?;
                op = Some(LogicalOp::Delete {
                    input: Box::new(input),
                    exprs: exprs.clone(),
                    detach: *detach,
                });
            }
        }
    }

    let mut result = op.unwrap_or(LogicalOp::EmptyRow);

    if let Some(ref rc) = stmt.return_clause {
        // Validate RETURN references only in-scope variables.
        if !scope_vars.is_empty() {
            validate_return_variables(&rc.items, &scope_vars)?;
        }
        result =
            apply_return_projection(conn, result, rc, &stmt.order_by, &stmt.skip, &stmt.limit)?;
    }

    Ok(result)
}

/// Plan a WITH clause as an intermediate projection (+aggregation) and optional filter.
/// Replace variable references that match WITH alias names with the original
/// expressions. This allows WITH WHERE to filter before projection while
/// correctly resolving aliases like `WITH n.age AS age WHERE age > 25`.
pub(in crate::cypher::planner) fn substitute_aliases(
    expr: &Expr,
    aliases: &std::collections::HashMap<String, Expr>,
) -> Expr {
    match &expr.kind {
        ExprKind::Variable(name) => {
            if let Some(original) = aliases.get(name) {
                original.clone()
            } else {
                expr.clone()
            }
        }
        ExprKind::BinaryOp { left, op, right } => Expr::synthetic(ExprKind::BinaryOp {
            left: Box::new(substitute_aliases(left, aliases)),
            op: *op,
            right: Box::new(substitute_aliases(right, aliases)),
        }),
        ExprKind::Not(inner) => {
            Expr::synthetic(ExprKind::Not(Box::new(substitute_aliases(inner, aliases))))
        }
        ExprKind::IsNull(inner) => Expr::synthetic(ExprKind::IsNull(Box::new(substitute_aliases(
            inner, aliases,
        )))),
        ExprKind::IsNotNull(inner) => Expr::synthetic(ExprKind::IsNotNull(Box::new(
            substitute_aliases(inner, aliases),
        ))),
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => Expr::synthetic(ExprKind::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| substitute_aliases(a, aliases))
                .collect(),
            distinct: *distinct,
            original_text: original_text.clone(),
        }),
        _ => expr.clone(),
    }
}

pub(in crate::cypher::planner) fn plan_with(
    conn: &Connection,
    input: LogicalOp,
    with: &WithClause,
) -> crate::types::Result<LogicalOp> {
    plan_with_scoped(conn, input, with, None)
}

pub(in crate::cypher::planner) fn plan_with_scoped(
    conn: &Connection,
    input: LogicalOp,
    with: &WithClause,
    input_scope: Option<&HashSet<String>>,
) -> crate::types::Result<LogicalOp> {
    let mut op = input;

    check_duplicate_columns(&with.items)?;

    // WITH requires aliases on non-variable expressions.
    for item in &with.items {
        if item.alias.is_none()
            && !matches!(
                item.expr.kind,
                ExprKind::Variable(_) | ExprKind::Star | ExprKind::Property(_, _)
            )
        {
            return Err(GraphError::syntax(
                "expression in WITH must be aliased (use AS)".to_string(),
            )
            .with_code(ErrorCode::NoExpressionAlias));
        }
    }

    // Check if WITH items contain aggregates.
    let has_aggregates = with.items.iter().any(|item| is_aggregate_fn(&item.expr));

    if has_aggregates {
        let (group_keys, aggregates) = split_aggregates(&with.items)?;
        op = LogicalOp::Aggregate {
            input: Box::new(op),
            group_keys,
            aggregates,
        };
        // Sort between Aggregate and Project so ORDER BY can reference
        // pre-projection aggregate columns (group keys are named by
        // expr_to_column_name, matching the original expression).
        if !with.order_by.is_empty() {
            // For aggregate WITH, non-aggregate leaves in ORDER BY must
            // reference group key variables or projected aliases only.
            validate_agg_order_by_scope(&with.items, &with.order_by)?;
            // Reject ORDER BY with aggregation not in the projection.
            let mut projected_agg_cols: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for item in &with.items {
                if is_aggregate_fn(&item.expr) {
                    projected_agg_cols.insert(crate::cypher::eval::expr_to_column_name(&item.expr));
                    if let Some(ref alias) = item.alias {
                        projected_agg_cols.insert(alias.clone());
                    }
                }
            }
            for sort_item in &with.order_by {
                let mut agg_calls = Vec::new();
                collect_aggregate_calls(&sort_item.expr, &mut agg_calls);
                for agg_expr in &agg_calls {
                    let agg_col = crate::cypher::eval::expr_to_column_name(agg_expr);
                    if !projected_agg_cols.contains(&agg_col) {
                        return Err(GraphError::syntax(
                            "ORDER BY contains an aggregation that is not projected in WITH"
                                .to_string(),
                        )
                        .with_code(ErrorCode::UndefinedVariable));
                    }
                }
            }
            // Don't resolve aliases here — aggregate results are stored
            // under alias names, so ORDER BY c evaluates directly against "c".
            op = LogicalOp::Sort {
                input: Box::new(op),
                items: with.order_by.clone(),
            };
        }
    }

    if let Some(ref predicate) = with.where_clause {
        if has_aggregates {
            // When WITH has aggregates, filter AFTER projection because
            // aggregate results are only available after Aggregate + Project.
            op = LogicalOp::Project {
                input: Box::new(op),
                items: with.items.clone(),
                emit_compound: false,
            };
            op = LogicalOp::Filter {
                input: Box::new(op),
                predicate: predicate.clone(),
            };
        } else {
            // No aggregates: filter BEFORE projection so WHERE can access
            // pre-projection variables (e.g. WITH c WHERE r IS NULL).
            // Substitute alias names with original expressions so aliases
            // like `WHERE age > 25` (age = n.age) resolve correctly.
            let alias_map: std::collections::HashMap<String, Expr> = with
                .items
                .iter()
                .filter_map(|item| item.alias.as_ref().map(|a| (a.clone(), item.expr.clone())))
                .collect();
            let resolved = substitute_aliases(predicate, &alias_map);
            op = LogicalOp::Filter {
                input: Box::new(op),
                predicate: resolved,
            };
            op = LogicalOp::Project {
                input: Box::new(op),
                items: with.items.clone(),
                emit_compound: false,
            };
        }
    } else {
        // No WHERE — Sort before Project so ORDER BY can reference
        // pre-projection variables (e.g. ORDER BY a.name when WITH projects a.name AS name).
        if !with.order_by.is_empty() && !has_aggregates {
            validate_no_aggregation_in_order_by(&with.order_by)?;
            if let Some(scope) = input_scope {
                validate_with_order_by_scope_input(scope, &with.items, &with.order_by)?;
            }
            let resolved: Vec<SortItem> = with
                .order_by
                .iter()
                .map(|si| SortItem {
                    expr: resolve_sort_aliases(&si.expr, &with.items),
                    descending: si.descending,
                })
                .collect();
            op = LogicalOp::Sort {
                input: Box::new(op),
                items: resolved,
            };
        }
        op = LogicalOp::Project {
            input: Box::new(op),
            items: with.items.clone(),
            emit_compound: false,
        };
    }

    // Apply DISTINCT.
    if with.distinct {
        op = LogicalOp::Distinct {
            input: Box::new(op),
        };
    }

    // Note: ORDER BY for both aggregate and non-aggregate cases is handled
    // above (before Project) so sort expressions can reference pre-projection
    // variables/columns.

    // Apply SKIP.
    if let Some(ref expr) = with.skip {
        let count = eval_skip_limit(expr, conn)?;
        op = LogicalOp::Skip {
            input: Box::new(op),
            count,
        };
    }

    // Apply LIMIT.
    if let Some(ref expr) = with.limit {
        let count = eval_skip_limit(expr, conn)?;
        op = LogicalOp::Limit {
            input: Box::new(op),
            count,
        };
    }

    Ok(op)
}

/// Plan an intermediate MATCH clause with an explicit set of already-bound variables.
pub(in crate::cypher::planner) fn plan_intermediate_match_with_scope(
    conn: &Connection,
    input: LogicalOp,
    im: &IntermediateMatch,
    upstream_vars: &HashSet<String>,
) -> crate::types::Result<LogicalOp> {
    let mut op = input;

    // Check for VariableAlreadyBound: a named path variable (p = ...)
    // cannot reuse a variable already bound in a prior scope.
    for pattern in &im.patterns {
        if let Some(ref path_var) = pattern.path_variable {
            if upstream_vars.contains(path_var) {
                return Err(
                    GraphError::syntax(format!("variable `{path_var}` already defined"))
                        .with_code(ErrorCode::VariableAlreadyBound),
                );
            }
        }
    }

    if !im.patterns.is_empty() {
        let right = plan_patterns(conn, &im.patterns)?;
        op = LogicalOp::CorrelatedJoin {
            input: Box::new(op),
            right: Box::new(right),
            same_match: false,
        };
    }

    // Collect variables bound so far for optional patterns.
    // Include both upstream (from WITH) and intermediate MATCH patterns.
    let mut bound_vars = upstream_vars.clone();
    bound_vars.extend(collect_pattern_variables(&im.patterns));

    for opt_match in &im.optional_patterns {
        let (right, new_aliases, opt_filter) = plan_optional_match(conn, opt_match, &bound_vars)?;
        op = LogicalOp::LeftOuterJoin {
            input: Box::new(op),
            right: Box::new(right),
            optional_aliases: new_aliases.clone(),
            opt_filter,
        };
        bound_vars.extend(new_aliases);
    }

    if let Some(ref predicate) = im.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    Ok(op)
}
