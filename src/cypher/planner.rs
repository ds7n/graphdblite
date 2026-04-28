use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::ir::*;
use crate::cypher::record::Record;
use crate::index;
use crate::types::{Direction, GraphError, Value};

/// Global counter for unique anonymous variable aliases across all plan_single_pattern calls.
static ANON_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Evaluate a SKIP/LIMIT expression to a u64 at plan time.
///
/// Handles integer literals, float literals (truncated), and simple function
/// calls like `toInteger(rand()*9)`. Parameters are already resolved to
/// literals before planning.
fn eval_skip_limit(expr: &Expr, conn: &Connection) -> crate::types::Result<u64> {
    match expr {
        Expr::Literal(LiteralValue::I64(n)) => {
            if *n < 0 {
                return Err(GraphError::syntax(
                    "NegativeIntegerArgument: SKIP/LIMIT must be a non-negative integer",
                ));
            }
            Ok(*n as u64)
        }
        Expr::Literal(LiteralValue::F64(_)) => Err(GraphError::type_error(
            crate::types::QueryPhase::Runtime,
            "InvalidArgumentType: SKIP/LIMIT does not accept a floating point value",
        )),
        _ => {
            // Evaluate the expression at plan time with an empty record.
            let rec = Record::new();
            let val = crate::cypher::eval::eval_expr(expr, &rec, conn)?;
            match val {
                Value::I64(n) => {
                    if n < 0 {
                        return Err(GraphError::syntax(
                            "NegativeIntegerArgument: SKIP/LIMIT must be a non-negative integer",
                        ));
                    }
                    Ok(n as u64)
                }
                Value::F64(_) => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "InvalidArgumentType: SKIP/LIMIT does not accept a floating point value",
                )),
                Value::Null => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "InvalidArgumentType: SKIP/LIMIT does not accept NULL",
                )),
                _ => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "InvalidArgumentType: SKIP/LIMIT must evaluate to an integer",
                )),
            }
        }
    }
}

/// Resolve RETURN-alias references within a sort expression.
///
/// Sort happens BEFORE projection, so ORDER BY expressions that reference
/// RETURN aliases (e.g. `ORDER BY x` where `RETURN foo.num AS x`) must be
/// rewritten to use the original expression (`foo.num`).
fn resolve_sort_aliases(expr: &Expr, items: &[ReturnItem]) -> Expr {
    match expr {
        Expr::Variable(name) => {
            for item in items {
                if item.alias.as_deref() == Some(name) {
                    return item.expr.clone();
                }
            }
            expr.clone()
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(resolve_sort_aliases(left, items)),
            op: *op,
            right: Box::new(resolve_sort_aliases(right, items)),
        },
        Expr::Not(inner) => Expr::Not(Box::new(resolve_sort_aliases(inner, items))),
        Expr::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| resolve_sort_aliases(a, items))
                .collect(),
            distinct: *distinct,
            original_text: original_text.clone(),
        },
        _ => expr.clone(),
    }
}

/// Apply RETURN projection (+ DISTINCT, ORDER BY, SKIP, LIMIT) to a plan operator.
fn apply_return_projection(
    conn: &Connection,
    mut op: LogicalOp,
    return_clause: &ReturnClause,
    order_by: &[SortItem],
    skip: &Option<Expr>,
    limit: &Option<Expr>,
) -> crate::types::Result<LogicalOp> {
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

    // Sort BEFORE projection so ORDER BY can reference pre-projection variables.
    // The Project that follows will discard sort-only columns.
    // Resolve alias references: if ORDER BY references a RETURN alias (e.g.
    // `ORDER BY x` where RETURN has `foo.num AS x`), substitute the original
    // expression so the sort can evaluate against the pre-projection record.
    if !order_by.is_empty() {
        let resolved: Vec<SortItem> = order_by
            .iter()
            .map(|si| SortItem {
                expr: resolve_sort_aliases(&si.expr, &return_clause.items),
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
        items: return_clause.items.clone(),
        emit_compound: true,
    };

    if return_clause.distinct {
        op = LogicalOp::Distinct {
            input: Box::new(op),
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

    Ok(op)
}

/// Compile a Cypher AST Statement into a LogicalOp plan.
pub fn plan(conn: &Connection, stmt: &Statement) -> crate::types::Result<LogicalOp> {
    match stmt {
        Statement::Match(m) => plan_match(conn, m),
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
        Statement::Explain(inner) => plan(conn, inner),
        Statement::Union { statements, all } => {
            // Validate that all branches have the same column names.
            let columns: Vec<Vec<String>> =
                statements.iter().map(statement_return_columns).collect();
            if columns.len() >= 2 {
                let first = &columns[0];
                for cols in &columns[1..] {
                    if cols != first {
                        return Err(GraphError::syntax(
                            "DifferentColumnsInUnion: all sub queries in a UNION must have the same column names".to_string(),
                        ));
                    }
                }
            }
            let inputs: crate::types::Result<Vec<LogicalOp>> =
                statements.iter().map(|s| plan(conn, s)).collect();
            Ok(LogicalOp::Union {
                inputs: inputs?,
                all: *all,
            })
        }
    }
}

fn plan_match(conn: &Connection, stmt: &MatchStatement) -> crate::types::Result<LogicalOp> {
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
                "InvalidAggregation: aggregation functions are not allowed in WHERE".to_string(),
            ));
        }
        check_expr_variables(predicate, &bound_vars)?;
        validate_expr_types(predicate, &var_types)?;
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

    // Apply intermediate clauses (WITH/UNWIND/MATCH).
    for clause in &stmt.intermediate_clauses {
        match clause {
            IntermediateClause::With(with) => {
                op = plan_with_scoped(conn, op, with, Some(&scope_vars))?;
                // WITH resets scope to only the projected aliases.
                // Also reset variable type tracking — WITH starts a new scope
                // where variables can be reused with different types.
                scope_vars.clear();
                var_types.clear();
                for item in &with.items {
                    if let Expr::Star = &item.expr {
                        // WITH * keeps all prior variables in scope.
                        scope_vars = bound_vars.clone();
                    } else if let Some(ref alias) = item.alias {
                        scope_vars.insert(alias.clone());
                    } else if let Expr::Variable(var) = &item.expr {
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
fn plan_return(conn: &Connection, stmt: &ReturnStatement) -> crate::types::Result<LogicalOp> {
    let mut op: LogicalOp = LogicalOp::SingleRow;

    check_duplicate_columns(&stmt.return_clause.items)?;

    // Standalone RETURN has no variables in scope — reject any variable refs.
    let empty_scope = HashSet::new();
    validate_return_variables(&stmt.return_clause.items, &empty_scope)?;

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

fn plan_create(conn: &Connection, stmt: &CreateStatement) -> crate::types::Result<LogicalOp> {
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

fn plan_match_create(
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

fn plan_delete(conn: &Connection, stmt: &DeleteStatement) -> crate::types::Result<LogicalOp> {
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

fn plan_set(conn: &Connection, stmt: &SetStatement) -> crate::types::Result<LogicalOp> {
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

fn plan_remove(conn: &Connection, stmt: &RemoveStatement) -> crate::types::Result<LogicalOp> {
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

fn plan_unwind(conn: &Connection, stmt: &UnwindStatement) -> crate::types::Result<LogicalOp> {
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
                            } else if let Expr::Variable(var) = &item.expr {
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
                            } else if let Expr::Variable(var) = &item.expr {
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

fn plan_merge(conn: &Connection, stmt: &MergeStatement) -> crate::types::Result<LogicalOp> {
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
                            "CreatingVarLength: variable-length relationships are not allowed in MERGE".to_string(),
                        ));
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

fn plan_match_merge(
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

/// Plan a multi-clause statement by threading an operator through each clause.
fn plan_multi_clause(
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
                                "VariableAlreadyBound: variable `{path_var}` already bound"
                            )));
                        }
                    }
                    for elem in &pat.elements {
                        match elem {
                            PatternElement::Node(n) => {
                                if let Some(ref var) = n.variable {
                                    if let Some(&kind) = with_value_kinds.get(var) {
                                        if kind == WithValueKind::Scalar {
                                            return Err(GraphError::syntax(format!(
                                                "VariableTypeConflict: variable `{var}` already defined as a scalar value"
                                            )));
                                        }
                                    }
                                }
                            }
                            PatternElement::Relationship(r) => {
                                if let Some(ref var) = r.variable {
                                    if let Some(&kind) = with_value_kinds.get(var) {
                                        if kind == WithValueKind::Scalar {
                                            return Err(GraphError::syntax(format!(
                                                "VariableTypeConflict: variable `{var}` already defined as a scalar value"
                                            )));
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
                                        "VariableAlreadyBound: variable `{var}` already bound"
                                    )));
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
                                        "VariableAlreadyBound: variable `{var}` already bound"
                                    )));
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
                                "CreatingVarLength: variable-length relationships are not allowed in MERGE".to_string(),
                            ));
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
                                GraphError::syntax(
                                    "UndefinedVariable: ORDER BY references a variable not in scope"
                                )
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
                    if let Expr::Star = &item.expr {
                        scope_vars = old_scope.clone();
                        // WITH * passes through existing kinds.
                    } else {
                        let var_name = if let Some(ref alias) = item.alias {
                            scope_vars.insert(alias.clone());
                            alias.clone()
                        } else if let Expr::Variable(var) = &item.expr {
                            scope_vars.insert(var.clone());
                            var.clone()
                        } else if let Expr::Property(var, prop) = &item.expr {
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
fn substitute_aliases(expr: &Expr, aliases: &std::collections::HashMap<String, Expr>) -> Expr {
    match expr {
        Expr::Variable(name) => {
            if let Some(original) = aliases.get(name) {
                original.clone()
            } else {
                expr.clone()
            }
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_aliases(left, aliases)),
            op: *op,
            right: Box::new(substitute_aliases(right, aliases)),
        },
        Expr::Not(inner) => Expr::Not(Box::new(substitute_aliases(inner, aliases))),
        Expr::IsNull(inner) => Expr::IsNull(Box::new(substitute_aliases(inner, aliases))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(substitute_aliases(inner, aliases))),
        Expr::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| substitute_aliases(a, aliases))
                .collect(),
            distinct: *distinct,
            original_text: original_text.clone(),
        },
        _ => expr.clone(),
    }
}

fn plan_with(
    conn: &Connection,
    input: LogicalOp,
    with: &WithClause,
) -> crate::types::Result<LogicalOp> {
    plan_with_scoped(conn, input, with, None)
}

fn plan_with_scoped(
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
                item.expr,
                Expr::Variable(_) | Expr::Star | Expr::Property(_, _)
            )
        {
            return Err(GraphError::syntax(
                "NoExpressionAlias: expression in WITH must be aliased (use AS)".to_string(),
            ));
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
                            "UndefinedVariable: ORDER BY contains an aggregation that is not projected in WITH".to_string(),
                        ));
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
fn plan_intermediate_match_with_scope(
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
                return Err(GraphError::syntax(format!(
                    "VariableAlreadyBound: variable `{path_var}` already defined"
                )));
            }
        }
    }

    if !im.patterns.is_empty() {
        let right = plan_patterns(conn, &im.patterns)?;
        op = LogicalOp::CorrelatedJoin {
            input: Box::new(op),
            right: Box::new(right),
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
                    }
                } else {
                    LogicalOp::CrossProduct {
                        left: Box::new(left),
                        right: Box::new(right),
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
        _ => {
            return Err(GraphError::Serialization(
                "shortestPath pattern must start with a node".to_string(),
            ))
        }
    };
    let rel = match &pattern.elements[1] {
        PatternElement::Relationship(r) => r,
        _ => {
            return Err(GraphError::Serialization(
                "shortestPath pattern must have a relationship".to_string(),
            ))
        }
    };
    let dst_node = match &pattern.elements[2] {
        PatternElement::Node(n) => n,
        _ => {
            return Err(GraphError::Serialization(
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
fn validate_return_variables(
    items: &[ReturnItem],
    scope_vars: &HashSet<String>,
) -> crate::types::Result<()> {
    for item in items {
        if matches!(item.expr, Expr::Star) {
            // RETURN * with no variables in scope.
            if scope_vars.is_empty() {
                return Err(GraphError::syntax(
                    "RETURN * is not allowed when there are no variables in scope".to_string(),
                ));
            }
            continue;
        }
        check_expr_variables(&item.expr, scope_vars)?;
    }
    Ok(())
}

/// Check for duplicate column names in RETURN/WITH items.
fn check_duplicate_columns(items: &[ReturnItem]) -> crate::types::Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    for item in items {
        if matches!(item.expr, Expr::Star) {
            continue;
        }
        let col = item
            .alias
            .clone()
            .unwrap_or_else(|| crate::cypher::eval::expr_to_column_name(&item.expr));
        if !seen.insert(col.clone()) {
            return Err(GraphError::syntax(format!(
                "Multiple result columns with the same name are not supported: '{col}'"
            )));
        }
    }
    Ok(())
}

/// Validate function argument types at compile time using known variable kinds.
fn validate_expr_types(
    expr: &Expr,
    var_types: &HashMap<String, VarKind>,
) -> crate::types::Result<()> {
    match expr {
        Expr::FunctionCall { name, args, .. } => {
            let name_lower = name.to_ascii_lowercase();
            if let Some(Expr::Variable(var)) = args.first() {
                if let Some(kind) = var_types.get(var) {
                    match name_lower.as_str() {
                        "type" if *kind == VarKind::Node => {
                            return Err(GraphError::type_error(
                                crate::types::QueryPhase::SemanticAnalysis,
                                "InvalidArgumentType: type() requires a relationship".to_string(),
                            ));
                        }
                        "length" if *kind == VarKind::Node || *kind == VarKind::Relationship => {
                            return Err(GraphError::type_error(
                                crate::types::QueryPhase::SemanticAnalysis,
                                "InvalidArgumentType: length() requires a path, string, or list"
                                    .to_string(),
                            ));
                        }
                        "toboolean" | "tointeger" | "tofloat" | "tostring"
                            if *kind == VarKind::Node || *kind == VarKind::Relationship =>
                        {
                            return Err(GraphError::type_error(
                                crate::types::QueryPhase::SemanticAnalysis,
                                format!("InvalidArgumentValue: {name}() cannot convert a {kind:?}"),
                            ));
                        }
                        _ => {}
                    }
                }
            }
            for arg in args {
                validate_expr_types(arg, var_types)?;
            }
        }
        Expr::Property(var, _) => {
            if let Some(kind) = var_types.get(var) {
                if *kind == VarKind::Path {
                    return Err(GraphError::type_error(
                        crate::types::QueryPhase::SemanticAnalysis,
                        "InvalidArgumentType: property access on a path is not allowed".to_string(),
                    ));
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_expr_types(left, var_types)?;
            validate_expr_types(right, var_types)?;
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            validate_expr_types(inner, var_types)?;
        }
        _ => {}
    }
    Ok(())
}

/// Check that every variable reference in an expression is present in `scope`.
fn check_expr_variables(expr: &Expr, scope: &HashSet<String>) -> crate::types::Result<()> {
    match expr {
        Expr::Variable(var) => {
            if !scope.contains(var) {
                return Err(GraphError::syntax(format!("UndefinedVariable: {var}")));
            }
        }
        Expr::Property(var, _) => {
            if !scope.contains(var) {
                return Err(GraphError::syntax(format!("UndefinedVariable: {var}")));
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            check_expr_variables(left, scope)?;
            check_expr_variables(right, scope)?;
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            check_expr_variables(inner, scope)?;
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                check_expr_variables(arg, scope)?;
            }
        }
        Expr::Case {
            operand,
            alternatives,
            default,
        } => {
            if let Some(o) = operand {
                check_expr_variables(o, scope)?;
            }
            for (cond, result) in alternatives {
                check_expr_variables(cond, scope)?;
                check_expr_variables(result, scope)?;
            }
            if let Some(d) = default {
                check_expr_variables(d, scope)?;
            }
        }
        Expr::List(items) => {
            for item in items {
                check_expr_variables(item, scope)?;
            }
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs {
                check_expr_variables(v, scope)?;
            }
        }
        Expr::Index { expr, index } => {
            check_expr_variables(expr, scope)?;
            check_expr_variables(index, scope)?;
        }
        Expr::Slice { expr, start, end } => {
            check_expr_variables(expr, scope)?;
            if let Some(s) = start {
                check_expr_variables(s, scope)?;
            }
            if let Some(e) = end {
                check_expr_variables(e, scope)?;
            }
        }
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Star => {}
        Expr::ListComprehension { list_expr, .. } => {
            check_expr_variables(list_expr, scope)?;
        }
        Expr::Quantifier { list_expr, .. } => {
            check_expr_variables(list_expr, scope)?;
        }
        Expr::Exists { .. }
        | Expr::ExistsSubquery(_)
        | Expr::PatternPredicate(_)
        | Expr::PatternComprehension { .. } => {}
        Expr::DotAccess { expr, .. } => {
            check_expr_variables(expr, scope)?;
        }
        Expr::HasLabel(var, _) => {
            if !scope.contains(var) {
                return Err(GraphError::syntax(format!("UndefinedVariable: {var}")));
            }
        }
    }
    Ok(())
}

/// Collect all variable names (nodes, relationships, and path variables) from a set of patterns.
fn collect_pattern_variables(patterns: &[Pattern]) -> HashSet<String> {
    let mut vars = HashSet::new();
    for pattern in patterns {
        if let Some(ref path_var) = pattern.path_variable {
            vars.insert(path_var.clone());
        }
        for elem in &pattern.elements {
            match elem {
                PatternElement::Node(n) => {
                    if let Some(ref var) = n.variable {
                        vars.insert(var.clone());
                    }
                }
                PatternElement::Relationship(r) => {
                    if let Some(ref var) = r.variable {
                        vars.insert(var.clone());
                    }
                }
            }
        }
    }
    vars
}

/// Validate that CREATE patterns don't reference undefined variables in property
/// expressions and that relationships have exactly one type and a direction.
fn validate_create_patterns(
    patterns: &[Pattern],
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    // Use a shared scope so variables from earlier patterns are visible in later ones.
    let mut local_scope: HashSet<String> = scope.clone();
    for pattern in patterns {
        for elem in &pattern.elements {
            match elem {
                PatternElement::Node(n) => {
                    // Check property expressions for undefined variables.
                    for expr in n.properties.values() {
                        check_expr_variables(expr, &local_scope)?;
                    }
                    if let Some(ref var) = n.variable {
                        local_scope.insert(var.clone());
                    }
                }
                PatternElement::Relationship(rel) => {
                    // CREATE relationships must have exactly one type.
                    if rel.rel_types.is_empty() {
                        return Err(GraphError::syntax(
                            "NoSingleRelationshipType: a relationship must have exactly one type in CREATE"
                        ));
                    }
                    if rel.rel_types.len() > 1 {
                        return Err(GraphError::syntax(
                            "NoSingleRelationshipType: a relationship must have exactly one type in CREATE"
                        ));
                    }
                    // CREATE relationships must be directed.
                    if rel.direction == RelDirection::Undirected {
                        return Err(GraphError::syntax(
                            "RequiresDirectedRelationship: only directed relationships are supported in CREATE"
                        ));
                    }
                    // Check property expressions for undefined variables.
                    for expr in rel.properties.values() {
                        check_expr_variables(expr, &local_scope)?;
                    }
                    if let Some(ref var) = rel.variable {
                        local_scope.insert(var.clone());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Validate that MERGE patterns have valid relationship types (exactly one) and
/// check for undefined variables in ON CREATE/ON MATCH SET items.
fn validate_merge_pattern(
    pattern: &Pattern,
    scope: &HashSet<String>,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> crate::types::Result<()> {
    // Collect merge-pattern variables.
    let mut merge_vars = scope.clone();
    for elem in &pattern.elements {
        match elem {
            PatternElement::Node(n) => {
                if let Some(ref var) = n.variable {
                    merge_vars.insert(var.clone());
                }
            }
            PatternElement::Relationship(rel) => {
                // MERGE relationships must have exactly one type.
                if rel.rel_types.is_empty() {
                    return Err(GraphError::syntax(
                        "NoSingleRelationshipType: a relationship must have exactly one type in MERGE"
                    ));
                }
                if rel.rel_types.len() > 1 {
                    return Err(GraphError::syntax(
                        "NoSingleRelationshipType: a relationship must have exactly one type in MERGE"
                    ));
                }
                if let Some(ref var) = rel.variable {
                    merge_vars.insert(var.clone());
                }
            }
        }
    }
    // Validate ON CREATE SET / ON MATCH SET variable references.
    for item in on_create.iter().chain(on_match.iter()) {
        match item {
            SetItem::Property(a) => {
                if !merge_vars.contains(&a.variable) {
                    return Err(GraphError::syntax(format!(
                        "UndefinedVariable: {}",
                        a.variable
                    )));
                }
                check_expr_variables(&a.value, &merge_vars)?;
            }
            SetItem::Label { variable, .. } => {
                if !merge_vars.contains(variable) {
                    return Err(GraphError::syntax(format!("UndefinedVariable: {variable}")));
                }
            }
            SetItem::MapOverwrite { variable, value } | SetItem::MapMerge { variable, value } => {
                if !merge_vars.contains(variable) {
                    return Err(GraphError::syntax(format!("UndefinedVariable: {variable}")));
                }
                check_expr_variables(value, &merge_vars)?;
            }
        }
    }
    Ok(())
}

/// Validate that SET items don't reference undefined variables.
fn validate_set_variables(items: &[SetItem], scope: &HashSet<String>) -> crate::types::Result<()> {
    for item in items {
        match item {
            SetItem::Property(a) => {
                if !scope.contains(&a.variable) {
                    return Err(GraphError::syntax(format!(
                        "UndefinedVariable: {}",
                        a.variable
                    )));
                }
                check_expr_variables(&a.value, scope)?;
            }
            SetItem::Label { variable, .. } => {
                if !scope.contains(variable) {
                    return Err(GraphError::syntax(format!("UndefinedVariable: {variable}")));
                }
            }
            SetItem::MapOverwrite { variable, value } | SetItem::MapMerge { variable, value } => {
                if !scope.contains(variable) {
                    return Err(GraphError::syntax(format!("UndefinedVariable: {variable}")));
                }
                check_expr_variables(value, scope)?;
            }
        }
    }
    Ok(())
}

/// Validate that DELETE variable references are in scope.
fn validate_delete_exprs(
    exprs: &[crate::cypher::ast::Expr],
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    use crate::cypher::ast::Expr;
    for expr in exprs {
        match expr {
            Expr::Variable(var) => {
                if !scope.contains(var) {
                    return Err(GraphError::syntax(format!("UndefinedVariable: {var}")));
                }
            }
            // HasLabel expression (e.g. `n:Person`) is not a valid DELETE target.
            Expr::HasLabel(..) => {
                return Err(GraphError::syntax(
                    "InvalidDelete: cannot delete a label predicate expression",
                ));
            }
            // Literal expressions are not valid DELETE targets.
            Expr::Literal(_) => {
                return Err(GraphError::syntax(
                    "InvalidArgumentType: DELETE requires a node, relationship, or path",
                ));
            }
            // Binary/arithmetic expressions are not valid DELETE targets.
            Expr::BinaryOp { .. } => {
                return Err(GraphError::syntax(
                    "InvalidArgumentType: DELETE requires a node, relationship, or path",
                ));
            }
            // Property access (e.g. nodes.key), index (e.g. friends[0]) — valid.
            // DotAccess (e.g. rels.key.key[0]) — valid.
            _ => {
                if let Some(root) = extract_root_variable(expr) {
                    if !scope.contains(&root) {
                        return Err(GraphError::syntax(format!("UndefinedVariable: {root}")));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Extract the root variable name from an expression tree.
fn extract_root_variable(expr: &crate::cypher::ast::Expr) -> Option<String> {
    use crate::cypher::ast::Expr;
    match expr {
        Expr::Variable(v) => Some(v.clone()),
        Expr::Property(v, _) => Some(v.clone()),
        Expr::Index { expr: base, .. } => extract_root_variable(base),
        _ => None,
    }
}

/// Check if ORDER BY contains aggregate functions when the RETURN/WITH itself
/// does not use aggregation. This is invalid per openCypher spec.
fn validate_no_aggregation_in_order_by(order_by: &[SortItem]) -> crate::types::Result<()> {
    for item in order_by {
        if is_aggregate_fn(&item.expr) {
            return Err(GraphError::syntax(
                "InvalidAggregation: aggregation functions are not allowed in ORDER BY when there is no aggregation in the preceding WITH/RETURN"
            ));
        }
    }
    Ok(())
}

/// Check if ORDER BY after RETURN with aggregates references non-returned
/// non-aggregate expressions (variables consumed by aggregation).
fn validate_return_order_by_with_aggregates(
    return_items: &[ReturnItem],
    order_by: &[SortItem],
) -> crate::types::Result<()> {
    // Build set of returned column names/exprs.
    let mut returned_cols: HashSet<String> = HashSet::new();
    for item in return_items {
        returned_cols.insert(crate::cypher::eval::expr_to_column_name(&item.expr));
        if let Some(ref alias) = item.alias {
            returned_cols.insert(alias.clone());
        }
    }

    for sort_item in order_by {
        // If the ORDER BY expression itself is an aggregate, that's InvalidAggregation
        // when the aggregate isn't in the RETURN projection.
        if is_aggregate_fn(&sort_item.expr) {
            // Check if any non-aggregate subexpressions reference non-returned variables.
            let mut non_agg_leaves = Vec::new();
            collect_non_aggregate_leaves(&sort_item.expr, &mut non_agg_leaves);
            for leaf in &non_agg_leaves {
                let col = crate::cypher::eval::expr_to_column_name(leaf);
                if !returned_cols.contains(&col) {
                    return Err(GraphError::syntax(
                        "UndefinedVariable: ORDER BY references a variable not in RETURN",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Validate that RETURN DISTINCT + ORDER BY only sorts by columns in the DISTINCT projection.
fn validate_distinct_order_by(
    return_items: &[ReturnItem],
    order_by: &[SortItem],
    distinct: bool,
) -> crate::types::Result<()> {
    if !distinct || order_by.is_empty() {
        return Ok(());
    }
    // Build set of projected columns and returned variable names.
    let mut projected: HashSet<String> = HashSet::new();
    let mut returned_vars: HashSet<String> = HashSet::new();
    for item in return_items {
        if let Some(ref alias) = item.alias {
            projected.insert(alias.clone());
        }
        projected.insert(crate::cypher::eval::expr_to_column_name(&item.expr));
        // Track bare variable names so ORDER BY can access their properties.
        if let Expr::Variable(var) = &item.expr {
            returned_vars.insert(var.clone());
        }
    }
    for sort_item in order_by {
        let col = crate::cypher::eval::expr_to_column_name(&sort_item.expr);
        if projected.contains(&col) {
            continue;
        }
        // Allow property access on returned variables (e.g. ORDER BY a.name when RETURN DISTINCT a).
        if let Expr::Property(var, _) = &sort_item.expr {
            if returned_vars.contains(var) {
                continue;
            }
        }
        return Err(GraphError::syntax(
            "UndefinedVariable: ORDER BY references a variable not in RETURN DISTINCT",
        ));
    }
    Ok(())
}

/// Validate that ORDER BY in WITH only references variables available in the
/// input scope (from the prior clause). The input scope is what was projected
/// by the previous WITH/MATCH, so variables dropped earlier are caught.
fn validate_with_order_by_scope_input(
    input_scope: &HashSet<String>,
    with_items: &[ReturnItem],
    order_by: &[SortItem],
) -> crate::types::Result<()> {
    // The effective scope for ORDER BY is the input scope plus any aliases
    // defined by the WITH items (for aggregate aliases like `count(*) AS c`).
    let mut scope = input_scope.clone();
    for item in with_items {
        if let Some(ref alias) = item.alias {
            scope.insert(alias.clone());
        }
        if matches!(item.expr, Expr::Star) {
            return Ok(()); // WITH * — skip validation.
        }
    }
    for sort_item in order_by {
        validate_expr_in_scope(&sort_item.expr, &scope)?;
    }
    Ok(())
}

/// For aggregate WITH ORDER BY, validate that non-aggregate leaf variable
/// references are in the group-key/alias scope (not the full input scope).
fn validate_agg_order_by_scope(
    with_items: &[ReturnItem],
    order_by: &[SortItem],
) -> crate::types::Result<()> {
    let mut scope = HashSet::new();
    for item in with_items {
        if let Some(ref alias) = item.alias {
            scope.insert(alias.clone());
        }
        // Non-aggregate items are group keys — their variables are in scope.
        if !is_aggregate_fn(&item.expr) {
            collect_variables_from_expr(&item.expr, &mut scope);
        }
    }
    for sort_item in order_by {
        validate_non_agg_leaves_in_scope(&sort_item.expr, &scope)?;
    }
    Ok(())
}

/// Collect all variable names referenced in an expression.
fn collect_variables_from_expr(expr: &Expr, vars: &mut HashSet<String>) {
    match expr {
        Expr::Variable(name) => {
            vars.insert(name.clone());
        }
        Expr::Property(var, _) => {
            vars.insert(var.clone());
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_variables_from_expr(left, vars);
            collect_variables_from_expr(right, vars);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_variables_from_expr(arg, vars);
            }
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            collect_variables_from_expr(inner, vars);
        }
        _ => {}
    }
}

/// Walk an expression, skipping aggregate function subtrees, and check that
/// remaining variable references are in the given scope.
fn validate_non_agg_leaves_in_scope(
    expr: &Expr,
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    match expr {
        Expr::FunctionCall { name, args, .. } => {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "count"
                    | "sum"
                    | "avg"
                    | "min"
                    | "max"
                    | "collect"
                    | "percentiledisc"
                    | "percentilecont"
                    | "stdev"
                    | "stdevp"
            ) {
                // Aggregate subtree — variables inside are fine.
                return Ok(());
            }
            for arg in args {
                validate_non_agg_leaves_in_scope(arg, scope)?;
            }
        }
        Expr::Variable(name) => {
            if !scope.contains(name) {
                return Err(GraphError::syntax(format!(
                    "UndefinedVariable: variable `{name}` not defined"
                )));
            }
        }
        Expr::Property(var, _) => {
            if !scope.contains(var) {
                return Err(GraphError::syntax(format!(
                    "UndefinedVariable: variable `{var}` not defined"
                )));
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_non_agg_leaves_in_scope(left, scope)?;
            validate_non_agg_leaves_in_scope(right, scope)?;
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            validate_non_agg_leaves_in_scope(inner, scope)?;
        }
        _ => {} // Literals, parameters, Star — always valid.
    }
    Ok(())
}

/// Check that all variable references in an expression are in the given scope.
fn validate_expr_in_scope(expr: &Expr, scope: &HashSet<String>) -> crate::types::Result<()> {
    match expr {
        Expr::Variable(name) => {
            if !scope.contains(name) {
                return Err(GraphError::syntax(format!(
                    "UndefinedVariable: variable `{name}` not defined"
                )));
            }
        }
        Expr::Property(var, _) => {
            if !scope.contains(var) {
                return Err(GraphError::syntax(format!(
                    "UndefinedVariable: variable `{var}` not defined"
                )));
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_expr_in_scope(left, scope)?;
            validate_expr_in_scope(right, scope)?;
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                validate_expr_in_scope(arg, scope)?;
            }
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            validate_expr_in_scope(inner, scope)?;
        }
        Expr::List(items) => {
            for item in items {
                validate_expr_in_scope(item, scope)?;
            }
        }
        _ => {} // Literals, Star, etc. — always valid.
    }
    Ok(())
}

/// Check for aggregation functions in a list comprehension mapping expression.
fn validate_no_aggregation_in_list_comp(expr: &Expr) -> crate::types::Result<()> {
    match expr {
        Expr::ListComprehension {
            map_expr,
            filter,
            list_expr,
            ..
        } => {
            if let Some(ref me) = map_expr {
                if is_aggregate_fn(me) {
                    return Err(GraphError::syntax(
                        "InvalidAggregation: aggregation functions are not allowed in list comprehension"
                    ));
                }
                validate_no_aggregation_in_list_comp(me)?;
            }
            if let Some(ref fe) = filter {
                validate_no_aggregation_in_list_comp(fe)?;
            }
            validate_no_aggregation_in_list_comp(list_expr)?;
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                validate_no_aggregation_in_list_comp(arg)?;
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_no_aggregation_in_list_comp(left)?;
            validate_no_aggregation_in_list_comp(right)?;
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            validate_no_aggregation_in_list_comp(inner)?;
        }
        Expr::List(items) => {
            for item in items {
                validate_no_aggregation_in_list_comp(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Track the "kind" of a variable bound by WITH (scalar value vs node/rel/path).
/// Used to detect VariableTypeConflict when a scalar-bound variable is later
/// used as a node or relationship in MATCH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum WithValueKind {
    Scalar, // literal, property access, list, etc.
    Node,
    Relationship,
    Path,
}

/// Infer the kind of value a WITH/RETURN expression produces.
fn infer_with_value_kind(expr: &Expr) -> WithValueKind {
    match expr {
        Expr::Literal(_) => WithValueKind::Scalar,
        Expr::List(_) => WithValueKind::Scalar,
        Expr::MapLiteral(_) => WithValueKind::Scalar,
        Expr::Property(_, _) => WithValueKind::Scalar,
        Expr::BinaryOp { .. } => WithValueKind::Scalar,
        Expr::FunctionCall { .. } => WithValueKind::Scalar,
        Expr::Index { .. } => WithValueKind::Scalar,
        Expr::Slice { .. } => WithValueKind::Scalar,
        _ => WithValueKind::Node, // Variables pass through — could be node/rel/path
    }
}

/// Validate that MERGE node variables that are already bound aren't being
/// re-created with new labels or predicates.
fn validate_merge_variable_rebinding(
    pattern: &Pattern,
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    // Check for single-node MERGE with already-bound variable.
    if pattern.elements.len() == 1 {
        if let PatternElement::Node(n) = &pattern.elements[0] {
            if let Some(ref var) = n.variable {
                if scope.contains(var) {
                    return Err(GraphError::syntax(format!(
                        "VariableAlreadyBound: variable `{var}` already bound"
                    )));
                }
            }
        }
    }
    // For relationship MERGE patterns, check relationship variable rebinding
    // and check node rebinding with new labels.
    if pattern.elements.len() == 3 {
        for elem in &pattern.elements {
            if let PatternElement::Relationship(rel) = elem {
                if let Some(ref var) = rel.variable {
                    if scope.contains(var) {
                        return Err(GraphError::syntax(format!(
                            "VariableAlreadyBound: variable `{var}` already bound"
                        )));
                    }
                }
            }
            if let PatternElement::Node(n) = elem {
                if let Some(ref var) = n.variable {
                    if scope.contains(var) && !n.labels.is_empty() {
                        return Err(GraphError::syntax(format!(
                            "VariableAlreadyBound: variable `{var}` already bound"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    Relationship,
    Path,
}

/// Validate that no variable is used as more than one type (node, relationship,
/// path) within the same MATCH statement's patterns. Raises `SyntaxError` on
/// conflicts — e.g. `MATCH ()-[r]-(r)` uses `r` as both relationship and node.
fn validate_variable_types(patterns: &[Pattern]) -> crate::types::Result<HashMap<String, VarKind>> {
    let mut types: HashMap<String, VarKind> = HashMap::new();
    validate_variable_types_with_map(patterns, &mut types)?;
    Ok(types)
}

/// Validate variable types against a persistent type map across clauses.
fn validate_variable_types_with_map(
    patterns: &[Pattern],
    types: &mut std::collections::HashMap<String, VarKind>,
) -> crate::types::Result<()> {
    for pattern in patterns {
        // Path variable binding: `r = (...)-[...]->(...)`
        if let Some(ref path_var) = pattern.path_variable {
            if let Some(&existing) = types.get(path_var) {
                if existing != VarKind::Path {
                    return Err(GraphError::Query(crate::types::QueryError::SyntaxError {
                        phase: crate::types::QueryPhase::SemanticAnalysis,
                        message: format!(
                            "variable '{path_var}' already defined with a different type"
                        ),
                    }));
                }
            } else {
                types.insert(path_var.clone(), VarKind::Path);
            }
        }

        for elem in &pattern.elements {
            match elem {
                PatternElement::Node(n) => {
                    if let Some(ref var) = n.variable {
                        if let Some(&existing) = types.get(var) {
                            if existing != VarKind::Node {
                                return Err(GraphError::Query(
                                    crate::types::QueryError::SyntaxError {
                                        phase: crate::types::QueryPhase::SemanticAnalysis,
                                        message: format!(
                                            "variable '{var}' already defined with a different type"
                                        ),
                                    },
                                ));
                            }
                        } else {
                            types.insert(var.clone(), VarKind::Node);
                        }
                    }
                }
                PatternElement::Relationship(r) => {
                    if let Some(ref var) = r.variable {
                        if let Some(&existing) = types.get(var) {
                            if existing != VarKind::Relationship {
                                return Err(GraphError::Query(
                                    crate::types::QueryError::SyntaxError {
                                        phase: crate::types::QueryPhase::SemanticAnalysis,
                                        message: format!(
                                            "variable '{var}' already defined with a different type"
                                        ),
                                    },
                                ));
                            }
                            // Relationship variables cannot be reused in the
                            // same MATCH clause (unlike node variables).
                            return Err(GraphError::syntax(format!(
                                "VariableAlreadyBound: cannot use relationship variable '{var}' more than once in a pattern"
                            )));
                        } else {
                            types.insert(var.clone(), VarKind::Relationship);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Plan OPTIONAL MATCH patterns. Returns (plan, new_aliases) where the plan is
/// an expansion chain and new_aliases lists variables introduced by the optional
/// patterns (not shared with the required MATCH).
///
/// `bound_vars` contains variables already established by prior MATCH / OPTIONAL
/// MATCH clauses. Any node variable NOT in `bound_vars` is new and will be
/// NULL-filled on no match.
fn plan_optional_match(
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
fn plan_single_pattern(conn: &Connection, pattern: &Pattern) -> crate::types::Result<LogicalOp> {
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
                        return Err(GraphError::Serialization(
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
                });

                // Apply destination node's label filters.
                for dst_label in &dst_node.labels {
                    if !dst_label.is_empty() {
                        let predicate = Expr::BinaryOp {
                            left: Box::new(Expr::Literal(LiteralValue::String(dst_label.clone()))),
                            op: BinOp::In,
                            right: Box::new(Expr::Property(
                                dst_alias.clone(),
                                "__labels".to_string(),
                            )),
                        };
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

    let mut result = op.ok_or_else(|| GraphError::Serialization("empty pattern".to_string()))?;

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
fn plan_node_scan(
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
                let count_a =
                    index::index_count_for_value(conn, &label, key_a, &val_a).unwrap_or(usize::MAX);
                let count_b =
                    index::index_count_for_value(conn, &label, key_b, &val_b).unwrap_or(usize::MAX);
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

    // Add filters for additional labels (multi-label nodes).
    for extra_label in node.labels.iter().skip(1) {
        let predicate = Expr::BinaryOp {
            left: Box::new(Expr::Literal(LiteralValue::String(extra_label.clone()))),
            op: BinOp::In,
            right: Box::new(Expr::Property(alias.to_string(), "__labels".to_string())),
        };
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
fn plan_create_pattern(
    pattern: &Pattern,
    seen: &mut HashSet<String>,
) -> crate::types::Result<Vec<LogicalOp>> {
    let mut anon_counter = 0usize;
    plan_create_pattern_with_counter(pattern, seen, &mut anon_counter)
}

fn plan_create_pattern_with_counter(
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
                        "VariableAlreadyBound: variable `{}` already bound",
                        alias.as_deref().unwrap_or("?")
                    )));
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
                        "CreatingVarLength: variable-length relationships are not allowed in CREATE".to_string(),
                    ));
                }

                let dst_node = match pattern.elements.get(i + 1) {
                    Some(PatternElement::Node(n)) => n,
                    _ => {
                        return Err(GraphError::Serialization(
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
                        "VariableAlreadyBound: variable `{}` already bound",
                        dst_alias.as_deref().unwrap_or("?")
                    )));
                }
                if !dst_already_seen {
                    ops.push(LogicalOp::CreateNode {
                        labels: dst_node.labels.clone(),
                        alias: dst_alias.clone(),
                        properties: dst_node.properties.clone(),
                    });
                }

                let left = last_alias.clone().ok_or_else(|| {
                    GraphError::Serialization("edge without source node".to_string())
                })?;
                let right = dst_alias.clone().ok_or_else(|| {
                    GraphError::Serialization("edge target must have a variable".to_string())
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
    match expr {
        Expr::FunctionCall { name, args, .. } => {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "count"
                    | "sum"
                    | "avg"
                    | "min"
                    | "max"
                    | "collect"
                    | "percentiledisc"
                    | "percentilecont"
                    | "stdev"
                    | "stdevp"
            ) {
                true
            } else {
                // Check if any argument contains an aggregate (e.g. size(collect(a))).
                args.iter().any(is_aggregate_fn)
            }
        }
        // Recursively check sub-expressions (e.g. `count(a) > 0`).
        Expr::BinaryOp { left, right, .. } => is_aggregate_fn(left) || is_aggregate_fn(right),
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => is_aggregate_fn(inner),
        Expr::MapLiteral(pairs) => pairs.iter().any(|(_, v)| is_aggregate_fn(v)),
        Expr::List(items) => items.iter().any(is_aggregate_fn),
        _ => false,
    }
}

/// Collect all aggregate function call sub-expressions from an expression tree.
/// Stops recursing into aggregate function arguments (aggregates don't nest).
fn collect_aggregate_calls<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::FunctionCall { name, .. }
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "count"
                    | "sum"
                    | "avg"
                    | "min"
                    | "max"
                    | "collect"
                    | "percentiledisc"
                    | "percentilecont"
                    | "stdev"
                    | "stdevp"
            ) =>
        {
            out.push(expr);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_aggregate_calls(arg, out);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_aggregate_calls(left, out);
            collect_aggregate_calls(right, out);
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            collect_aggregate_calls(inner, out);
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs {
                collect_aggregate_calls(v, out);
            }
        }
        Expr::List(items) => {
            for item in items {
                collect_aggregate_calls(item, out);
            }
        }
        _ => {}
    }
}

/// Check if an expression is a "pure" aggregate — a direct aggregate function call,
/// not a mix like `x + count(y)`.
fn is_pure_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, .. } => parse_agg_name(name).is_some(),
        _ => false,
    }
}

/// Split RETURN/WITH items into group keys (non-aggregate) and aggregate expressions.
fn split_aggregates(items: &[ReturnItem]) -> crate::types::Result<(Vec<Expr>, Vec<AggregateExpr>)> {
    let mut group_keys = Vec::new();
    let mut aggregates = Vec::new();
    let mut mixed_items: Vec<&Expr> = Vec::new();

    for item in items {
        if let Expr::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } = &item.expr
        {
            if let Some(function) = parse_agg_name(name) {
                // Reject aggregate-in-aggregate: count(count(*))
                for arg in args {
                    if is_aggregate_fn(arg) {
                        return Err(GraphError::syntax(
                            "Can not use an aggregation in an aggregation".to_string(),
                        ));
                    }
                }
                let input = args.first().cloned().unwrap_or(Expr::Star);
                let extra_arg = args.get(1).cloned();
                aggregates.push(AggregateExpr {
                    function,
                    input,
                    alias: item.alias.clone(),
                    distinct: *distinct,
                    extra_arg,
                    original_name: name.clone(),
                    original_call_text: original_text.clone(),
                });
                continue;
            }
        }
        // For non-aggregate expressions, extract any nested aggregates.
        extract_nested_aggregates(&item.expr, &mut aggregates);
        if !is_aggregate_fn(&item.expr) {
            group_keys.push(item.expr.clone());
        } else if !is_pure_aggregate(&item.expr) {
            // Mixed aggregate + non-aggregate expression (e.g. `me.age + count(you.age)`).
            // Collect non-aggregate leaf expressions.
            mixed_items.push(&item.expr);
        }
    }

    // Validate mixed items: non-aggregate sub-expressions must be group keys.
    for mixed_expr in &mixed_items {
        let mut non_agg_leaves = Vec::new();
        collect_non_aggregate_leaves(mixed_expr, &mut non_agg_leaves);
        for leaf in &non_agg_leaves {
            if !group_keys.iter().any(|gk| gk == *leaf) {
                return Err(GraphError::syntax(
                    "AmbiguousAggregationExpression: expression mixes aggregate and non-aggregate sub-expressions".to_string(),
                ));
            }
        }
    }

    Ok((group_keys, aggregates))
}

/// Collect non-aggregate, non-constant leaf expressions from a mixed expression.
fn collect_non_aggregate_leaves<'a>(expr: &'a Expr, leaves: &mut Vec<&'a Expr>) {
    match expr {
        Expr::FunctionCall { name, .. } if parse_agg_name(name).is_some() => {
            // Aggregate function — skip entirely (its args are aggregated)
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_non_aggregate_leaves(left, leaves);
            collect_non_aggregate_leaves(right, leaves);
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            collect_non_aggregate_leaves(inner, leaves);
        }
        // Constants don't need grouping.
        Expr::Literal(_) | Expr::Star => {}
        // Non-aggregate functions are fine if their args are constants/grouped.
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_non_aggregate_leaves(arg, leaves);
            }
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs {
                collect_non_aggregate_leaves(v, leaves);
            }
        }
        Expr::List(items) => {
            for item in items {
                collect_non_aggregate_leaves(item, leaves);
            }
        }
        _ => {
            // Variable reference, property access, etc. — needs grouping.
            leaves.push(expr);
        }
    }
}

/// Parse aggregate function name to enum.
fn parse_agg_name(name: &str) -> Option<AggregateFunction> {
    match name.to_ascii_lowercase().as_str() {
        "count" => Some(AggregateFunction::Count),
        "sum" => Some(AggregateFunction::Sum),
        "avg" => Some(AggregateFunction::Avg),
        "min" => Some(AggregateFunction::Min),
        "max" => Some(AggregateFunction::Max),
        "collect" => Some(AggregateFunction::Collect),
        "percentiledisc" => Some(AggregateFunction::PercentileDisc),
        "percentilecont" => Some(AggregateFunction::PercentileCont),
        "stdev" => Some(AggregateFunction::StDev),
        "stdevp" => Some(AggregateFunction::StDevP),
        _ => None,
    }
}

/// Walk an expression tree and extract aggregate function calls into the list.
fn extract_nested_aggregates(expr: &Expr, aggregates: &mut Vec<AggregateExpr>) {
    match expr {
        Expr::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => {
            if let Some(function) = parse_agg_name(name) {
                let input = args.first().cloned().unwrap_or(Expr::Star);
                let extra_arg = args.get(1).cloned();
                aggregates.push(AggregateExpr {
                    function,
                    input,
                    alias: None,
                    distinct: *distinct,
                    extra_arg,
                    original_name: name.clone(),
                    original_call_text: original_text.clone(),
                });
                return; // Don't recurse into aggregate arguments.
            }
            for arg in args {
                extract_nested_aggregates(arg, aggregates);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            extract_nested_aggregates(left, aggregates);
            extract_nested_aggregates(right, aggregates);
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            extract_nested_aggregates(inner, aggregates);
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs {
                extract_nested_aggregates(v, aggregates);
            }
        }
        Expr::List(items) => {
            for item in items {
                extract_nested_aggregates(item, aggregates);
            }
        }
        _ => {}
    }
}

// ── Predicate pushdown helpers ──────────────────────────────────────────

/// Flatten a predicate into AND-connected conjuncts.
fn decompose_conjuncts(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinOp::And,
            right,
        } => {
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
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Property(a, p), Expr::Literal(l)) => (a.clone(), p.clone(), l.clone()),
            (Expr::Literal(l), Expr::Property(a, p)) => (a.clone(), p.clone(), l.clone()),
            _ => return None,
        },
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
        LogicalOp::Scan {
            label,
            alias: scan_alias,
        } if scan_alias == alias => {
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
        LogicalOp::Filter {
            input,
            predicate: existing,
        } => try_replace_scan(conn, input, alias, prop, lit).map(|new_input| LogicalOp::Filter {
            input: Box::new(new_input),
            predicate: existing.clone(),
        }),

        LogicalOp::Expand {
            input,
            src_alias,
            dst_alias,
            rel_alias,
            edge_types,
            direction,
            min_hops,
            max_hops,
            var_length,
            var_length_prop_filters,
        } => try_replace_scan(conn, input, alias, prop, lit).map(|new_input| LogicalOp::Expand {
            input: Box::new(new_input),
            src_alias: src_alias.clone(),
            dst_alias: dst_alias.clone(),
            rel_alias: rel_alias.clone(),
            edge_types: edge_types.clone(),
            direction: *direction,
            min_hops: *min_hops,
            max_hops: *max_hops,
            var_length: *var_length,
            var_length_prop_filters: var_length_prop_filters.clone(),
        }),

        LogicalOp::CrossProduct { left, right } => {
            if let Some(new_left) = try_replace_scan(conn, left, alias, prop, lit) {
                Some(LogicalOp::CrossProduct {
                    left: Box::new(new_left),
                    right: right.clone(),
                })
            } else {
                try_replace_scan(conn, right, alias, prop, lit).map(|new_right| {
                    LogicalOp::CrossProduct {
                        left: left.clone(),
                        right: Box::new(new_right),
                    }
                })
            }
        }

        _ => None,
    }
}

/// Extract the column names from a statement's RETURN clause for UNION validation.
fn return_items_columns(items: &[ReturnItem]) -> Vec<String> {
    use crate::cypher::eval::expr_to_column_name;
    items
        .iter()
        .map(|item| {
            item.alias
                .clone()
                .unwrap_or_else(|| expr_to_column_name(&item.expr))
        })
        .collect()
}

fn statement_return_columns(stmt: &Statement) -> Vec<String> {
    match stmt {
        Statement::Match(s) => return_items_columns(&s.return_clause.items),
        Statement::Return(s) => return_items_columns(&s.return_clause.items),
        Statement::Create(s) => s
            .return_clause
            .as_ref()
            .map(|rc| return_items_columns(&rc.items))
            .unwrap_or_default(),
        Statement::Unwind(s) => match &s.body {
            UnwindBody::Return { return_clause, .. } => return_items_columns(&return_clause.items),
            UnwindBody::Create { return_clause, .. } => return_clause
                .as_ref()
                .map(|rc| return_items_columns(&rc.items))
                .unwrap_or_default(),
        },
        Statement::MultiClause(s) => s
            .return_clause
            .as_ref()
            .map(|rc| return_items_columns(&rc.items))
            .unwrap_or_default(),
        _ => vec![],
    }
}
