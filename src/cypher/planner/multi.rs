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
            validate_with_order_by_scope_input(&scope_vars, &rc.items, &stmt.order_by)?;
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
            // Aggregate results are stored under their alias, so ORDER BY c
            // evaluates directly against "c". Group keys are stored under
            // their column name (`x % 2`), so an alias of a group key
            // (`x % 2 AS k ... ORDER BY k`) is rewritten to that column.
            let key_columns: Vec<ReturnItem> = with
                .items
                .iter()
                .map(|item| {
                    if is_aggregate_fn(&item.expr) {
                        return item.clone();
                    }
                    ReturnItem {
                        expr: Expr::synthetic(ExprKind::Variable(
                            crate::cypher::eval::expr_to_column_name(&item.expr),
                        )),
                        alias: item.alias.clone(),
                    }
                })
                .collect();
            let resolved: Vec<SortItem> = with
                .order_by
                .iter()
                .map(|si| SortItem {
                    expr: resolve_sort_aliases(&si.expr, &key_columns),
                    descending: si.descending,
                })
                .collect();
            op = LogicalOp::Sort {
                input: Box::new(op),
                items: resolved,
            };
        }
    } else if !with.order_by.is_empty() {
        // Sort before Project so ORDER BY can reference pre-projection
        // variables (e.g. ORDER BY a.name when WITH projects a.name AS name).
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

    // openCypher applies WITH's WHERE after projection, ORDER BY, SKIP and
    // LIMIT, against the projected values (so `WITH rand() AS r WHERE r < 0.1`
    // tests the value it returns). A non-aggregating, non-DISTINCT WITH may
    // also reference variables it does not project (`WITH a.x AS x WHERE
    // a.y > 1`); those conjuncts cannot run after Project, so they run on the
    // pre-projection row with aliases substituted. Projection is 1:1 in that
    // case, so applying SKIP/LIMIT and that filter before Project is
    // equivalent.
    let (pre_where, post_where) = match &with.where_clause {
        Some(predicate) => split_with_where(predicate, with, has_aggregates, input_scope)?,
        None => (None, None),
    };

    if let Some(pre) = pre_where {
        op = apply_with_skip_limit(conn, op, with)?;
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: pre,
        };
        op = LogicalOp::Project {
            input: Box::new(op),
            items: with.items.clone(),
            emit_compound: false,
        };
    } else {
        op = LogicalOp::Project {
            input: Box::new(op),
            items: with.items.clone(),
            emit_compound: false,
        };
        if with.distinct {
            op = LogicalOp::Distinct {
                input: Box::new(op),
            };
        }
        op = apply_with_skip_limit(conn, op, with)?;
    }

    if let Some(post) = post_where {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: post,
        };
    }

    Ok(op)
}

/// Wrap `op` in the WITH clause's SKIP and LIMIT, in that order.
fn apply_with_skip_limit(
    conn: &Connection,
    mut op: LogicalOp,
    with: &WithClause,
) -> crate::types::Result<LogicalOp> {
    if let Some(ref expr) = with.skip {
        let count = eval_skip_limit(expr, conn)?;
        op = LogicalOp::Skip {
            input: Box::new(op),
            count,
        };
    }
    if let Some(ref expr) = with.limit {
        let count = eval_skip_limit(expr, conn)?;
        op = LogicalOp::Limit {
            input: Box::new(op),
            count,
        };
    }
    Ok(op)
}

/// Split a WITH WHERE predicate into `(pre_projection, post_projection)`
/// filters.
///
/// Conjuncts that only read names the WITH projects run after projection
/// against the projected values. Conjuncts that read a variable the WITH
/// drops (or whose scope can't be determined, e.g. pattern predicates) run on
/// the pre-projection row, which is only possible for a non-aggregating,
/// non-DISTINCT WITH; otherwise a dropped variable is an `UndefinedVariable`
/// error. When `input_scope` is known, a dropped variable must exist in it.
fn split_with_where(
    predicate: &Expr,
    with: &WithClause,
    has_aggregates: bool,
    input_scope: Option<&HashSet<String>>,
) -> crate::types::Result<(Option<Expr>, Option<Expr>)> {
    if with
        .items
        .iter()
        .any(|item| matches!(item.expr.kind, ExprKind::Star))
    {
        // `WITH *` projects every input variable.
        return Ok((None, Some(predicate.clone())));
    }
    let mut projected: HashSet<String> = HashSet::new();
    let mut projected_props: HashSet<(String, String)> = HashSet::new();
    for item in &with.items {
        match (&item.alias, &item.expr.kind) {
            (Some(alias), _) => {
                projected.insert(alias.clone());
            }
            (None, ExprKind::Variable(v)) => {
                projected.insert(v.clone());
            }
            (None, ExprKind::Property(v, p)) => {
                projected_props.insert((v.clone(), p.clone()));
            }
            _ => {}
        }
    }

    let can_filter_before_projection = !has_aggregates && !with.distinct;
    let mut pre: Vec<Expr> = Vec::new();
    let mut post: Vec<Expr> = Vec::new();
    for conjunct in flatten_top_level_and(predicate) {
        let mut refs = WhereRefs::default();
        collect_where_refs(conjunct, &projected, &projected_props, &mut refs);
        if refs.dropped.is_empty() {
            // An opaque form (pattern predicate, EXISTS) can run before
            // projection only when every variable it names exists in the
            // pre-projection row. A renamed or computed alias (`a AS z`,
            // `m.k AS mk`) exists only after projection, so such a conjunct
            // must run on the projected row.
            if refs.opaque
                && can_filter_before_projection
                && !mentions_computed_alias(conjunct, with)
            {
                pre.push(conjunct.clone());
            } else {
                post.push(conjunct.clone());
            }
            continue;
        }
        if !can_filter_before_projection {
            // After DISTINCT / aggregation only projected values survive, but
            // a projected expression may be repeated verbatim
            // (`WITH DISTINCT a.x AS x WHERE a.x = 'B'`): read its alias.
            let rewritten = alias_projected_exprs(conjunct, &with.items);
            let mut refs = WhereRefs::default();
            collect_where_refs(&rewritten, &projected, &projected_props, &mut refs);
            if let Some((name, span)) = refs.dropped.first() {
                return Err(undefined_variable_error(name, &projected, *span));
            }
            post.push(rewritten);
            continue;
        }
        if let Some(scope) = input_scope {
            if let Some((name, span)) = refs.dropped.iter().find(|(n, _)| !scope.contains(n)) {
                return Err(undefined_variable_error(name, scope, *span));
            }
        }
        pre.push(conjunct.clone());
    }

    let alias_map: HashMap<String, Expr> = with
        .items
        .iter()
        .filter_map(|item| item.alias.as_ref().map(|a| (a.clone(), item.expr.clone())))
        .collect();
    let pre =
        rebuild_and(&pre.iter().collect::<Vec<_>>()).map(|e| substitute_aliases(&e, &alias_map));
    Ok((pre, rebuild_and(&post.iter().collect::<Vec<_>>())))
}

/// True when `expr` names a WITH alias whose value is not simply the
/// same-named variable (`a AS z`, `m.k AS mk`), so the alias exists only in the
/// projected row.
fn mentions_computed_alias(expr: &Expr, with: &WithClause) -> bool {
    let mut mentioned = MentionedNames::default();
    mentioned.expr(expr);
    with.items.iter().any(|item| {
        let Some(alias) = &item.alias else {
            return false;
        };
        if matches!(&item.expr.kind, ExprKind::Variable(v) if v == alias) {
            return false;
        }
        mentioned.unknown || mentioned.names.contains(alias)
    })
}

/// Every variable name an expression mentions, including names bound or used
/// inside patterns, comprehensions and EXISTS subqueries (which
/// `collect_where_refs` treats as opaque). Locally bound names are included:
/// over-reporting a name only moves a filter after projection.
#[derive(Default)]
struct MentionedNames {
    names: HashSet<String>,
    /// A construct whose variables were not walked (a statement kind that can
    /// not appear in an EXISTS body); callers must assume any name is used.
    unknown: bool,
}

impl MentionedNames {
    fn name(&mut self, n: &str) {
        self.names.insert(n.to_string());
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Literal(_) | ExprKind::Parameter(_) | ExprKind::Star => {}
            ExprKind::Variable(v) | ExprKind::Property(v, _) | ExprKind::HasLabel(v, _) => {
                self.name(v)
            }
            ExprKind::BinaryOp { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            ExprKind::Not(x)
            | ExprKind::IsNull(x)
            | ExprKind::IsNotNull(x)
            | ExprKind::DotAccess { expr: x, .. } => self.expr(x),
            ExprKind::FunctionCall { args, .. } => args.iter().for_each(|a| self.expr(a)),
            ExprKind::List(items) => items.iter().for_each(|a| self.expr(a)),
            ExprKind::MapLiteral(pairs) => pairs.iter().for_each(|(_, v)| self.expr(v)),
            ExprKind::Case {
                operand,
                alternatives,
                default,
            } => {
                operand.iter().for_each(|x| self.expr(x));
                for (cond, result) in alternatives {
                    self.expr(cond);
                    self.expr(result);
                }
                default.iter().for_each(|x| self.expr(x));
            }
            ExprKind::Index { expr, index } => {
                self.expr(expr);
                self.expr(index);
            }
            ExprKind::Slice { expr, start, end } => {
                self.expr(expr);
                start.iter().for_each(|x| self.expr(x));
                end.iter().for_each(|x| self.expr(x));
            }
            ExprKind::ListComprehension {
                variable,
                list_expr,
                filter,
                map_expr,
            } => {
                self.name(variable);
                self.expr(list_expr);
                filter.iter().for_each(|x| self.expr(x));
                map_expr.iter().for_each(|x| self.expr(x));
            }
            ExprKind::Quantifier {
                variable,
                list_expr,
                predicate,
                ..
            } => {
                self.name(variable);
                self.expr(list_expr);
                self.expr(predicate);
            }
            ExprKind::PatternComprehension {
                path_variable,
                pattern,
                where_clause,
                map_expr,
            } => {
                path_variable.iter().for_each(|v| self.name(v));
                self.pattern(pattern);
                where_clause.iter().for_each(|x| self.expr(x));
                self.expr(map_expr);
            }
            ExprKind::Exists {
                patterns,
                where_clause,
            } => {
                patterns.iter().for_each(|p| self.pattern(p));
                where_clause.iter().for_each(|x| self.expr(x));
            }
            ExprKind::ExistsSubquery(stmt) => self.statement(stmt),
            ExprKind::PatternPredicate(pattern) => self.pattern(pattern),
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        pattern.path_variable.iter().for_each(|v| self.name(v));
        for element in &pattern.elements {
            let (variable, properties) = match element {
                PatternElement::Node(n) => (&n.variable, &n.properties),
                PatternElement::Relationship(r) => (&r.variable, &r.properties),
            };
            variable.iter().for_each(|v| self.name(v));
            properties.values().for_each(|x| self.expr(x));
        }
    }

    fn match_parts(&mut self, patterns: &[Pattern], optional: &[OptionalMatch], w: &Option<Expr>) {
        patterns.iter().for_each(|p| self.pattern(p));
        for opt in optional {
            opt.patterns.iter().for_each(|p| self.pattern(p));
            opt.where_clause.iter().for_each(|x| self.expr(x));
        }
        w.iter().for_each(|x| self.expr(x));
    }

    fn with_clause(&mut self, with: &WithClause) {
        for item in &with.items {
            self.expr(&item.expr);
            item.alias.iter().for_each(|a| self.name(a));
        }
        with.order_by.iter().for_each(|si| self.expr(&si.expr));
        for x in [&with.skip, &with.limit, &with.where_clause]
            .into_iter()
            .flatten()
        {
            self.expr(x);
        }
    }

    fn return_tail(
        &mut self,
        ret: &ReturnClause,
        order_by: &[SortItem],
        skip: &Option<Expr>,
        limit: &Option<Expr>,
    ) {
        for item in &ret.items {
            self.expr(&item.expr);
            item.alias.iter().for_each(|a| self.name(a));
        }
        order_by.iter().for_each(|si| self.expr(&si.expr));
        skip.iter().chain(limit.iter()).for_each(|x| self.expr(x));
    }

    /// Walk the statement forms the parser can produce for an EXISTS body.
    fn statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Match(m) => {
                self.match_parts(&m.patterns, &m.optional_patterns, &m.where_clause);
                for clause in &m.intermediate_clauses {
                    match clause {
                        IntermediateClause::With(w) => self.with_clause(w),
                        IntermediateClause::Unwind(u) => {
                            self.expr(&u.expr);
                            self.name(&u.alias);
                        }
                        IntermediateClause::Match(im) => {
                            self.match_parts(&im.patterns, &im.optional_patterns, &im.where_clause)
                        }
                    }
                }
                self.return_tail(&m.return_clause, &m.order_by, &m.skip, &m.limit);
            }
            Statement::MultiClause(mc) => {
                for clause in &mc.clauses {
                    match clause {
                        Clause::Match {
                            patterns,
                            optional_patterns,
                            where_clause,
                        } => self.match_parts(patterns, optional_patterns, where_clause),
                        Clause::With(w) => self.with_clause(w),
                        Clause::Unwind(u) => {
                            self.expr(&u.expr);
                            self.name(&u.alias);
                        }
                        _ => self.unknown = true,
                    }
                }
                if let Some(ret) = &mc.return_clause {
                    self.return_tail(ret, &mc.order_by, &mc.skip, &mc.limit);
                }
            }
            Statement::Union { statements, .. } => {
                statements.iter().for_each(|s| self.statement(s))
            }
            _ => self.unknown = true,
        }
    }
}

/// Replace every subexpression of `expr` that equals an aliased WITH item's
/// expression with a reference to that alias. Covers the operator, function,
/// list and CASE forms; anything else is kept as written.
fn alias_projected_exprs(expr: &Expr, items: &[ReturnItem]) -> Expr {
    for item in items {
        if let Some(alias) = &item.alias {
            if item.expr == *expr {
                return Expr::new(ExprKind::Variable(alias.clone()), expr.span);
            }
        }
    }
    let rec = |e: &Expr| Box::new(alias_projected_exprs(e, items));
    let kind = match &expr.kind {
        ExprKind::BinaryOp { left, op, right } => ExprKind::BinaryOp {
            left: rec(left),
            op: *op,
            right: rec(right),
        },
        ExprKind::Not(e) => ExprKind::Not(rec(e)),
        ExprKind::IsNull(e) => ExprKind::IsNull(rec(e)),
        ExprKind::IsNotNull(e) => ExprKind::IsNotNull(rec(e)),
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => ExprKind::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| alias_projected_exprs(a, items))
                .collect(),
            distinct: *distinct,
            original_text: original_text.clone(),
        },
        ExprKind::List(list) => ExprKind::List(
            list.iter()
                .map(|e| alias_projected_exprs(e, items))
                .collect(),
        ),
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => ExprKind::Case {
            operand: operand.as_deref().map(rec),
            alternatives: alternatives.iter().map(|(c, r)| (rec(c), rec(r))).collect(),
            default: default.as_deref().map(rec),
        },
        _ => return expr.clone(),
    };
    Expr::new(kind, expr.span)
}

/// Variable references found by `collect_where_refs`.
#[derive(Default)]
struct WhereRefs {
    /// Variables read by the predicate that the WITH does not project.
    dropped: Vec<(String, Span)>,
    /// The predicate contains a form whose variable use isn't inspected
    /// (pattern predicates, subqueries).
    opaque: bool,
}

/// Walk `expr`, recording variables outside `projected` (names) and
/// `projected_props` (unaliased `var.prop` items) into `refs`.
fn collect_where_refs(
    expr: &Expr,
    projected: &HashSet<String>,
    projected_props: &HashSet<(String, String)>,
    refs: &mut WhereRefs,
) {
    let mut walk = |e: &Expr| collect_where_refs(e, projected, projected_props, refs);
    match &expr.kind {
        ExprKind::Variable(v) | ExprKind::HasLabel(v, _) => {
            if !projected.contains(v) {
                refs.dropped.push((v.clone(), expr.span));
            }
        }
        ExprKind::Property(v, p) => {
            if !projected.contains(v) && !projected_props.contains(&(v.clone(), p.clone())) {
                refs.dropped.push((v.clone(), expr.span));
            }
        }
        ExprKind::Literal(_) | ExprKind::Parameter(_) | ExprKind::Star => {}
        ExprKind::BinaryOp { left, right, .. } => {
            walk(left);
            walk(right);
        }
        ExprKind::Not(e)
        | ExprKind::IsNull(e)
        | ExprKind::IsNotNull(e)
        | ExprKind::DotAccess { expr: e, .. } => walk(e),
        ExprKind::FunctionCall { args, .. } => args.iter().for_each(walk),
        ExprKind::List(items) => items.iter().for_each(walk),
        ExprKind::MapLiteral(pairs) => pairs.iter().for_each(|(_, v)| walk(v)),
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            operand.iter().for_each(|e| walk(e));
            for (cond, result) in alternatives {
                walk(cond);
                walk(result);
            }
            default.iter().for_each(|e| walk(e));
        }
        ExprKind::Index { expr: e, index } => {
            walk(e);
            walk(index);
        }
        ExprKind::Slice {
            expr: e,
            start,
            end,
        } => {
            walk(e);
            start.iter().for_each(|e| walk(e));
            end.iter().for_each(|e| walk(e));
        }
        ExprKind::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => {
            walk(list_expr);
            let mut inner = projected.clone();
            inner.insert(variable.clone());
            for e in filter.iter().chain(map_expr.iter()) {
                collect_where_refs(e, &inner, projected_props, refs);
            }
        }
        ExprKind::Quantifier {
            variable,
            list_expr,
            predicate,
            ..
        } => {
            walk(list_expr);
            let mut inner = projected.clone();
            inner.insert(variable.clone());
            collect_where_refs(predicate, &inner, projected_props, refs);
        }
        ExprKind::PatternComprehension { .. }
        | ExprKind::Exists { .. }
        | ExprKind::ExistsSubquery(_)
        | ExprKind::PatternPredicate(_) => refs.opaque = true,
    }
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
