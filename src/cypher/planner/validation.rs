//! Compile-time validation — return-var/expr-type/scoping/pattern-predicate/create/merge/aggregation checks, WithValueKind/VarKind inference.

use std::collections::{HashMap, HashSet};

use crate::types::*;

use super::helpers::*;
use super::*;

pub(in crate::cypher::planner) fn validate_return_variables(
    items: &[ReturnItem],
    scope_vars: &HashSet<String>,
) -> crate::types::Result<()> {
    for item in items {
        if matches!(item.expr.kind, ExprKind::Star) {
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
pub(in crate::cypher::planner) fn check_duplicate_columns(
    items: &[ReturnItem],
) -> crate::types::Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    for item in items {
        if matches!(item.expr.kind, ExprKind::Star) {
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
pub(in crate::cypher::planner) fn validate_expr_types(
    expr: &Expr,
    var_types: &HashMap<String, VarKind>,
) -> crate::types::Result<()> {
    match &expr.kind {
        ExprKind::FunctionCall { name, args, .. } => {
            let name_lower = name.to_ascii_lowercase();
            if !crate::cypher::eval::is_known_function(&name_lower) {
                return Err(unknown_function_error(name, expr.span));
            }
            if let Some((min, max)) = function_arity(&name_lower) {
                let got = args.len();
                if got < min || got > max {
                    let expected = if min == max {
                        format!("{min}")
                    } else {
                        format!("{min} or {max}")
                    };
                    return Err(GraphError::Query(crate::types::QueryError::ArgumentError {
                        phase: crate::types::QueryPhase::SemanticAnalysis,
                        code: ErrorCode::InvalidNumberOfArguments,
                        message: format!("{name}() expected {expected} argument(s) but got {got}"),
                        hint: None,
                        span: Some(expr.span),
                    }));
                }
            }
            if let Some(Expr {
                kind: ExprKind::Variable(var),
                ..
            }) = args.first()
            {
                if let Some(kind) = var_types.get(var) {
                    match name_lower.as_str() {
                        "type" if *kind == VarKind::Node => {
                            return Err(GraphError::type_error(
                                crate::types::QueryPhase::SemanticAnalysis,
                                "type() requires a relationship".to_string(),
                            )
                            .with_code(ErrorCode::InvalidArgumentType));
                        }
                        "length" if *kind == VarKind::Node || *kind == VarKind::Relationship => {
                            return Err(GraphError::type_error(
                                crate::types::QueryPhase::SemanticAnalysis,
                                "length() requires a path, string, or list".to_string(),
                            )
                            .with_code(ErrorCode::InvalidArgumentType));
                        }
                        "size" if *kind == VarKind::Path => {
                            return Err(GraphError::type_error(
                                crate::types::QueryPhase::SemanticAnalysis,
                                "size() requires a string or list".to_string(),
                            )
                            .with_code(ErrorCode::InvalidArgumentType));
                        }
                        "toboolean" | "tointeger" | "tofloat" | "tostring"
                            if *kind == VarKind::Node || *kind == VarKind::Relationship =>
                        {
                            return Err(GraphError::type_error(
                                crate::types::QueryPhase::SemanticAnalysis,
                                format!("{name}() cannot convert a {kind:?}"),
                            )
                            .with_code(ErrorCode::InvalidArgumentValue));
                        }
                        _ => {}
                    }
                }
            }
            for arg in args {
                validate_expr_types(arg, var_types)?;
            }
        }
        ExprKind::Property(var, _) => {
            if let Some(kind) = var_types.get(var) {
                if *kind == VarKind::Path {
                    return Err(GraphError::type_error(
                        crate::types::QueryPhase::SemanticAnalysis,
                        "property access on a path is not allowed".to_string(),
                    )
                    .with_code(ErrorCode::InvalidArgumentType));
                }
            }
        }
        ExprKind::BinaryOp { left, right, .. } => {
            validate_expr_types(left, var_types)?;
            validate_expr_types(right, var_types)?;
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            validate_expr_types(inner, var_types)?;
        }
        ExprKind::Quantifier {
            list_expr,
            variable,
            predicate,
            ..
        } => {
            if let ExprKind::List(items) = &list_expr.as_ref().kind {
                if !items.is_empty() {
                    let all_strings = items.iter().all(|e| {
                        matches!(
                            e.kind,
                            ExprKind::Literal(crate::cypher::ast::LiteralValue::String(_))
                        )
                    });
                    let all_booleans = items.iter().all(|e| {
                        matches!(
                            e.kind,
                            ExprKind::Literal(crate::cypher::ast::LiteralValue::Bool(_))
                        )
                    });
                    if (all_strings || all_booleans)
                        && predicate_uses_arithmetic_on(predicate, variable)
                    {
                        let elem_type = if all_strings { "String" } else { "Boolean" };
                        return Err(GraphError::type_error(
                            crate::types::QueryPhase::SemanticAnalysis,
                            format!("{elem_type} is not a valid argument type for arithmetic operations"),
                        ).with_code(ErrorCode::InvalidArgumentType));
                    }
                }
            }
            validate_expr_types(list_expr, var_types)?;
        }
        _ => {}
    }
    Ok(())
}

/// Check if an expression uses arithmetic operators on a specific variable.
pub(in crate::cypher::planner) fn predicate_uses_arithmetic_on(expr: &Expr, var: &str) -> bool {
    match &expr.kind {
        ExprKind::BinaryOp { left, op, right } => {
            let is_arith = matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow
            );
            if is_arith && (expr_references_var(left, var) || expr_references_var(right, var)) {
                return true;
            }
            predicate_uses_arithmetic_on(left, var) || predicate_uses_arithmetic_on(right, var)
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            predicate_uses_arithmetic_on(inner, var)
        }
        ExprKind::FunctionCall { args, .. } => {
            args.iter().any(|a| predicate_uses_arithmetic_on(a, var))
        }
        _ => false,
    }
}

/// Check if an expression directly references a variable by name.
pub(in crate::cypher::planner) fn expr_references_var(expr: &Expr, var: &str) -> bool {
    match &expr.kind {
        ExprKind::Variable(v) => v == var,
        ExprKind::Property(v, _) => v == var,
        ExprKind::BinaryOp { left, right, .. } => {
            expr_references_var(left, var) || expr_references_var(right, var)
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            expr_references_var(inner, var)
        }
        ExprKind::FunctionCall { args, .. } => args.iter().any(|a| expr_references_var(a, var)),
        _ => false,
    }
}

/// Check that every variable reference in an expression is present in `scope`.
pub(in crate::cypher::planner) fn check_expr_variables(
    expr: &Expr,
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    let mut props = Vec::new();
    collect_property_refs(expr, &mut props);
    check_expr_variables_inner(expr, scope, &props)
}

pub(in crate::cypher::planner) fn check_expr_variables_inner(
    expr: &Expr,
    scope: &HashSet<String>,
    seen_props: &[(String, String)],
) -> crate::types::Result<()> {
    match &expr.kind {
        ExprKind::Variable(var) => {
            if !scope.contains(var) {
                return Err(undefined_variable_error_with_props(
                    var, scope, seen_props, expr.span,
                ));
            }
        }
        ExprKind::Property(var, _) => {
            if !scope.contains(var) {
                return Err(undefined_variable_error_with_props(
                    var, scope, seen_props, expr.span,
                ));
            }
        }
        ExprKind::BinaryOp { left, right, .. } => {
            check_expr_variables_inner(left, scope, seen_props)?;
            check_expr_variables_inner(right, scope, seen_props)?;
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            check_expr_variables_inner(inner, scope, seen_props)?;
        }
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                check_expr_variables_inner(arg, scope, seen_props)?;
            }
        }
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            if let Some(o) = operand {
                check_expr_variables_inner(o, scope, seen_props)?;
            }
            for (cond, result) in alternatives {
                check_expr_variables_inner(cond, scope, seen_props)?;
                check_expr_variables_inner(result, scope, seen_props)?;
            }
            if let Some(d) = default {
                check_expr_variables_inner(d, scope, seen_props)?;
            }
        }
        ExprKind::List(items) => {
            for item in items {
                check_expr_variables_inner(item, scope, seen_props)?;
            }
        }
        ExprKind::MapLiteral(pairs) => {
            for (_, v) in pairs {
                check_expr_variables_inner(v, scope, seen_props)?;
            }
        }
        ExprKind::Index { expr, index } => {
            check_expr_variables_inner(expr, scope, seen_props)?;
            check_expr_variables_inner(index, scope, seen_props)?;
        }
        ExprKind::Slice { expr, start, end } => {
            check_expr_variables_inner(expr, scope, seen_props)?;
            if let Some(s) = start {
                check_expr_variables_inner(s, scope, seen_props)?;
            }
            if let Some(e) = end {
                check_expr_variables_inner(e, scope, seen_props)?;
            }
        }
        ExprKind::Literal(_) | ExprKind::Parameter(_) | ExprKind::Star => {}
        ExprKind::ListComprehension { list_expr, .. } => {
            check_expr_variables_inner(list_expr, scope, seen_props)?;
        }
        ExprKind::Quantifier { list_expr, .. } => {
            check_expr_variables_inner(list_expr, scope, seen_props)?;
        }
        ExprKind::Exists { .. }
        | ExprKind::ExistsSubquery(_)
        | ExprKind::PatternPredicate(_)
        | ExprKind::PatternComprehension { .. } => {}
        ExprKind::DotAccess { expr, .. } => {
            check_expr_variables_inner(expr, scope, seen_props)?;
        }
        ExprKind::HasLabel(var, _) => {
            if !scope.contains(var) {
                return Err(
                    GraphError::syntax(var.to_string()).with_code(ErrorCode::UndefinedVariable)
                );
            }
        }
    }
    Ok(())
}

/// Validate pattern predicate variables in WHERE clauses.
/// All named variables in a PatternPredicate must be in scope.
/// Does NOT recurse into ExistsSubquery (those have their own scope).
pub(in crate::cypher::planner) fn validate_pattern_predicate_vars(
    expr: &Expr,
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    match &expr.kind {
        ExprKind::PatternPredicate(pattern) => {
            // Self-pattern check: single bound node is not a valid predicate.
            if pattern.elements.len() == 1 {
                if let PatternElement::Node(n) = &pattern.elements[0] {
                    if n.variable.as_ref().is_some_and(|v| scope.contains(v)) {
                        return Err(GraphError::type_error(
                            crate::types::QueryPhase::SemanticAnalysis,
                            "a single node pattern is not a valid predicate".to_string(),
                        )
                        .with_code(ErrorCode::InvalidArgumentType));
                    }
                }
            }
            // All named variables must already be in scope.
            for elem in &pattern.elements {
                match elem {
                    PatternElement::Node(n) => {
                        if let Some(ref var) = n.variable {
                            if !scope.contains(var) {
                                return Err(GraphError::syntax(var.to_string())
                                    .with_code(ErrorCode::UndefinedVariable));
                            }
                        }
                    }
                    PatternElement::Relationship(r) => {
                        if let Some(ref var) = r.variable {
                            if !scope.contains(var) {
                                return Err(GraphError::syntax(var.to_string())
                                    .with_code(ErrorCode::UndefinedVariable));
                            }
                        }
                    }
                }
            }
        }
        // Recurse into sub-expressions, but NOT into ExistsSubquery (own scope).
        ExprKind::BinaryOp { left, right, .. } => {
            validate_pattern_predicate_vars(left, scope)?;
            validate_pattern_predicate_vars(right, scope)?;
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            validate_pattern_predicate_vars(inner, scope)?;
        }
        _ => {}
    }
    Ok(())
}

/// Reject pattern predicate expressions in non-WHERE contexts (RETURN, WITH, SET).
pub(in crate::cypher::planner) fn reject_pattern_predicates(
    expr: &Expr,
) -> crate::types::Result<()> {
    match &expr.kind {
        ExprKind::PatternPredicate(_) => {
            return Err(
                GraphError::syntax("pattern expressions are not allowed here".to_string())
                    .with_code(ErrorCode::UnexpectedSyntax),
            );
        }
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                reject_pattern_predicates(arg)?;
            }
        }
        ExprKind::BinaryOp { left, right, .. } => {
            reject_pattern_predicates(left)?;
            reject_pattern_predicates(right)?;
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            reject_pattern_predicates(inner)?;
        }
        ExprKind::List(items) => {
            for item in items {
                reject_pattern_predicates(item)?;
            }
        }
        ExprKind::Index { expr, index } => {
            reject_pattern_predicates(expr)?;
            reject_pattern_predicates(index)?;
        }
        ExprKind::DotAccess { expr, .. } => {
            reject_pattern_predicates(expr)?;
        }
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            if let Some(o) = operand {
                reject_pattern_predicates(o)?;
            }
            for (cond, result) in alternatives {
                reject_pattern_predicates(cond)?;
                reject_pattern_predicates(result)?;
            }
            if let Some(d) = default {
                reject_pattern_predicates(d)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Collect all variable names (nodes, relationships, and path variables) from a set of patterns.
pub(in crate::cypher::planner) fn collect_pattern_variables(
    patterns: &[Pattern],
) -> HashSet<String> {
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
pub(in crate::cypher::planner) fn validate_create_patterns(
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
                            "a relationship must have exactly one type in CREATE",
                        )
                        .with_code(ErrorCode::NoSingleRelationshipType));
                    }
                    if rel.rel_types.len() > 1 {
                        return Err(GraphError::syntax(
                            "a relationship must have exactly one type in CREATE",
                        )
                        .with_code(ErrorCode::NoSingleRelationshipType));
                    }
                    // CREATE relationships must be directed.
                    if rel.direction == RelDirection::Undirected {
                        return Err(GraphError::syntax(
                            "only directed relationships are supported in CREATE",
                        )
                        .with_code(ErrorCode::RequiresDirectedRelationship));
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
pub(in crate::cypher::planner) fn validate_merge_pattern(
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
                        "a relationship must have exactly one type in MERGE",
                    )
                    .with_code(ErrorCode::NoSingleRelationshipType));
                }
                if rel.rel_types.len() > 1 {
                    return Err(GraphError::syntax(
                        "a relationship must have exactly one type in MERGE",
                    )
                    .with_code(ErrorCode::NoSingleRelationshipType));
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
                    return Err(GraphError::syntax(a.variable.to_string())
                        .with_code(ErrorCode::UndefinedVariable));
                }
                check_expr_variables(&a.value, &merge_vars)?;
            }
            SetItem::Label { variable, .. } => {
                if !merge_vars.contains(variable) {
                    return Err(GraphError::syntax(variable.to_string())
                        .with_code(ErrorCode::UndefinedVariable));
                }
            }
            SetItem::MapOverwrite { variable, value } | SetItem::MapMerge { variable, value } => {
                if !merge_vars.contains(variable) {
                    return Err(GraphError::syntax(variable.to_string())
                        .with_code(ErrorCode::UndefinedVariable));
                }
                check_expr_variables(value, &merge_vars)?;
            }
        }
    }
    Ok(())
}

/// Validate that SET items don't reference undefined variables.
pub(in crate::cypher::planner) fn validate_set_variables(
    items: &[SetItem],
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    for item in items {
        match item {
            SetItem::Property(a) => {
                if !scope.contains(&a.variable) {
                    return Err(GraphError::syntax(a.variable.to_string())
                        .with_code(ErrorCode::UndefinedVariable));
                }
                check_expr_variables(&a.value, scope)?;
                reject_pattern_predicates(&a.value)?;
            }
            SetItem::Label { variable, .. } => {
                if !scope.contains(variable) {
                    return Err(GraphError::syntax(variable.to_string())
                        .with_code(ErrorCode::UndefinedVariable));
                }
            }
            SetItem::MapOverwrite { variable, value } | SetItem::MapMerge { variable, value } => {
                if !scope.contains(variable) {
                    return Err(GraphError::syntax(variable.to_string())
                        .with_code(ErrorCode::UndefinedVariable));
                }
                check_expr_variables(value, scope)?;
            }
        }
    }
    Ok(())
}

/// Validate that DELETE variable references are in scope.
pub(in crate::cypher::planner) fn validate_delete_exprs(
    exprs: &[crate::cypher::ast::Expr],
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    use crate::cypher::ast::ExprKind;
    for expr in exprs {
        match &expr.kind {
            ExprKind::Variable(var) => {
                if !scope.contains(var) {
                    return Err(
                        GraphError::syntax(var.to_string()).with_code(ErrorCode::UndefinedVariable)
                    );
                }
            }
            // HasLabel expression (e.g. `n:Person`) is not a valid DELETE target.
            ExprKind::HasLabel(..) => {
                return Err(
                    GraphError::syntax("cannot delete a label predicate expression")
                        .with_code(ErrorCode::InvalidDelete),
                );
            }
            // Literal expressions are not valid DELETE targets.
            ExprKind::Literal(_) => {
                return Err(
                    GraphError::syntax("DELETE requires a node, relationship, or path")
                        .with_code(ErrorCode::InvalidArgumentType),
                );
            }
            // Binary/arithmetic expressions are not valid DELETE targets.
            ExprKind::BinaryOp { .. } => {
                return Err(
                    GraphError::syntax("DELETE requires a node, relationship, or path")
                        .with_code(ErrorCode::InvalidArgumentType),
                );
            }
            // Property access (e.g. nodes.key), index (e.g. friends[0]) — valid.
            // DotAccess (e.g. rels.key.key[0]) — valid.
            _ => {
                if let Some(root) = extract_root_variable(expr) {
                    if !scope.contains(&root) {
                        return Err(GraphError::syntax(root.to_string())
                            .with_code(ErrorCode::UndefinedVariable));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Extract the root variable name from an expression tree.
pub(in crate::cypher::planner) fn extract_root_variable(
    expr: &crate::cypher::ast::Expr,
) -> Option<String> {
    use crate::cypher::ast::ExprKind;
    match &expr.kind {
        ExprKind::Variable(v) => Some(v.clone()),
        ExprKind::Property(v, _) => Some(v.clone()),
        ExprKind::Index { expr: base, .. } => extract_root_variable(base),
        _ => None,
    }
}

/// Check if ORDER BY contains aggregate functions when the RETURN/WITH itself
/// does not use aggregation. This is invalid per openCypher spec.
pub(in crate::cypher::planner) fn validate_no_aggregation_in_order_by(
    order_by: &[SortItem],
) -> crate::types::Result<()> {
    for item in order_by {
        if is_aggregate_fn(&item.expr) {
            return Err(GraphError::syntax(
                "aggregation functions are not allowed in ORDER BY when there is no aggregation in the preceding WITH/RETURN"
            ).with_code(ErrorCode::InvalidAggregation));
        }
    }
    Ok(())
}

/// Check if ORDER BY after RETURN with aggregates references non-returned
/// non-aggregate expressions (variables consumed by aggregation).
pub(in crate::cypher::planner) fn validate_return_order_by_with_aggregates(
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
                    return Err(
                        GraphError::syntax("ORDER BY references a variable not in RETURN")
                            .with_code(ErrorCode::UndefinedVariable),
                    );
                }
            }
        }
    }
    Ok(())
}

/// Validate that RETURN DISTINCT + ORDER BY only sorts by columns in the DISTINCT projection.
pub(in crate::cypher::planner) fn validate_distinct_order_by(
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
        if let ExprKind::Variable(var) = &item.expr.kind {
            returned_vars.insert(var.clone());
        }
    }
    for sort_item in order_by {
        let col = crate::cypher::eval::expr_to_column_name(&sort_item.expr);
        if projected.contains(&col) {
            continue;
        }
        // Allow property access on returned variables (e.g. ORDER BY a.name when RETURN DISTINCT a).
        if let ExprKind::Property(var, _) = &sort_item.expr.kind {
            if returned_vars.contains(var) {
                continue;
            }
        }
        return Err(
            GraphError::syntax("ORDER BY references a variable not in RETURN DISTINCT")
                .with_code(ErrorCode::UndefinedVariable),
        );
    }
    Ok(())
}

/// Validate that ORDER BY in WITH only references variables available in the
/// input scope (from the prior clause). The input scope is what was projected
/// by the previous WITH/MATCH, so variables dropped earlier are caught.
pub(in crate::cypher::planner) fn validate_with_order_by_scope_input(
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
        if matches!(item.expr.kind, ExprKind::Star) {
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
pub(in crate::cypher::planner) fn validate_agg_order_by_scope(
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
pub(in crate::cypher::planner) fn collect_variables_from_expr(
    expr: &Expr,
    vars: &mut HashSet<String>,
) {
    match &expr.kind {
        ExprKind::Variable(name) => {
            vars.insert(name.clone());
        }
        ExprKind::Property(var, _) => {
            vars.insert(var.clone());
        }
        ExprKind::BinaryOp { left, right, .. } => {
            collect_variables_from_expr(left, vars);
            collect_variables_from_expr(right, vars);
        }
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                collect_variables_from_expr(arg, vars);
            }
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            collect_variables_from_expr(inner, vars);
        }
        _ => {}
    }
}

/// Walk an expression, skipping aggregate function subtrees, and check that
/// remaining variable references are in the given scope.
pub(in crate::cypher::planner) fn validate_non_agg_leaves_in_scope(
    expr: &Expr,
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    match &expr.kind {
        ExprKind::FunctionCall { name, args, .. } => {
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
        ExprKind::Variable(name) if !scope.contains(name) => {
            return Err(undefined_variable_error(name, scope, expr.span));
        }
        ExprKind::Property(var, _) if !scope.contains(var) => {
            return Err(undefined_variable_error(var, scope, expr.span));
        }
        ExprKind::BinaryOp { left, right, .. } => {
            validate_non_agg_leaves_in_scope(left, scope)?;
            validate_non_agg_leaves_in_scope(right, scope)?;
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            validate_non_agg_leaves_in_scope(inner, scope)?;
        }
        _ => {} // Literals, parameters, Star, in-scope Variable/Property — always valid.
    }
    Ok(())
}

/// Check that all variable references in an expression are in the given scope.
pub(in crate::cypher::planner) fn validate_expr_in_scope(
    expr: &Expr,
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    match &expr.kind {
        ExprKind::Variable(name) if !scope.contains(name) => {
            return Err(undefined_variable_error(name, scope, expr.span));
        }
        ExprKind::Property(var, _) if !scope.contains(var) => {
            return Err(undefined_variable_error(var, scope, expr.span));
        }
        ExprKind::BinaryOp { left, right, .. } => {
            validate_expr_in_scope(left, scope)?;
            validate_expr_in_scope(right, scope)?;
        }
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                validate_expr_in_scope(arg, scope)?;
            }
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            validate_expr_in_scope(inner, scope)?;
        }
        ExprKind::List(items) => {
            for item in items {
                validate_expr_in_scope(item, scope)?;
            }
        }
        _ => {} // Literals, Star, etc. — always valid.
    }
    Ok(())
}

/// Check for aggregation functions in a list comprehension mapping expression.
pub(in crate::cypher::planner) fn validate_no_aggregation_in_list_comp(
    expr: &Expr,
) -> crate::types::Result<()> {
    match &expr.kind {
        ExprKind::ListComprehension {
            map_expr,
            filter,
            list_expr,
            ..
        } => {
            if let Some(ref me) = map_expr {
                if is_aggregate_fn(me) {
                    return Err(GraphError::syntax(
                        "aggregation functions are not allowed in list comprehension",
                    )
                    .with_code(ErrorCode::InvalidAggregation));
                }
                validate_no_aggregation_in_list_comp(me)?;
            }
            if let Some(ref fe) = filter {
                validate_no_aggregation_in_list_comp(fe)?;
            }
            validate_no_aggregation_in_list_comp(list_expr)?;
        }
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                validate_no_aggregation_in_list_comp(arg)?;
            }
        }
        ExprKind::BinaryOp { left, right, .. } => {
            validate_no_aggregation_in_list_comp(left)?;
            validate_no_aggregation_in_list_comp(right)?;
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            validate_no_aggregation_in_list_comp(inner)?;
        }
        ExprKind::List(items) => {
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
pub(in crate::cypher::planner) enum WithValueKind {
    Scalar, // literal, property access, list, etc.
    Node,   // conservative pass-through — variables, function calls
}

/// Infer the kind of value a WITH/RETURN expression produces.
/// Only returns Scalar for expressions that definitely cannot be structural
/// (nodes, relationships, paths). Conservative: unknown → Node (passes through).
pub(in crate::cypher::planner) fn infer_with_value_kind(expr: &Expr) -> WithValueKind {
    match &expr.kind {
        // Definitely scalar: literals, property access, arithmetic, map literals.
        ExprKind::Literal(_) => WithValueKind::Scalar,
        ExprKind::MapLiteral(_) => WithValueKind::Scalar,
        ExprKind::Property(_, _) => WithValueKind::Scalar,
        ExprKind::BinaryOp { .. } => WithValueKind::Scalar,
        // A list is always a list value, not a node/relationship.
        ExprKind::List(_) => WithValueKind::Scalar,
        // Function calls, index, slice, variables could return structural types.
        _ => WithValueKind::Node, // Pass through — could be node/rel/path
    }
}

/// Validate that MERGE node variables that are already bound aren't being
/// re-created with new labels or predicates.
pub(in crate::cypher::planner) fn validate_merge_variable_rebinding(
    pattern: &Pattern,
    scope: &HashSet<String>,
) -> crate::types::Result<()> {
    // Check for single-node MERGE with already-bound variable.
    if pattern.elements.len() == 1 {
        if let PatternElement::Node(n) = &pattern.elements[0] {
            if let Some(ref var) = n.variable {
                if scope.contains(var) {
                    return Err(
                        GraphError::syntax(format!("variable `{var}` already bound"))
                            .with_code(ErrorCode::VariableAlreadyBound),
                    );
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
                        return Err(
                            GraphError::syntax(format!("variable `{var}` already bound"))
                                .with_code(ErrorCode::VariableAlreadyBound),
                        );
                    }
                }
            }
            if let PatternElement::Node(n) = elem {
                if let Some(ref var) = n.variable {
                    if scope.contains(var) && !n.labels.is_empty() {
                        return Err(
                            GraphError::syntax(format!("variable `{var}` already bound"))
                                .with_code(ErrorCode::VariableAlreadyBound),
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cypher::planner) enum VarKind {
    Node,
    Relationship,
    Path,
}

/// Validate that no variable is used as more than one type (node, relationship,
/// path) within the same MATCH statement's patterns. Raises `SyntaxError` on
/// conflicts — e.g. `MATCH ()-[r]-(r)` uses `r` as both relationship and node.
pub(in crate::cypher::planner) fn validate_variable_types(
    patterns: &[Pattern],
) -> crate::types::Result<HashMap<String, VarKind>> {
    let mut types: HashMap<String, VarKind> = HashMap::new();
    validate_variable_types_with_map(patterns, &mut types)?;
    Ok(types)
}

/// Validate variable types against a persistent type map across clauses.
pub(in crate::cypher::planner) fn validate_variable_types_with_map(
    patterns: &[Pattern],
    types: &mut std::collections::HashMap<String, VarKind>,
) -> crate::types::Result<()> {
    // Snapshot relationship vars already in the map from prior MATCHes.
    // These are "bound" rels that may legally appear in this MATCH.
    let prior_rels: HashSet<String> = types
        .iter()
        .filter(|(_, &k)| k == VarKind::Relationship)
        .map(|(n, _)| n.clone())
        .collect();

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
                        code: ErrorCode::Other,
                        hint: None,
                        span: None,
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
                                        code: ErrorCode::Other,
                                        hint: None,
                                        span: None,
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
                                        code: ErrorCode::Other,
                                        hint: None,
                                        span: None,
                                    },
                                ));
                            }
                            // Relationship variables cannot be reused in the
                            // same MATCH clause (unlike node variables).
                            // However, a rel bound in a *prior* MATCH is legal
                            // (bound relationship reference in a cross-MATCH pattern).
                            if !prior_rels.contains(var) {
                                return Err(GraphError::syntax(format!(
                                    "cannot use relationship variable '{var}' more than once in a pattern"
                                )).with_code(ErrorCode::VariableAlreadyBound));
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
