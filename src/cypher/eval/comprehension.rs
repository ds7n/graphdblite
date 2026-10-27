//! List, pattern, and quantifier comprehensions.

use crate::cypher::ast::*;
use crate::cypher::record::NamedRecord;
use crate::cypher::record_view::{view_to_named, RecordView};
use crate::types::{GraphError, Value};

use super::*;

/// Materialize the outer scope for a comprehension or quantifier that binds
/// `variable`, dropping any outer flat keys `variable.*`.
///
/// An outer node or relationship named like the local variable is stored as
/// flat keys (`x.name`, `x.__id`, ...). Property access resolves those keys
/// before the variable itself, so leaving them in place would make `x.name`
/// read the outer entity instead of the per-item value.
fn scope_without(record: &dyn RecordView, variable: &str) -> NamedRecord {
    let mut base = view_to_named(record);
    let prefix = format!("{variable}.");
    base.fields.retain(|k, _| !k.starts_with(&prefix));
    base
}

pub(in crate::cypher::eval) fn eval_list_comprehension(
    variable: &str,
    list_expr: &Expr,
    filter: Option<&Expr>,
    map_expr: Option<&Expr>,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<Value> {
    let list_val = eval_expr(list_expr, record, ecx)?;
    let items = match list_val {
        Value::List(items) => items,
        // Like every other list operation, a null list yields null.
        Value::Null => return Ok(Value::Null),
        _ => {
            return Err(GraphError::semantic(
                "list comprehension requires a list input".to_string(),
            ))
        }
    };

    // Comprehension binds a fresh per-item variable on top of the outer
    // scope, so we need an owned mutable record. Materialize once.
    let base = scope_without(record, variable);
    let mut results = Vec::new();
    let mut budget = ListBudget::default();
    for item in items {
        let mut local = base.clone();
        local.set(variable.to_string(), item.clone());

        if let Some(pred) = filter {
            if !eval_predicate(pred, &local, ecx)? {
                continue;
            }
        }

        let val = match map_expr {
            Some(expr) => eval_expr(expr, &local, ecx)?,
            None => item,
        };
        budget.add(&val)?;
        results.push(val);
    }

    Ok(Value::List(results))
}

/// Evaluate a quantifier predicate: none/single/any/all(x IN list WHERE pred).
///
/// Uses three-valued logic (true/false/null) per Cypher semantics.
pub(in crate::cypher::eval) fn eval_quantifier(
    kind: QuantifierKind,
    variable: &str,
    list_expr: &Expr,
    predicate: &Expr,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<Value> {
    let list_val = eval_expr(list_expr, record, ecx)?;
    let items = match list_val {
        Value::List(items) => items,
        Value::Null => return Ok(Value::Null),
        _ => {
            return Err(GraphError::semantic(
                "quantifier requires a list input".to_string(),
            ))
        }
    };

    let mut true_count: usize = 0;
    let mut false_count: usize = 0;
    let mut null_count: usize = 0;

    let base = scope_without(record, variable);
    for item in &items {
        let mut local = base.clone();
        local.set(variable.to_string(), item.clone());
        let val = eval_expr(predicate, &local, ecx)?;
        match val {
            Value::Bool(true) => true_count += 1,
            Value::Bool(false) => false_count += 1,
            _ => null_count += 1,
        }
    }

    match kind {
        QuantifierKind::All => {
            if false_count > 0 {
                Ok(Value::Bool(false))
            } else if null_count > 0 {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(true))
            }
        }
        QuantifierKind::Any => {
            if true_count > 0 {
                Ok(Value::Bool(true))
            } else if null_count > 0 {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(false))
            }
        }
        QuantifierKind::None => {
            if true_count > 0 {
                Ok(Value::Bool(false))
            } else if null_count > 0 {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(true))
            }
        }
        QuantifierKind::Single => {
            if true_count == 1 && null_count == 0 {
                Ok(Value::Bool(true))
            } else if null_count > 0 && true_count <= 1 {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(false))
            }
        }
    }
}

/// Evaluate a pattern comprehension: [(p = )? pattern (WHERE pred)? | expr].
///
/// Plans and executes the pattern as a correlated subquery, evaluating the
/// map expression for each matching row and collecting results into a list.
pub(in crate::cypher::eval) fn eval_pattern_comprehension(
    path_variable: Option<&str>,
    pattern: &crate::cypher::ast::Pattern,
    where_clause: Option<&Expr>,
    map_expr: &Expr,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<Value> {
    use crate::cypher::executor::exec_correlated_subquery;
    use crate::cypher::ir::LogicalOp;
    use crate::cypher::planner::plan_patterns;

    let conn = ecx.conn;

    // If a path variable is requested, set it on the pattern so the planner
    // emits a MaterializePath operator.
    let mut pat = pattern.clone();
    if let Some(pv) = path_variable {
        pat.path_variable = Some(pv.to_string());
    }

    // Plan the pattern into a scan/expand chain.
    let mut op = plan_patterns(conn, std::slice::from_ref(&pat))?;

    // Apply the optional WHERE filter.
    if let Some(predicate) = where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    // Build a clean outer record for correlated execution.
    // 1. Only include bindings that are relevant (named variables, not internal
    //    _anon_* or _path_rel_* aliases) to avoid alias collisions.
    // 2. Flatten Node values into the internal record format (alias -> I64(id),
    //    alias.__id, alias.__label, etc.) so exec_correlated can bind them.
    let outer_base = view_to_named(record);
    let mut outer_rec = NamedRecord::new();
    for (k, v) in &outer_base.fields {
        // Skip internal anonymous aliases from outer scopes.
        if k.starts_with("_anon_") || k.starts_with("_path_rel_") {
            continue;
        }
        match v {
            Value::Node(n) => {
                outer_rec.set(k.clone(), Value::I64(n.id.0 as i64));
                outer_rec.set(format!("{k}.__id"), Value::I64(n.id.0 as i64));
                outer_rec.set(format!("{k}.__label"), Value::String(n.labels.join(":")));
                outer_rec.set(
                    format!("{k}.__labels"),
                    Value::List(n.labels.iter().map(|l| Value::String(l.clone())).collect()),
                );
                for (pk, pv) in &n.properties {
                    outer_rec.set(format!("{k}.{pk}"), pv.clone());
                }
            }
            _ => {
                // Skip internal metadata keys from anonymous aliases.
                let base = k.split('.').next().unwrap_or(k);
                if base.starts_with("_anon_") || base.starts_with("_path_rel_") {
                    continue;
                }
                outer_rec.set(k.clone(), v.clone());
            }
        }
    }

    // Execute the plan as a correlated subquery, passing outer bindings.
    let rows = exec_correlated_subquery(conn, &op, &outer_rec)?;

    // For each matching row, evaluate the map expression.
    let mut results = Vec::new();
    let mut budget = ListBudget::default();
    for row in &rows {
        // Merge the outer record with the inner row so the map expression
        // can reference both outer and inner variables.
        let mut merged = outer_base.clone();
        for (k, v) in &row.fields {
            merged.set(k.clone(), v.clone());
        }

        let val = eval_expr(map_expr, &merged, ecx)?;
        budget.add(&val)?;
        results.push(val);
    }

    Ok(Value::List(results))
}
