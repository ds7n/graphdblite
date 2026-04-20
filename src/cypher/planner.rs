use std::collections::HashSet;

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::ir::*;
use crate::index;
use crate::types::{Direction, GraphError};

/// Apply RETURN projection (+ DISTINCT, ORDER BY, SKIP, LIMIT) to a plan operator.
fn apply_return_projection(
    mut op: LogicalOp,
    return_clause: &ReturnClause,
    order_by: &[SortItem],
    skip: Option<u64>,
    limit: Option<u64>,
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
            items: order_by.to_vec(),
        };
    }

    if let Some(count) = skip {
        op = LogicalOp::Skip {
            input: Box::new(op),
            count,
        };
    }

    if let Some(count) = limit {
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
        Statement::Create(c) => plan_create(c),
        Statement::MatchCreate(mc) => plan_match_create(conn, mc),
        Statement::Delete(d) => plan_delete(conn, d),
        Statement::Set(s) => plan_set(conn, s),
        Statement::Remove(r) => plan_remove(conn, r),
        Statement::Merge(m) => plan_merge(m),
        Statement::MatchMerge(mm) => plan_match_merge(conn, mm),
        Statement::Unwind(u) => plan_unwind(conn, u),
        Statement::Return(r) => plan_return(r),
        Statement::MultiClause(mc) => plan_multi_clause(conn, mc),
        Statement::Explain(inner) => plan(conn, inner),
        Statement::Union { statements, all } => {
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
    validate_variable_types(&stmt.patterns)?;

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
                op = plan_with(op, with)?;
                // WITH resets scope to only the projected aliases.
                scope_vars.clear();
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

    // ORDER BY.
    if !stmt.order_by.is_empty() {
        op = LogicalOp::Sort {
            input: Box::new(op),
            items: stmt.order_by.clone(),
        };
    }

    // SKIP.
    if let Some(count) = stmt.skip {
        op = LogicalOp::Skip {
            input: Box::new(op),
            count,
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

/// Plan a standalone `RETURN` statement (no preceding MATCH).
fn plan_return(stmt: &ReturnStatement) -> crate::types::Result<LogicalOp> {
    let mut op: LogicalOp = LogicalOp::SingleRow;

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

    if !stmt.order_by.is_empty() {
        op = LogicalOp::Sort {
            input: Box::new(op),
            items: stmt.order_by.clone(),
        };
    }

    if let Some(count) = stmt.skip {
        op = LogicalOp::Skip {
            input: Box::new(op),
            count,
        };
    }

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
        op = apply_return_projection(op, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
    }

    Ok(op)
}

fn plan_match_create(
    conn: &Connection,
    stmt: &MatchCreateStatement,
) -> crate::types::Result<LogicalOp> {
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
        result = apply_return_projection(result, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
    }

    Ok(result)
}

fn plan_delete(conn: &Connection, stmt: &DeleteStatement) -> crate::types::Result<LogicalOp> {
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
        variables: stmt.variables.clone(),
        detach: stmt.detach,
    };

    if let Some(ref rc) = stmt.return_clause {
        op = apply_return_projection(op, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
    }

    Ok(op)
}

fn plan_set(conn: &Connection, stmt: &SetStatement) -> crate::types::Result<LogicalOp> {
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
                op = plan_with(op, with)?;
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
        op = apply_return_projection(op, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
    }

    Ok(op)
}

fn plan_remove(conn: &Connection, stmt: &RemoveStatement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_patterns(conn, &stmt.patterns)?;

    // Optional MATCH clauses.
    for opt_match in &stmt.optional_patterns {
        let (right, new_aliases, opt_filter) =
            plan_optional_match(conn, opt_match, &HashSet::new())?;
        op = LogicalOp::LeftOuterJoin {
            input: Box::new(op),
            right: Box::new(right),
            optional_aliases: new_aliases,
            opt_filter,
        };
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
        op = apply_return_projection(op, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
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
                        op = plan_with(op, with)?;
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

            if let Some(count) = skip {
                op = LogicalOp::Skip {
                    input: Box::new(op),
                    count: *count,
                };
            }

            if let Some(count) = limit {
                op = LogicalOp::Limit {
                    input: Box::new(op),
                    count: *count,
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
                        op = plan_with(op, with)?;
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
                op = apply_return_projection(op, rc, order_by, *skip, *limit)?;
            }
        }
    }

    Ok(op)
}

fn plan_merge(stmt: &MergeStatement) -> crate::types::Result<LogicalOp> {
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
                    PatternElement::Relationship(_),
                    PatternElement::Node(_),
                ) => {}
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
        op = apply_return_projection(op, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
    }

    Ok(op)
}

fn plan_match_merge(
    conn: &Connection,
    stmt: &MatchMergeStatement,
) -> crate::types::Result<LogicalOp> {
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
        result = apply_return_projection(result, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
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

    for clause in &stmt.clauses {
        match clause {
            Clause::Match {
                patterns,
                optional_patterns,
                where_clause,
            } => {
                // Validate variable-type consistency (node vs rel vs path) across all clauses.
                validate_variable_types_with_map(patterns, &mut var_types)?;
                let match_op = plan_patterns(conn, patterns)?;
                op = Some(if let Some(input) = op.take() {
                    // Correlated join: thread prior rows into MATCH.
                    let mut joined = LogicalOp::CorrelatedJoin {
                        input: Box::new(input),
                        right: Box::new(match_op),
                    };
                    // Optional matches.
                    let mut bound_vars = collect_pattern_variables(patterns);
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
                let input = op.take().unwrap_or(LogicalOp::EmptyRow);
                op = Some(plan_with(input, with)?);
                // WITH resets scope.
                let old_scope = scope_vars.clone();
                scope_vars.clear();
                for item in &with.items {
                    if let Expr::Star = &item.expr {
                        scope_vars = old_scope.clone();
                    } else if let Some(ref alias) = item.alias {
                        scope_vars.insert(alias.clone());
                    } else if let Expr::Variable(var) = &item.expr {
                        scope_vars.insert(var.clone());
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
            Clause::Delete { variables, detach } => {
                let input = op
                    .take()
                    .ok_or_else(|| GraphError::semantic("DELETE requires preceding MATCH"))?;
                op = Some(LogicalOp::Delete {
                    input: Box::new(input),
                    variables: variables.clone(),
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
        result = apply_return_projection(result, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
    }

    Ok(result)
}

/// Plan a WITH clause as an intermediate projection (+aggregation) and optional filter.
fn plan_with(input: LogicalOp, with: &WithClause) -> crate::types::Result<LogicalOp> {
    let mut op = input;

    // Check if WITH items contain aggregates.
    let has_aggregates = with.items.iter().any(|item| is_aggregate_fn(&item.expr));

    if has_aggregates {
        let (group_keys, aggregates) = split_aggregates(&with.items)?;
        op = LogicalOp::Aggregate {
            input: Box::new(op),
            group_keys,
            aggregates,
        };
    }

    // Project the WITH items. Keep flat shape — downstream operators rely on
    // `var.__id` / `var.prop` flat fields.
    op = LogicalOp::Project {
        input: Box::new(op),
        items: with.items.clone(),
        emit_compound: false,
    };

    // Apply WITH's WHERE filter before ORDER BY/SKIP/LIMIT (Cypher semantics).
    if let Some(ref predicate) = with.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    // Apply ORDER BY.
    if !with.order_by.is_empty() {
        op = LogicalOp::Sort {
            input: Box::new(op),
            items: with.order_by.clone(),
        };
    }

    // Apply SKIP.
    if let Some(count) = with.skip {
        op = LogicalOp::Skip {
            input: Box::new(op),
            count,
        };
    }

    // Apply LIMIT.
    if let Some(count) = with.limit {
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
            continue;
        }
        check_expr_variables(&item.expr, scope_vars)?;
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
            alternatives,
            default,
        } => {
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
        Expr::Exists { .. } => {}
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    Relationship,
    Path,
}

/// Validate that no variable is used as more than one type (node, relationship,
/// path) within the same MATCH statement's patterns. Raises `SyntaxError` on
/// conflicts — e.g. `MATCH ()-[r]-(r)` uses `r` as both relationship and node.
fn validate_variable_types(patterns: &[Pattern]) -> crate::types::Result<()> {
    let mut types: std::collections::HashMap<String, VarKind> = std::collections::HashMap::new();
    validate_variable_types_with_map(patterns, &mut types)
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
                    dst_alias: dst_alias.clone(),
                    rel_alias: rel.variable.clone(),
                    edge_types: rel.rel_types.clone(),
                    direction,
                    min_hops,
                    max_hops,
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

                i += 2; // skip rel + dst node
            }
        }
    }

    let mut result = op.ok_or_else(|| GraphError::Serialization("empty pattern".to_string()))?;

    // If this pattern has a path variable binding, wrap with MaterializePath.
    if let Some(ref path_var) = pattern.path_variable {
        let mut node_aliases = Vec::new();
        let mut rel_aliases = Vec::new();
        for elem in &pattern.elements {
            match elem {
                PatternElement::Node(n) => {
                    if let Some(ref var) = n.variable {
                        node_aliases.push(var.clone());
                    }
                }
                PatternElement::Relationship(r) => {
                    if let Some(ref var) = r.variable {
                        rel_aliases.push(var.clone());
                    }
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

                let src = last_alias.clone().ok_or_else(|| {
                    GraphError::Serialization("edge without source node".to_string())
                })?;
                let dst = dst_alias.clone().ok_or_else(|| {
                    GraphError::Serialization("edge target must have a variable".to_string())
                })?;

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
        Expr::FunctionCall { name, .. } => matches!(
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
        ),
        // Recursively check sub-expressions (e.g. `count(a) > 0`).
        Expr::BinaryOp { left, right, .. } => is_aggregate_fn(left) || is_aggregate_fn(right),
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            is_aggregate_fn(inner)
        }
        _ => false,
    }
}

/// Split RETURN/WITH items into group keys (non-aggregate) and aggregate expressions.
fn split_aggregates(items: &[ReturnItem]) -> crate::types::Result<(Vec<Expr>, Vec<AggregateExpr>)> {
    let mut group_keys = Vec::new();
    let mut aggregates = Vec::new();

    for item in items {
        if let Expr::FunctionCall {
            name,
            args,
            distinct,
        } = &item.expr
        {
            if let Some(function) = parse_agg_name(name) {
                let input = args.first().cloned().unwrap_or(Expr::Star);
                let extra_arg = args.get(1).cloned();
                aggregates.push(AggregateExpr {
                    function,
                    input,
                    alias: item.alias.clone(),
                    distinct: *distinct,
                    extra_arg,
                    original_name: name.clone(),
                });
                continue;
            }
        }
        // For non-aggregate expressions, extract any nested aggregates.
        extract_nested_aggregates(&item.expr, &mut aggregates);
        if !is_aggregate_fn(&item.expr) {
            group_keys.push(item.expr.clone());
        }
    }

    Ok((group_keys, aggregates))
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
        } => try_replace_scan(conn, input, alias, prop, lit).map(|new_input| LogicalOp::Expand {
            input: Box::new(new_input),
            src_alias: src_alias.clone(),
            dst_alias: dst_alias.clone(),
            rel_alias: rel_alias.clone(),
            edge_types: edge_types.clone(),
            direction: *direction,
            min_hops: *min_hops,
            max_hops: *max_hops,
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
