//! Statement-level planners — plan_inner dispatch + plan_match/return/create/delete/set/remove/unwind/merge/match_create/match_merge plus plan_call.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::types::*;

use super::helpers::*;
use super::multi::*;
use super::pattern::*;
use super::validation::*;
use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::cypher::planner) fn plan_call(
    conn: &Connection,
    procedure_name: &str,
    args: &[Expr],
    implicit_args: bool,
    yield_items: Option<&[(String, Option<String>)]>,
    yield_star: bool,
    return_clause: Option<&ReturnClause>,
    order_by: &[SortItem],
    skip: Option<&Expr>,
    limit: Option<&Expr>,
    procedures: &crate::cypher::procedure::ProcedureRegistry,
    params: Option<&std::collections::HashMap<String, Value>>,
) -> crate::types::Result<LogicalOp> {
    // 1. ProcedureNotFound — look up procedure in the test registry,
    //    then fall back to the built-in allowlist.
    let registry_def = procedures.get(procedure_name).cloned();
    let proc_def_owned = match registry_def {
        Some(def) => def,
        None => match crate::cypher::builtin_procedures::builtin_signature(procedure_name) {
            Some(sig) => crate::cypher::procedure::ProcedureDef {
                name: procedure_name.to_string(),
                inputs: sig.inputs,
                outputs: sig.outputs,
                rows: Vec::new(),
            },
            None => {
                return Err(GraphError::Query(
                    crate::types::QueryError::ProcedureError {
                        phase: crate::types::QueryPhase::SemanticAnalysis,
                        message: format!("unknown procedure `{procedure_name}`"),
                        code: ErrorCode::ProcedureNotFound,
                        hint: None,
                        span: None,
                    },
                ));
            }
        },
    };
    let proc_def = &proc_def_owned;

    // Determine if this is an in-query CALL (has RETURN or YIELD with RETURN).
    let is_in_query = return_clause.is_some();

    // 8. UnexpectedSyntax — YIELD * in an in-query context.
    if yield_star && is_in_query {
        return Err(
            GraphError::syntax("YIELD * is not allowed in an in-query CALL")
                .with_code(ErrorCode::UnexpectedSyntax),
        );
    }

    // 3. InvalidArgumentPassingMode — implicit args with YIELD (in-query form).
    if implicit_args && (yield_items.is_some() || yield_star) {
        return Err(
            GraphError::syntax("implicit argument passing is not allowed with YIELD")
                .with_code(ErrorCode::InvalidArgumentPassingMode),
        );
    }

    // 7. MissingParameter — implicit args, missing required parameter.
    // With implicit args, the arguments come from parameters with the same name
    // as the procedure inputs.
    let resolved_args: Vec<Expr> = if implicit_args && !proc_def.inputs.is_empty() {
        // Build args from parameters by matching input parameter names.
        let empty_params = std::collections::HashMap::new();
        let param_map = params.unwrap_or(&empty_params);
        let mut built_args = Vec::new();
        for input_param in &proc_def.inputs {
            if let Some(val) = param_map.get(&input_param.name) {
                built_args.push(value_to_literal_expr(val));
            } else {
                return Err(GraphError::syntax(
                    "implicit argument passing requires all procedure parameters to be provided",
                )
                .with_code(ErrorCode::MissingParameter));
            }
        }
        built_args
    } else {
        args.to_vec()
    };
    let args = &resolved_args[..];

    if !implicit_args {
        // 2. InvalidNumberOfArguments — wrong arg count.
        if args.len() != proc_def.inputs.len() {
            return Err(GraphError::syntax(format!(
                "expected {} argument(s) but got {}",
                proc_def.inputs.len(),
                args.len()
            ))
            .with_code(ErrorCode::InvalidNumberOfArguments));
        }

        // 6. InvalidAggregation — aggregate function in CALL argument.
        for arg in args {
            if is_aggregate_fn(arg) {
                return Err(GraphError::syntax(
                    "aggregation functions are not allowed in CALL arguments",
                )
                .with_code(ErrorCode::InvalidAggregation));
            }
        }

        // 4. InvalidArgumentType — wrong literal type for parameter.
        for (arg, param) in args.iter().zip(&proc_def.inputs) {
            if let Some(err) = check_arg_type(arg, &param.type_name) {
                return Err(GraphError::syntax(err));
            }
        }
    }

    // Resolve yield items.
    let resolved_yields: Vec<(String, Option<String>)> = if yield_star {
        // YIELD * — emit all output columns.
        proc_def
            .outputs
            .iter()
            .map(|p| (p.name.clone(), None))
            .collect()
    } else if let Some(items) = yield_items {
        // 5. VariableAlreadyBound — duplicate yield aliases.
        let mut bound_names: HashSet<String> = HashSet::new();
        for (col, alias) in items {
            let bind_name = alias.as_deref().unwrap_or(col);
            if !bound_names.insert(bind_name.to_string()) {
                return Err(GraphError::syntax(
                    format!("variable `{bind_name}` already declared",),
                )
                .with_code(ErrorCode::VariableAlreadyBound));
            }
        }
        items.to_vec()
    } else if is_in_query {
        // In-query CALL without YIELD — outputs are NOT in scope.
        Vec::new()
    } else {
        // Standalone CALL without YIELD — emit all output columns.
        proc_def
            .outputs
            .iter()
            .map(|p| (p.name.clone(), None))
            .collect()
    };

    let mut plan = LogicalOp::Call {
        input: Box::new(LogicalOp::SingleRow),
        procedure_name: procedure_name.to_string(),
        args: args.to_vec(),
        yield_items: resolved_yields.clone(),
        yield_star,
    };

    // If there's a RETURN clause, add projection.
    if let Some(ret) = return_clause {
        // Check that RETURN references are valid — they must be yielded columns.
        let yielded_names: HashSet<String> = resolved_yields
            .iter()
            .map(|(col, alias)| alias.as_ref().unwrap_or(col).clone())
            .collect();

        // Handle RETURN * — expand to all yielded columns.
        let return_items = if ret.items.len() == 1
            && matches!(ret.items[0].expr.kind, ExprKind::Variable(ref v) if v == "*")
        {
            resolved_yields
                .iter()
                .map(|(col, alias)| ReturnItem {
                    expr: Expr::synthetic(ExprKind::Variable(
                        alias.as_ref().unwrap_or(col).clone(),
                    )),
                    alias: None,
                })
                .collect()
        } else {
            // Validate that RETURN variables are in scope.
            for item in &ret.items {
                check_return_vars_in_scope(&item.expr, &yielded_names)?;
            }
            ret.items.clone()
        };

        plan = LogicalOp::Project {
            input: Box::new(plan),
            items: return_items,
            emit_compound: true,
        };

        if ret.distinct {
            plan = LogicalOp::Distinct {
                input: Box::new(plan),
            };
        }

        // Add ORDER BY, SKIP, LIMIT if present.
        if !order_by.is_empty() {
            plan = LogicalOp::Sort {
                input: Box::new(plan),
                items: order_by.to_vec(),
            };
        }
        if let Some(s) = skip {
            plan = LogicalOp::Skip {
                input: Box::new(plan),
                count: eval_skip_limit(s, conn)?,
            };
        }
        if let Some(l) = limit {
            plan = LogicalOp::Limit {
                input: Box::new(plan),
                count: eval_skip_limit(l, conn)?,
            };
        }
    }

    Ok(plan)
}

/// Convert a runtime Value to a literal Expr for implicit argument injection.
pub(in crate::cypher::planner) fn value_to_literal_expr(val: &Value) -> Expr {
    match val {
        Value::I64(n) => Expr::synthetic(ExprKind::Literal(LiteralValue::I64(*n))),
        Value::F64(f) => Expr::synthetic(ExprKind::Literal(LiteralValue::F64(*f))),
        Value::String(s) => Expr::synthetic(ExprKind::Literal(LiteralValue::String(s.clone()))),
        Value::Bool(b) => Expr::synthetic(ExprKind::Literal(LiteralValue::Bool(*b))),
        Value::Null => Expr::synthetic(ExprKind::Literal(LiteralValue::Null)),
        _ => Expr::synthetic(ExprKind::Literal(LiteralValue::Null)), // Fallback for complex types
    }
}

/// Check that a literal argument is type-compatible with a procedure parameter.
/// Returns Some(error_message) if incompatible, None if OK.
pub(in crate::cypher::planner) fn check_arg_type(arg: &Expr, type_name: &str) -> Option<String> {
    // Strip trailing `?` for nullable types.
    let base_type = type_name.trim_end_matches('?').to_ascii_uppercase();

    match &arg.kind {
        ExprKind::Literal(LiteralValue::Null) => None, // null is compatible with any type
        ExprKind::Literal(LiteralValue::I64(_)) => match base_type.as_str() {
            "INTEGER" | "NUMBER" | "FLOAT" | "ANY" => None,
            _ => Some(format!(
                "InvalidArgumentType: expected {type_name} but got INTEGER"
            )),
        },
        ExprKind::Literal(LiteralValue::F64(_)) => match base_type.as_str() {
            "FLOAT" | "NUMBER" | "ANY" => None,
            _ => Some(format!(
                "InvalidArgumentType: expected {type_name} but got FLOAT"
            )),
        },
        ExprKind::Literal(LiteralValue::String(_)) => match base_type.as_str() {
            "STRING" | "ANY" => None,
            _ => Some(format!(
                "InvalidArgumentType: expected {type_name} but got STRING"
            )),
        },
        ExprKind::Literal(LiteralValue::Bool(_)) => match base_type.as_str() {
            "BOOLEAN" | "ANY" => None,
            _ => Some(format!(
                "InvalidArgumentType: expected {type_name} but got BOOLEAN"
            )),
        },
        // Non-literal expressions (variables, function calls) — can't type-check at compile time.
        _ => None,
    }
}

/// Check that all variables in a RETURN expression are in the yielded scope.
pub(in crate::cypher::planner) fn check_return_vars_in_scope(
    expr: &Expr,
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    match &expr.kind {
        ExprKind::Variable(name) => {
            if !scope.contains(name) {
                return Err(
                    GraphError::syntax(format!("variable `{name}` not defined",))
                        .with_code(ErrorCode::UndefinedVariable),
                );
            }
            Ok(())
        }
        ExprKind::Property(var, _) => {
            if !scope.contains(var) {
                return Err(GraphError::syntax(format!("variable `{var}` not defined",))
                    .with_code(ErrorCode::UndefinedVariable));
            }
            Ok(())
        }
        ExprKind::BinaryOp { left, right, .. } => {
            check_return_vars_in_scope(left, scope)?;
            check_return_vars_in_scope(right, scope)
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            check_return_vars_in_scope(inner, scope)
        }
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                check_return_vars_in_scope(arg, scope)?;
            }
            Ok(())
        }
        ExprKind::List(items) => {
            for item in items {
                check_return_vars_in_scope(item, scope)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub(in crate::cypher::planner) fn plan_inner(
    conn: &Connection,
    stmt: &Statement,
    subquery: bool,
) -> crate::types::Result<LogicalOp> {
    match stmt {
        Statement::Match(m) => plan_match(conn, m, subquery),
        Statement::Create(c) => plan_create(conn, c),
        Statement::MatchCreate(mc) => plan_match_create(conn, mc),
        Statement::Delete(d) => plan_delete(conn, d),
        Statement::Set(s) => plan_set(conn, s),
        Statement::Remove(r) => plan_remove(conn, r),
        Statement::Merge(m) => plan_merge(conn, m),
        Statement::MatchMerge(mm) => plan_match_merge(conn, mm),
        Statement::Unwind(u) => plan_unwind(conn, u),
        Statement::Return(r) => plan_return(conn, r),
        Statement::MultiClause(mc) => plan_multi_clause(conn, mc),
        Statement::Call { procedure_name, .. } => Err(GraphError::Query(
            crate::types::QueryError::ProcedureError {
                phase: crate::types::QueryPhase::SemanticAnalysis,
                message: format!("unknown procedure `{procedure_name}`"),
                code: ErrorCode::ProcedureNotFound,
                hint: None,
                span: None,
            },
        )),
        Statement::Explain(inner) => plan_inner(conn, inner, subquery),
        Statement::Union { statements, all } => {
            // Validate that all branches have the same column names.
            let columns: Vec<Vec<String>> =
                statements.iter().map(statement_return_columns).collect();
            if columns.len() >= 2 {
                let first = &columns[0];
                for cols in &columns[1..] {
                    if cols != first {
                        return Err(GraphError::syntax(
                            "all sub queries in a UNION must have the same column names"
                                .to_string(),
                        )
                        .with_code(ErrorCode::DifferentColumnsInUnion));
                    }
                }
            }
            let inputs: crate::types::Result<Vec<LogicalOp>> = statements
                .iter()
                .map(|s| plan_inner(conn, s, subquery))
                .collect();
            Ok(LogicalOp::Union {
                inputs: inputs?,
                all: *all,
            })
        }
    }
}

pub(in crate::cypher::planner) fn plan_match(
    conn: &Connection,
    stmt: &MatchStatement,
    subquery: bool,
) -> crate::types::Result<LogicalOp> {
    // Validate variable-type consistency across patterns before planning.
    let mut var_types = validate_variable_types(&stmt.patterns)?;

    // Validate function argument types against known variable kinds.
    for item in &stmt.return_clause.items {
        validate_expr_types(&item.expr, &var_types)?;
    }

    // Build scan + expand chain from patterns.
    let mut op = if stmt.patterns.is_empty() {
        LogicalOp::SingleRow
    } else {
        plan_patterns(conn, &stmt.patterns)?
    };

    // Collect variables bound by the required MATCH so OPTIONAL MATCH can
    // distinguish shared vs. new aliases (instead of assuming first = shared).
    let mut bound_vars = collect_pattern_variables(&stmt.patterns);

    // Apply OPTIONAL MATCH clauses as LeftOuterJoins.
    for opt_match in &stmt.optional_patterns {
        let (right, new_aliases, opt_filter) = plan_optional_match(conn, opt_match, &bound_vars)?;
        op = LogicalOp::LeftOuterJoin {
            input: Box::new(op),
            right: Box::new(right),
            optional_aliases: new_aliases.clone(),
            opt_filter,
        };
        // Each OPTIONAL MATCH introduces new variables that become bound for
        // subsequent OPTIONAL MATCH clauses.
        bound_vars.extend(new_aliases);
    }

    // Reject aggregation functions in WHERE clause and validate variables.
    if let Some(ref predicate) = stmt.where_clause {
        if is_aggregate_fn(predicate) {
            return Err(GraphError::syntax(
                "aggregation functions are not allowed in WHERE".to_string(),
            )
            .with_code(ErrorCode::InvalidAggregation));
        }
        // Reject using a node/relationship variable as a boolean predicate.
        if let ExprKind::Variable(var) = &predicate.kind {
            if let Some(kind) = var_types.get(var) {
                if *kind == VarKind::Node || *kind == VarKind::Relationship {
                    return Err(GraphError::type_error(
                        crate::types::QueryPhase::SemanticAnalysis,
                        "a single node pattern is not a valid predicate".to_string(),
                    )
                    .with_code(ErrorCode::InvalidArgumentType));
                }
            }
        }
        check_expr_variables(predicate, &bound_vars)?;
        validate_expr_types(predicate, &var_types)?;
        // Only validate pattern predicate vars for top-level queries.
        // Subqueries (EXISTS) can reference outer scope variables.
        if !subquery {
            validate_pattern_predicate_vars(predicate, &bound_vars)?;
        }
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

    // Track scope variables for validation.
    let mut scope_vars = bound_vars.clone();
    // Track WITH-bound variable value kinds for VariableTypeConflict detection.
    let mut with_value_kinds: HashMap<String, WithValueKind> = HashMap::new();

    // Apply intermediate clauses (WITH/UNWIND/MATCH).
    for clause in &stmt.intermediate_clauses {
        match clause {
            IntermediateClause::With(with) => {
                // Reject pattern predicates in WITH items.
                for item in &with.items {
                    reject_pattern_predicates(&item.expr)?;
                }
                op = plan_with_scoped(conn, op, with, Some(&scope_vars))?;
                // WITH resets scope to only the projected aliases.
                // Also reset variable type tracking — WITH starts a new scope
                // where variables can be reused with different types.
                scope_vars.clear();
                var_types.clear();
                with_value_kinds.clear();
                for item in &with.items {
                    if let ExprKind::Star = &item.expr.kind {
                        // WITH * keeps all prior variables in scope.
                        scope_vars = bound_vars.clone();
                    } else {
                        let var_name = if let Some(ref alias) = item.alias {
                            scope_vars.insert(alias.clone());
                            alias.clone()
                        } else if let ExprKind::Variable(var) = &item.expr.kind {
                            scope_vars.insert(var.clone());
                            var.clone()
                        } else {
                            continue;
                        };
                        with_value_kinds.insert(var_name, infer_with_value_kind(&item.expr));
                    }
                }
            }
            IntermediateClause::Unwind(unwind) => {
                op = LogicalOp::Unwind {
                    input: Box::new(op),
                    expr: unwind.expr.clone(),
                    alias: unwind.alias.clone(),
                };
                scope_vars.insert(unwind.alias.clone());
            }
            IntermediateClause::Match(im) => {
                // Check VariableTypeConflict: WITH-bound scalars used as node/rel.
                for pat in &im.patterns {
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
                                // Skip var-length rels — they accept list types.
                                if r.var_length.is_none() {
                                    if let Some(ref var) = r.variable {
                                        if let Some(&kind) = with_value_kinds.get(var) {
                                            if kind == WithValueKind::Scalar {
                                                return Err(GraphError::syntax(format!(
                                                    "variable `{var}` already defined as a scalar value"
                                                )).with_code(ErrorCode::VariableTypeConflict));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Validate variable-type consistency for consecutive MATCHes
                // (not separated by WITH, which resets scope).
                validate_variable_types_with_map(&im.patterns, &mut var_types)?;
                // Validate expression types in WHERE (e.g. property access on paths).
                if let Some(ref predicate) = im.where_clause {
                    validate_expr_types(predicate, &var_types)?;
                }
                op = plan_intermediate_match_with_scope(conn, op, im, &scope_vars)?;
                scope_vars.extend(collect_pattern_variables(&im.patterns));
                for opt in &im.optional_patterns {
                    scope_vars.extend(collect_pattern_variables(&opt.patterns));
                }
            }
        }
    }

    // Validate that RETURN items only reference variables in scope.
    validate_return_variables(&stmt.return_clause.items, &scope_vars)?;

    // Reject pattern predicates in RETURN (only valid in WHERE).
    for item in &stmt.return_clause.items {
        reject_pattern_predicates(&item.expr)?;
    }

    // Check for aggregation in list comprehensions.
    for item in &stmt.return_clause.items {
        validate_no_aggregation_in_list_comp(&item.expr)?;
    }

    // Check if RETURN contains aggregates.
    let has_aggregates = stmt
        .return_clause
        .items
        .iter()
        .any(|item| is_aggregate_fn(&item.expr));

    if has_aggregates {
        let (group_keys, aggregates) = split_aggregates(&stmt.return_clause.items)?;
        op = LogicalOp::Aggregate {
            input: Box::new(op),
            group_keys,
            aggregates,
        };
    }

    // ORDER BY before projection so sort expressions can reference
    // pre-projection variables (e.g. RETURN n.num AS prop ORDER BY n.num).
    if !stmt.order_by.is_empty() {
        // Reject aggregation in ORDER BY when RETURN has no aggregation.
        if !has_aggregates {
            validate_no_aggregation_in_order_by(&stmt.order_by)?;
        } else {
            // With aggregates: reject ORDER BY that mixes aggregates with
            // non-returned variables.
            validate_return_order_by_with_aggregates(&stmt.return_clause.items, &stmt.order_by)?;
        }
        // Validate DISTINCT + ORDER BY scope.
        validate_distinct_order_by(
            &stmt.return_clause.items,
            &stmt.order_by,
            stmt.return_clause.distinct,
        )?;

        // Resolve alias references: if ORDER BY references a RETURN alias
        // (e.g. `ORDER BY x` where RETURN has `foo.num AS x`), substitute
        // the original expression so the sort evaluates against pre-projection
        // record keys.
        let resolved: Vec<SortItem> = stmt
            .order_by
            .iter()
            .map(|si| SortItem {
                expr: resolve_sort_aliases(&si.expr, &stmt.return_clause.items),
                descending: si.descending,
            })
            .collect();
        op = LogicalOp::Sort {
            input: Box::new(op),
            items: resolved,
        };
    }

    // Project (RETURN).
    op = LogicalOp::Project {
        input: Box::new(op),
        items: stmt.return_clause.items.clone(),
        emit_compound: true,
    };

    // DISTINCT.
    if stmt.return_clause.distinct {
        op = LogicalOp::Distinct {
            input: Box::new(op),
        };
    }

    // SKIP.
    if let Some(ref expr) = stmt.skip {
        let count = eval_skip_limit(expr, conn)?;
        op = LogicalOp::Skip {
            input: Box::new(op),
            count,
        };
    }

    // LIMIT.
    if let Some(ref expr) = stmt.limit {
        let count = eval_skip_limit(expr, conn)?;
        op = LogicalOp::Limit {
            input: Box::new(op),
            count,
        };
    }

    Ok(op)
}

/// Plan a standalone `RETURN` statement (no preceding MATCH).
pub(in crate::cypher::planner) fn plan_return(
    conn: &Connection,
    stmt: &ReturnStatement,
) -> crate::types::Result<LogicalOp> {
    let mut op: LogicalOp = LogicalOp::SingleRow;

    check_duplicate_columns(&stmt.return_clause.items)?;

    // Standalone RETURN has no variables in scope — reject any variable refs.
    let empty_scope = HashSet::new();
    validate_return_variables(&stmt.return_clause.items, &empty_scope)?;

    // Validate function argument types (quantifier type checks, etc.).
    let var_types_empty = HashMap::new();
    for item in &stmt.return_clause.items {
        validate_expr_types(&item.expr, &var_types_empty)?;
    }

    // Check for aggregation in list comprehensions.
    for item in &stmt.return_clause.items {
        validate_no_aggregation_in_list_comp(&item.expr)?;
    }

    let has_aggregates = stmt
        .return_clause
        .items
        .iter()
        .any(|item| is_aggregate_fn(&item.expr));

    if has_aggregates {
        let (group_keys, aggregates) = split_aggregates(&stmt.return_clause.items)?;
        op = LogicalOp::Aggregate {
            input: Box::new(op),
            group_keys,
            aggregates,
        };
    }

    // Sort BEFORE projection so ORDER BY can reference pre-projection variables.
    if !stmt.order_by.is_empty() {
        op = LogicalOp::Sort {
            input: Box::new(op),
            items: stmt.order_by.clone(),
        };
    }

    op = LogicalOp::Project {
        input: Box::new(op),
        items: stmt.return_clause.items.clone(),
        emit_compound: true,
    };

    if stmt.return_clause.distinct {
        op = LogicalOp::Distinct {
            input: Box::new(op),
        };
    }

    if let Some(ref expr) = stmt.skip {
        let count = eval_skip_limit(expr, conn)?;
        op = LogicalOp::Skip {
            input: Box::new(op),
            count,
        };
    }

    if let Some(ref expr) = stmt.limit {
        let count = eval_skip_limit(expr, conn)?;
        op = LogicalOp::Limit {
            input: Box::new(op),
            count,
        };
    }

    Ok(op)
}

pub(in crate::cypher::planner) fn plan_create(
    conn: &Connection,
    stmt: &CreateStatement,
) -> crate::types::Result<LogicalOp> {
    // Validate CREATE patterns: no undefined variables, relationships have type + direction.
    validate_create_patterns(&stmt.patterns, &HashSet::new())?;

    let mut ops = Vec::new();
    let mut seen = HashSet::new();

    for pattern in &stmt.patterns {
        let pattern_ops = plan_create_pattern(pattern, &mut seen)?;
        ops.extend(pattern_ops);
    }

    let mut op = if ops.len() == 1 {
        ops.remove(0)
    } else {
        LogicalOp::CreateSequence { ops }
    };

    if let Some(ref rc) = stmt.return_clause {
        op = apply_return_projection(conn, op, rc, &stmt.order_by, &stmt.skip, &stmt.limit)?;
    }

    Ok(op)
}

pub(in crate::cypher::planner) fn plan_match_create(
    conn: &Connection,
    stmt: &MatchCreateStatement,
) -> crate::types::Result<LogicalOp> {
    let match_vars = collect_pattern_variables(&stmt.patterns);
    // Validate CREATE patterns with MATCH variables in scope.
    validate_create_patterns(&stmt.create_patterns, &match_vars)?;

    let mut op = plan_patterns(conn, &stmt.patterns)?;

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    let mut create_ops = Vec::new();
    let mut seen = HashSet::new();
    for pattern in &stmt.create_patterns {
        create_ops.extend(plan_create_pattern(pattern, &mut seen)?);
    }

    let mut result = LogicalOp::MatchCreate {
        input: Box::new(op),
        create_ops,
    };

    if let Some(ref rc) = stmt.return_clause {
        result =
            apply_return_projection(conn, result, rc, &stmt.order_by, &stmt.skip, &stmt.limit)?;
    }

    Ok(result)
}

pub(in crate::cypher::planner) fn plan_delete(
    conn: &Connection,
    stmt: &DeleteStatement,
) -> crate::types::Result<LogicalOp> {
    // Validate DELETE variables are in scope (including OPTIONAL MATCH vars).
    let mut match_vars = collect_pattern_variables(&stmt.patterns);
    for opt in &stmt.optional_patterns {
        match_vars.extend(collect_pattern_variables(&opt.patterns));
    }
    validate_delete_exprs(&stmt.exprs, &match_vars)?;

    let mut op = plan_patterns(conn, &stmt.patterns)?;

    let mut bound_vars = collect_pattern_variables(&stmt.patterns);
    for opt_match in &stmt.optional_patterns {
        let (right, new_aliases, opt_filter) = plan_optional_match(conn, opt_match, &bound_vars)?;
        op = LogicalOp::LeftOuterJoin {
            input: Box::new(op),
            right: Box::new(right),
            optional_aliases: new_aliases.clone(),
            opt_filter,
        };
        bound_vars.extend(new_aliases);
    }

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    let mut op = LogicalOp::Delete {
        input: Box::new(op),
        exprs: stmt.exprs.clone(),
        detach: stmt.detach,
    };

    if let Some(ref rc) = stmt.return_clause {
        op = apply_return_projection(conn, op, rc, &stmt.order_by, &stmt.skip, &stmt.limit)?;
    }

    Ok(op)
}

pub(in crate::cypher::planner) fn plan_set(
    conn: &Connection,
    stmt: &SetStatement,
) -> crate::types::Result<LogicalOp> {
    // Validate SET variable references are in scope (including OPTIONAL MATCH vars).
    let mut match_vars = collect_pattern_variables(&stmt.patterns);
    for opt in &stmt.optional_patterns {
        match_vars.extend(collect_pattern_variables(&opt.patterns));
    }
    validate_set_variables(&stmt.items, &match_vars)?;

    let mut op = if stmt.patterns.is_empty() {
        LogicalOp::SingleRow
    } else {
        plan_patterns(conn, &stmt.patterns)?
    };

    // Optional MATCH clauses.
    let mut bound_vars = collect_pattern_variables(&stmt.patterns);
    for opt_match in &stmt.optional_patterns {
        let (right, new_aliases, opt_filter) = plan_optional_match(conn, opt_match, &bound_vars)?;
        op = LogicalOp::LeftOuterJoin {
            input: Box::new(op),
            right: Box::new(right),
            optional_aliases: new_aliases.clone(),
            opt_filter,
        };
        bound_vars.extend(new_aliases);
    }

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    // Build chain of SET operations from items.
    for item in &stmt.items {
        op = match item {
            SetItem::Property(a) => LogicalOp::SetProperty {
                input: Box::new(op),
                assignments: vec![a.clone()],
            },
            SetItem::Label { variable, labels } => LogicalOp::SetLabel {
                input: Box::new(op),
                variable: variable.clone(),
                labels: labels.clone(),
            },
            SetItem::MapOverwrite { variable, value } => LogicalOp::SetProperties {
                input: Box::new(op),
                variable: variable.clone(),
                value: value.clone(),
                merge: false,
            },
            SetItem::MapMerge { variable, value } => LogicalOp::SetProperties {
                input: Box::new(op),
                variable: variable.clone(),
                value: value.clone(),
                merge: true,
            },
        };
    }

    // Apply intermediate clauses (WITH/MATCH after SET).
    for clause in &stmt.intermediate_clauses {
        match clause {
            IntermediateClause::With(with) => {
                op = plan_with(conn, op, with)?;
            }
            IntermediateClause::Match(im) => {
                op = plan_intermediate_match_with_scope(conn, op, im, &bound_vars)?;
                bound_vars.extend(collect_pattern_variables(&im.patterns));
            }
            IntermediateClause::Unwind(unwind) => {
                op = LogicalOp::Unwind {
                    input: Box::new(op),
                    expr: unwind.expr.clone(),
                    alias: unwind.alias.clone(),
                };
            }
        }
    }

    if let Some(ref rc) = stmt.return_clause {
        op = apply_return_projection(conn, op, rc, &stmt.order_by, &stmt.skip, &stmt.limit)?;
    }

    Ok(op)
}

pub(in crate::cypher::planner) fn plan_remove(
    conn: &Connection,
    stmt: &RemoveStatement,
) -> crate::types::Result<LogicalOp> {
    let mut op = plan_patterns(conn, &stmt.patterns)?;

    // Collect variables bound by the required MATCH so OPTIONAL MATCH can
    // distinguish shared vs. new aliases.
    let mut bound_vars = collect_pattern_variables(&stmt.patterns);

    // Optional MATCH clauses.
    for opt_match in &stmt.optional_patterns {
        let (right, new_aliases, opt_filter) = plan_optional_match(conn, opt_match, &bound_vars)?;
        op = LogicalOp::LeftOuterJoin {
            input: Box::new(op),
            right: Box::new(right),
            optional_aliases: new_aliases.clone(),
            opt_filter,
        };
        bound_vars.extend(new_aliases);
    }

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    op = LogicalOp::Remove {
        input: Box::new(op),
        items: stmt.items.clone(),
    };

    if let Some(ref rc) = stmt.return_clause {
        op = apply_return_projection(conn, op, rc, &stmt.order_by, &stmt.skip, &stmt.limit)?;
    }

    Ok(op)
}

pub(in crate::cypher::planner) fn plan_unwind(
    conn: &Connection,
    stmt: &UnwindStatement,
) -> crate::types::Result<LogicalOp> {
    let mut op = LogicalOp::Unwind {
        input: Box::new(LogicalOp::EmptyRow),
        expr: stmt.expr.clone(),
        alias: stmt.alias.clone(),
    };

    match &stmt.body {
        UnwindBody::Return {
            where_clause,
            intermediate_clauses,
            return_clause,
            order_by,
            skip,
            limit,
        } => {
            if let Some(ref predicate) = where_clause {
                op = LogicalOp::Filter {
                    input: Box::new(op),
                    predicate: predicate.clone(),
                };
            }

            // Apply intermediate clauses (WITH/UNWIND/MATCH).
            let mut scope_vars: HashSet<String> = HashSet::new();
            scope_vars.insert(stmt.alias.clone());
            for clause in intermediate_clauses {
                match clause {
                    IntermediateClause::With(with) => {
                        op = plan_with(conn, op, with)?;
                        scope_vars.clear();
                        for item in &with.items {
                            if let Some(ref alias) = item.alias {
                                scope_vars.insert(alias.clone());
                            } else if let ExprKind::Variable(var) = &item.expr.kind {
                                scope_vars.insert(var.clone());
                            }
                        }
                    }
                    IntermediateClause::Unwind(unwind) => {
                        op = LogicalOp::Unwind {
                            input: Box::new(op),
                            expr: unwind.expr.clone(),
                            alias: unwind.alias.clone(),
                        };
                        scope_vars.insert(unwind.alias.clone());
                    }
                    IntermediateClause::Match(im) => {
                        op = plan_intermediate_match_with_scope(conn, op, im, &scope_vars)?;
                        scope_vars.extend(collect_pattern_variables(&im.patterns));
                        for opt in &im.optional_patterns {
                            scope_vars.extend(collect_pattern_variables(&opt.patterns));
                        }
                    }
                }
            }

            let has_aggregates = return_clause
                .items
                .iter()
                .any(|item| is_aggregate_fn(&item.expr));

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
                emit_compound: true,
            };

            if return_clause.distinct {
                op = LogicalOp::Distinct {
                    input: Box::new(op),
                };
            }

            if !order_by.is_empty() {
                op = LogicalOp::Sort {
                    input: Box::new(op),
                    items: order_by.clone(),
                };
            }

            if let Some(ref expr) = skip {
                let count = eval_skip_limit(expr, conn)?;
                op = LogicalOp::Skip {
                    input: Box::new(op),
                    count,
                };
            }

            if let Some(ref expr) = limit {
                let count = eval_skip_limit(expr, conn)?;
                op = LogicalOp::Limit {
                    input: Box::new(op),
                    count,
                };
            }
        }
        UnwindBody::Create {
            patterns,
            intermediate_clauses,
            return_clause,
            order_by,
            skip,
            limit,
        } => {
            let mut create_ops = Vec::new();
            let mut seen = HashSet::new();
            for pattern in patterns {
                create_ops.extend(plan_create_pattern(pattern, &mut seen)?);
            }
            op = LogicalOp::MatchCreate {
                input: Box::new(op),
                create_ops,
            };

            // Apply intermediate clauses (WITH/MATCH/UNWIND after CREATE).
            let mut scope_vars2: HashSet<String> = HashSet::new();
            scope_vars2.insert(stmt.alias.clone());
            for clause in intermediate_clauses {
                match clause {
                    IntermediateClause::With(with) => {
                        op = plan_with(conn, op, with)?;
                        scope_vars2.clear();
                        for item in &with.items {
                            if let Some(ref alias) = item.alias {
                                scope_vars2.insert(alias.clone());
                            } else if let ExprKind::Variable(var) = &item.expr.kind {
                                scope_vars2.insert(var.clone());
                            }
                        }
                    }
                    IntermediateClause::Unwind(unwind) => {
                        op = LogicalOp::Unwind {
                            input: Box::new(op),
                            expr: unwind.expr.clone(),
                            alias: unwind.alias.clone(),
                        };
                        scope_vars2.insert(unwind.alias.clone());
                    }
                    IntermediateClause::Match(im) => {
                        op = plan_intermediate_match_with_scope(conn, op, im, &scope_vars2)?;
                        scope_vars2.extend(collect_pattern_variables(&im.patterns));
                        for opt in &im.optional_patterns {
                            scope_vars2.extend(collect_pattern_variables(&opt.patterns));
                        }
                    }
                }
            }

            if let Some(rc) = return_clause {
                op = apply_return_projection(conn, op, rc, order_by, skip, limit)?;
            }
        }
    }

    Ok(op)
}

pub(in crate::cypher::planner) fn plan_merge(
    conn: &Connection,
    stmt: &MergeStatement,
) -> crate::types::Result<LogicalOp> {
    // Validate MERGE pattern + ON CREATE/ON MATCH SET variables.
    validate_merge_pattern(
        &stmt.pattern,
        &HashSet::new(),
        &stmt.on_create,
        &stmt.on_match,
    )?;

    let len = stmt.pattern.elements.len();
    match len {
        // Single node MERGE: MERGE (n:Label {props})
        1 => match stmt.pattern.elements.first() {
            Some(crate::cypher::ast::PatternElement::Node(_)) => {}
            _ => {
                return Err(crate::types::GraphError::semantic(
                    "MERGE pattern must start with a node",
                ));
            }
        },
        // Relationship MERGE: MERGE (a:L {p})-[:TYPE]->(b:L {p})
        3 => {
            use crate::cypher::ast::PatternElement;
            match (
                &stmt.pattern.elements[0],
                &stmt.pattern.elements[1],
                &stmt.pattern.elements[2],
            ) {
                (
                    PatternElement::Node(_),
                    PatternElement::Relationship(rel),
                    PatternElement::Node(_),
                ) => {
                    if rel.var_length.is_some() {
                        return Err(GraphError::syntax(
                            "variable-length relationships are not allowed in MERGE".to_string(),
                        )
                        .with_code(ErrorCode::CreatingVarLength));
                    }
                }
                _ => {
                    return Err(crate::types::GraphError::semantic(
                        "MERGE relationship pattern must be (node)-[rel]->(node)",
                    ));
                }
            }
        }
        _ => {
            return Err(crate::types::GraphError::semantic(
                "MERGE only supports single node or (node)-[rel]->(node) patterns",
            ));
        }
    }
    let mut op = LogicalOp::Merge {
        pattern: stmt.pattern.clone(),
        on_create: stmt.on_create.clone(),
        on_match: stmt.on_match.clone(),
    };

    if let Some(ref rc) = stmt.return_clause {
        op = apply_return_projection(conn, op, rc, &stmt.order_by, &stmt.skip, &stmt.limit)?;
    }

    Ok(op)
}

pub(in crate::cypher::planner) fn plan_match_merge(
    conn: &Connection,
    stmt: &MatchMergeStatement,
) -> crate::types::Result<LogicalOp> {
    let match_vars = collect_pattern_variables(&stmt.patterns);
    // Validate MERGE pattern + ON CREATE/ON MATCH SET variables.
    validate_merge_pattern(
        &stmt.merge_pattern,
        &match_vars,
        &stmt.on_create,
        &stmt.on_match,
    )?;
    // Validate VariableAlreadyBound: MERGE re-creating already-bound nodes/rels.
    validate_merge_variable_rebinding(&stmt.merge_pattern, &match_vars)?;

    let mut op = plan_patterns(conn, &stmt.patterns)?;

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    let mut result = LogicalOp::MatchMerge {
        input: Box::new(op),
        merge_pattern: stmt.merge_pattern.clone(),
        on_create: stmt.on_create.clone(),
        on_match: stmt.on_match.clone(),
    };

    if let Some(ref rc) = stmt.return_clause {
        result =
            apply_return_projection(conn, result, rc, &stmt.order_by, &stmt.skip, &stmt.limit)?;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use crate::cypher::parser::parse;
    use crate::cypher::planner::plan_with_procedures;
    use crate::cypher::procedure::ProcedureRegistry;

    #[test]
    fn plan_call_resolves_db_indexes_built_in() {
        let stmt = parse("CALL db.indexes() YIELD label, property, kind RETURN *").unwrap();
        let registry = ProcedureRegistry::new();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let plan = plan_with_procedures(&conn, &stmt, &registry, None).unwrap();
        let dbg = format!("{plan:?}");
        assert!(dbg.contains("db.indexes"), "plan missing CALL node: {dbg}");
    }
}
