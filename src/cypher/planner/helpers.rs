//! Pre-plan helpers — anon counters callers, name suggestions, function arity, sort/return utilities, aggregate detection.

use std::collections::HashSet;

use rusqlite::Connection;

use crate::cypher::record::NamedRecord;
use crate::types::*;

use super::*;

pub(in crate::cypher::planner) fn suggest_close_name<'a>(
    target: &str,
    candidates: impl IntoIterator<Item = &'a String>,
) -> Option<String> {
    // Tighter threshold for short names so `n` doesn't match every 2-char alias.
    let max_dist = if target.len() <= 3 { 1 } else { 2 };
    let target_lc = target.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        let d = levenshtein_lc(&target_lc, &cand.to_ascii_lowercase());
        if d <= max_dist && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand.as_str()));
        }
    }
    best.map(|(_, s)| s.to_string())
}

/// Build an `UndefinedVariable` error with a `did you mean X?` hint when a
/// close in-scope name exists.
pub(in crate::cypher::planner) fn undefined_variable_error(
    name: &str,
    scope: &HashSet<String>,
    span: Span,
) -> GraphError {
    undefined_variable_error_with_props(name, scope, &[], span)
}

/// Like `undefined_variable_error`, but also considers `var.<name>` property
/// references already seen in the surrounding expression. When the user wrote
/// a bare identifier that shadows or matches a property accessed via some
/// in-scope variable, suggest the qualified form.
pub(in crate::cypher::planner) fn undefined_variable_error_with_props(
    name: &str,
    scope: &HashSet<String>,
    seen_props: &[(String, String)],
    span: Span,
) -> GraphError {
    let mut err = GraphError::syntax(name.to_string())
        .with_code(ErrorCode::UndefinedVariable)
        .with_span(span);
    if let Some(suggestion) = suggest_close_name(name, scope) {
        err = err.with_hint(format!("did you mean `{suggestion}`?"));
        return err;
    }
    // Variable→Property hint: if the same expression already references
    // `<v>.<name>` as a property (case-insensitive on the property part) for
    // some in-scope variable `v`, the user likely forgot to qualify.
    let name_lc = name.to_ascii_lowercase();
    if let Some((var, prop)) = seen_props
        .iter()
        .find(|(v, p)| scope.contains(v) && p.to_ascii_lowercase() == name_lc)
    {
        err = err.with_hint(format!("did you mean `{var}.{prop}`?"));
    }
    err
}

/// Collect every `Property(var, prop)` access in an expression tree.
pub(in crate::cypher::planner) fn collect_property_refs(
    expr: &Expr,
    out: &mut Vec<(String, String)>,
) {
    match &expr.kind {
        ExprKind::Property(v, p) => out.push((v.clone(), p.clone())),
        ExprKind::BinaryOp { left, right, .. } => {
            collect_property_refs(left, out);
            collect_property_refs(right, out);
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            collect_property_refs(inner, out);
        }
        ExprKind::FunctionCall { args, .. } => {
            for a in args {
                collect_property_refs(a, out);
            }
        }
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            if let Some(o) = operand {
                collect_property_refs(o, out);
            }
            for (c, r) in alternatives {
                collect_property_refs(c, out);
                collect_property_refs(r, out);
            }
            if let Some(d) = default {
                collect_property_refs(d, out);
            }
        }
        ExprKind::List(items) => {
            for i in items {
                collect_property_refs(i, out);
            }
        }
        ExprKind::MapLiteral(pairs) => {
            for (_, v) in pairs {
                collect_property_refs(v, out);
            }
        }
        ExprKind::Index { expr, index } => {
            collect_property_refs(expr, out);
            collect_property_refs(index, out);
        }
        ExprKind::Slice { expr, start, end } => {
            collect_property_refs(expr, out);
            if let Some(s) = start {
                collect_property_refs(s, out);
            }
            if let Some(e) = end {
                collect_property_refs(e, out);
            }
        }
        ExprKind::ListComprehension { list_expr, .. } | ExprKind::Quantifier { list_expr, .. } => {
            collect_property_refs(list_expr, out)
        }
        ExprKind::DotAccess { expr, .. } => collect_property_refs(expr, out),
        _ => {}
    }
}

/// Build an `UnknownFunction` error with a `did you mean X()?` hint when a
/// known function name is close (Levenshtein) to the misspelled one.
/// Return `(min, max)` argument counts for known fixed-arity scalar functions.
///
/// Returns `None` for variadic functions (`coalesce`), aggregates (`count`,
/// `collect`, …), and temporal constructors (which accept 0 or 1 args of
/// varying shape). The runtime evaluator is the source of truth for those.
pub(in crate::cypher::planner) fn function_arity(name: &str) -> Option<(usize, usize)> {
    let bounds = match name {
        // 1 argument
        "length" | "nodes" | "tolower" | "toupper" | "tostring" | "toboolean" | "tointeger"
        | "tofloat" | "keys" | "labels" | "id" | "type" | "properties" | "relationships"
        | "head" | "last" | "tail" | "size" | "abs" | "sqrt" | "sign" | "ceil" | "floor"
        | "log" | "log10" | "exp" | "reverse" | "startnode" | "endnode" | "trim" | "ltrim"
        | "rtrim" => (1, 1),
        // 2 arguments
        "split" | "left" | "right" => (2, 2),
        // 3 arguments
        "replace" => (3, 3),
        // 1..2 (round has optional precision)
        "round" => (1, 2),
        // 2..3
        "substring" | "range" => (2, 3),
        // 0 arguments
        "e" | "pi" | "rand" => (0, 0),
        _ => return None,
    };
    Some(bounds)
}

pub(in crate::cypher::planner) fn unknown_function_error(name: &str, span: Span) -> GraphError {
    let candidates: Vec<String> = crate::cypher::eval::KNOWN_FUNCTION_NAMES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut err = GraphError::syntax(format!("unknown function `{name}`"))
        .with_code(ErrorCode::UnknownFunction)
        .with_span(span);
    if let Some(suggestion) = suggest_close_name(name, &candidates) {
        err = err.with_hint(format!("did you mean `{suggestion}()`?"));
    }
    err
}

/// Compute Levenshtein edit distance on already-lowercased ASCII byte slices.
pub(in crate::cypher::planner) fn levenshtein_lc(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (cur[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Evaluate a SKIP/LIMIT expression to a u64 at plan time.
///
/// Handles integer literals, float literals (truncated), and simple function
/// calls like `toInteger(rand()*9)`. Parameters are already resolved to
/// literals before planning.
pub(in crate::cypher::planner) fn eval_skip_limit(
    expr: &Expr,
    conn: &Connection,
) -> crate::types::Result<u64> {
    match &expr.kind {
        ExprKind::Literal(LiteralValue::I64(n)) => {
            if *n < 0 {
                return Err(
                    GraphError::syntax("SKIP/LIMIT must be a non-negative integer")
                        .with_code(ErrorCode::NegativeIntegerArgument),
                );
            }
            Ok(*n as u64)
        }
        ExprKind::Literal(LiteralValue::F64(_)) => Err(GraphError::type_error(
            crate::types::QueryPhase::Runtime,
            "SKIP/LIMIT does not accept a floating point value",
        )
        .with_code(ErrorCode::InvalidArgumentType)),
        _ => {
            // Evaluate the expression at plan time with an empty record.
            let rec = NamedRecord::new();
            let val =
                crate::cypher::eval::eval_expr(expr, &rec, crate::cypher::eval::EvalCx::new(conn))?;
            match val {
                Value::I64(n) => {
                    if n < 0 {
                        return Err(GraphError::syntax(
                            "SKIP/LIMIT must be a non-negative integer",
                        )
                        .with_code(ErrorCode::NegativeIntegerArgument));
                    }
                    Ok(n as u64)
                }
                Value::F64(_) => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "SKIP/LIMIT does not accept a floating point value",
                )
                .with_code(ErrorCode::InvalidArgumentType)),
                Value::Null => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "SKIP/LIMIT does not accept NULL",
                )
                .with_code(ErrorCode::InvalidArgumentType)),
                _ => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "SKIP/LIMIT must evaluate to an integer",
                )
                .with_code(ErrorCode::InvalidArgumentType)),
            }
        }
    }
}

/// Resolve RETURN-alias references within a sort expression.
///
/// Sort happens BEFORE projection, so ORDER BY expressions that reference
/// RETURN aliases (e.g. `ORDER BY x` where `RETURN foo.num AS x`) must be
/// rewritten to use the original expression (`foo.num`).
pub(in crate::cypher::planner) fn resolve_sort_aliases(expr: &Expr, items: &[ReturnItem]) -> Expr {
    match &expr.kind {
        ExprKind::Variable(name) => {
            for item in items {
                if item.alias.as_deref() == Some(name) {
                    // Aggregate aliases are stored under the alias by the
                    // Aggregate executor — leave the Variable lookup intact
                    // so the post-Aggregate record's alias key is used.
                    // Substituting the original aggregate call here would
                    // produce a synthetic FunctionCall that misses the
                    // record key (alias != reconstructed column name).
                    if is_aggregate_fn(&item.expr) {
                        return expr.clone();
                    }
                    return item.expr.clone();
                }
            }
            expr.clone()
        }
        ExprKind::BinaryOp { left, op, right } => Expr::synthetic(ExprKind::BinaryOp {
            left: Box::new(resolve_sort_aliases(left, items)),
            op: *op,
            right: Box::new(resolve_sort_aliases(right, items)),
        }),
        ExprKind::Not(inner) => {
            Expr::synthetic(ExprKind::Not(Box::new(resolve_sort_aliases(inner, items))))
        }
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => Expr::synthetic(ExprKind::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| resolve_sort_aliases(a, items))
                .collect(),
            distinct: *distinct,
            original_text: original_text.clone(),
        }),
        _ => expr.clone(),
    }
}

/// Apply RETURN projection (+ DISTINCT, ORDER BY, SKIP, LIMIT) to a plan operator.
pub(in crate::cypher::planner) fn apply_return_projection(
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
pub(in crate::cypher::planner) fn properties_to_filter(
    variable: &str,
    properties: &std::collections::HashMap<String, Expr>,
) -> Expr {
    let mut exprs: Vec<Expr> = properties
        .iter()
        .map(|(key, value)| {
            Expr::synthetic(ExprKind::BinaryOp {
                left: Box::new(Expr::synthetic(ExprKind::Property(
                    variable.to_string(),
                    key.clone(),
                ))),
                op: BinOp::Eq,
                right: Box::new(value.clone()),
            })
        })
        .collect();

    if exprs.len() == 1 {
        return exprs.remove(0);
    }

    // Chain with AND.
    let mut result = exprs.remove(0);
    for expr in exprs {
        result = Expr::synthetic(ExprKind::BinaryOp {
            left: Box::new(result),
            op: BinOp::And,
            right: Box::new(expr),
        });
    }
    result
}

/// Extract the alias from the last operator in the chain.
pub(in crate::cypher::planner) fn get_last_alias(op: &Option<LogicalOp>) -> String {
    match op {
        Some(LogicalOp::Scan { alias, .. }) => alias.clone(),
        Some(LogicalOp::IndexLookup { alias, .. }) => alias.clone(),
        Some(LogicalOp::Expand { dst_alias, .. }) => dst_alias.clone(),
        Some(LogicalOp::Filter { input, .. }) => get_last_alias(&Some(*input.clone())),
        _ => "_unknown".to_string(),
    }
}

/// Returns true if the expression is an aggregate function call (count, sum, avg, etc.).
pub(in crate::cypher::planner) fn is_aggregate_fn(expr: &Expr) -> bool {
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
                true
            } else {
                // Check if any argument contains an aggregate (e.g. size(collect(a))).
                args.iter().any(is_aggregate_fn)
            }
        }
        // Recursively check sub-expressions (e.g. `count(a) > 0`).
        ExprKind::BinaryOp { left, right, .. } => is_aggregate_fn(left) || is_aggregate_fn(right),
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            is_aggregate_fn(inner)
        }
        ExprKind::MapLiteral(pairs) => pairs.iter().any(|(_, v)| is_aggregate_fn(v)),
        ExprKind::List(items) => items.iter().any(is_aggregate_fn),
        ExprKind::ListComprehension { list_expr, .. } => is_aggregate_fn(list_expr),
        ExprKind::Quantifier { list_expr, .. } => is_aggregate_fn(list_expr),
        _ => false,
    }
}

/// Collect all aggregate function call sub-expressions from an expression tree.
/// Stops recursing into aggregate function arguments (aggregates don't nest).
pub(in crate::cypher::planner) fn collect_aggregate_calls<'a>(
    expr: &'a Expr,
    out: &mut Vec<&'a Expr>,
) {
    match &expr.kind {
        ExprKind::FunctionCall { name, .. }
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
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                collect_aggregate_calls(arg, out);
            }
        }
        ExprKind::BinaryOp { left, right, .. } => {
            collect_aggregate_calls(left, out);
            collect_aggregate_calls(right, out);
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            collect_aggregate_calls(inner, out);
        }
        ExprKind::MapLiteral(pairs) => {
            for (_, v) in pairs {
                collect_aggregate_calls(v, out);
            }
        }
        ExprKind::List(items) => {
            for item in items {
                collect_aggregate_calls(item, out);
            }
        }
        ExprKind::ListComprehension { list_expr, .. } => {
            collect_aggregate_calls(list_expr, out);
        }
        ExprKind::Quantifier {
            list_expr,
            predicate,
            ..
        } => {
            collect_aggregate_calls(list_expr, out);
            collect_aggregate_calls(predicate, out);
        }
        _ => {}
    }
}

/// Check if an expression is a "pure" aggregate — a direct aggregate function call,
/// not a mix like `x + count(y)`.
pub(in crate::cypher::planner) fn is_pure_aggregate(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::FunctionCall { name, .. } => parse_agg_name(name).is_some(),
        _ => false,
    }
}

/// Split RETURN/WITH items into group keys (non-aggregate) and aggregate expressions.
pub(in crate::cypher::planner) fn split_aggregates(
    items: &[ReturnItem],
) -> crate::types::Result<(Vec<Expr>, Vec<AggregateExpr>)> {
    let mut group_keys = Vec::new();
    let mut aggregates = Vec::new();
    let mut mixed_items: Vec<&Expr> = Vec::new();

    for item in items {
        if let ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } = &item.expr.kind
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
                // Reject non-deterministic functions inside aggregates.
                for arg in args {
                    if contains_nondeterministic_fn(arg) {
                        return Err(GraphError::type_error(
                            crate::types::QueryPhase::SemanticAnalysis,
                            "non-deterministic function inside aggregate".to_string(),
                        )
                        .with_code(ErrorCode::NonConstantExpression));
                    }
                }
                let input = args
                    .first()
                    .cloned()
                    .unwrap_or(Expr::synthetic(ExprKind::Star));
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
                    "expression mixes aggregate and non-aggregate sub-expressions".to_string(),
                )
                .with_code(ErrorCode::AmbiguousAggregationExpression));
            }
        }
    }

    Ok((group_keys, aggregates))
}

/// Check if an expression contains a non-deterministic function call (e.g. rand()).
pub(in crate::cypher::planner) fn contains_nondeterministic_fn(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::FunctionCall { name, args, .. } => {
            if name.eq_ignore_ascii_case("rand") {
                return true;
            }
            args.iter().any(contains_nondeterministic_fn)
        }
        ExprKind::BinaryOp { left, right, .. } => {
            contains_nondeterministic_fn(left) || contains_nondeterministic_fn(right)
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            contains_nondeterministic_fn(inner)
        }
        _ => false,
    }
}

/// Collect non-aggregate, non-constant leaf expressions from a mixed expression.
pub(in crate::cypher::planner) fn collect_non_aggregate_leaves<'a>(
    expr: &'a Expr,
    leaves: &mut Vec<&'a Expr>,
) {
    match &expr.kind {
        ExprKind::FunctionCall { name, .. } if parse_agg_name(name).is_some() => {
            // Aggregate function — skip entirely (its args are aggregated)
        }
        ExprKind::BinaryOp { left, right, .. } => {
            collect_non_aggregate_leaves(left, leaves);
            collect_non_aggregate_leaves(right, leaves);
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            collect_non_aggregate_leaves(inner, leaves);
        }
        // Constants and parameters don't need grouping — the param's value
        // is fixed for the query, same as a literal post-resolve.
        ExprKind::Literal(_) | ExprKind::Parameter(_) | ExprKind::Star => {}
        // Non-aggregate functions are fine if their args are constants/grouped.
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                collect_non_aggregate_leaves(arg, leaves);
            }
        }
        ExprKind::MapLiteral(pairs) => {
            for (_, v) in pairs {
                collect_non_aggregate_leaves(v, leaves);
            }
        }
        ExprKind::List(items) => {
            for item in items {
                collect_non_aggregate_leaves(item, leaves);
            }
        }
        ExprKind::ListComprehension { list_expr, .. } => {
            collect_non_aggregate_leaves(list_expr, leaves);
        }
        ExprKind::Quantifier { list_expr, .. } => {
            collect_non_aggregate_leaves(list_expr, leaves);
        }
        _ => {
            // Variable reference, property access, etc. — needs grouping.
            leaves.push(expr);
        }
    }
}

/// Parse aggregate function name to enum.
pub(in crate::cypher::planner) fn parse_agg_name(name: &str) -> Option<AggregateFunction> {
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
pub(in crate::cypher::planner) fn extract_nested_aggregates(
    expr: &Expr,
    aggregates: &mut Vec<AggregateExpr>,
) {
    match &expr.kind {
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => {
            if let Some(function) = parse_agg_name(name) {
                let input = args
                    .first()
                    .cloned()
                    .unwrap_or(Expr::synthetic(ExprKind::Star));
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
        ExprKind::BinaryOp { left, right, .. } => {
            extract_nested_aggregates(left, aggregates);
            extract_nested_aggregates(right, aggregates);
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            extract_nested_aggregates(inner, aggregates);
        }
        ExprKind::MapLiteral(pairs) => {
            for (_, v) in pairs {
                extract_nested_aggregates(v, aggregates);
            }
        }
        ExprKind::List(items) => {
            for item in items {
                extract_nested_aggregates(item, aggregates);
            }
        }
        ExprKind::ListComprehension { list_expr, .. } => {
            extract_nested_aggregates(list_expr, aggregates);
        }
        ExprKind::Quantifier { list_expr, .. } => {
            extract_nested_aggregates(list_expr, aggregates);
        }
        _ => {}
    }
}

// ── Predicate pushdown helpers ──────────────────────────────────────────

/// Flatten a predicate into AND-connected conjuncts.
pub(in crate::cypher::planner) fn decompose_conjuncts(expr: &Expr) -> Vec<Expr> {
    match &expr.kind {
        ExprKind::BinaryOp {
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
pub(in crate::cypher::planner) fn rebuild_conjunction(conjuncts: Vec<Expr>) -> Option<Expr> {
    conjuncts.into_iter().reduce(|acc, c| {
        Expr::synthetic(ExprKind::BinaryOp {
            left: Box::new(acc),
            op: BinOp::And,
            right: Box::new(c),
        })
    })
}

/// Try to push a single equality predicate (`alias.prop = literal`) into an
/// existing Scan node in the plan tree, converting it to an IndexLookup.
///
/// Returns `Some(modified_op)` if the predicate was pushed, `None` if it
/// cannot be pushed (no matching scan, no index, non-eligible predicate).
pub(in crate::cypher::planner) fn try_push_predicate(
    conn: &Connection,
    op: &mut LogicalOp,
    predicate: &Expr,
) -> Option<LogicalOp> {
    // Handle: Property(alias, prop) = Literal(val)  OR  Property = Parameter
    let (alias, prop, key) = match &predicate.kind {
        ExprKind::BinaryOp {
            left,
            op: BinOp::Eq,
            right,
        } => match (&left.as_ref().kind, &right.as_ref().kind) {
            (ExprKind::Property(a, p), ExprKind::Literal(l)) => {
                (a.clone(), p.clone(), LookupKey::Literal(l.clone()))
            }
            (ExprKind::Literal(l), ExprKind::Property(a, p)) => {
                (a.clone(), p.clone(), LookupKey::Literal(l.clone()))
            }
            (ExprKind::Property(a, p), ExprKind::Parameter(n)) => {
                (a.clone(), p.clone(), LookupKey::Param(n.clone()))
            }
            (ExprKind::Parameter(n), ExprKind::Property(a, p)) => {
                (a.clone(), p.clone(), LookupKey::Param(n.clone()))
            }
            _ => return None,
        },
        _ => return None,
    };

    // Check if there's an index for this label+property.
    // Walk the plan tree to find the Scan for this alias.
    try_replace_scan(conn, op, &alias, &prop, &key)
}

/// Recursively search the plan tree for a `Scan` with the given alias and
/// replace it with an `IndexLookup` if an index exists. Returns the new
/// root op if a replacement was made.
pub(in crate::cypher::planner) fn try_replace_scan(
    conn: &Connection,
    op: &mut LogicalOp,
    alias: &str,
    prop: &str,
    lit: &LookupKey,
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
            result_cap,
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
            result_cap: *result_cap,
        }),

        LogicalOp::CrossProduct {
            left,
            right,
            same_match,
        } => {
            if let Some(new_left) = try_replace_scan(conn, left, alias, prop, lit) {
                Some(LogicalOp::CrossProduct {
                    left: Box::new(new_left),
                    right: right.clone(),
                    same_match: *same_match,
                })
            } else {
                try_replace_scan(conn, right, alias, prop, lit).map(|new_right| {
                    LogicalOp::CrossProduct {
                        left: left.clone(),
                        right: Box::new(new_right),
                        same_match: *same_match,
                    }
                })
            }
        }

        _ => None,
    }
}

/// Extract the column names from a statement's RETURN clause for UNION validation.
pub(in crate::cypher::planner) fn return_items_columns(items: &[ReturnItem]) -> Vec<String> {
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

pub(in crate::cypher::planner) fn statement_return_columns(stmt: &Statement) -> Vec<String> {
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
