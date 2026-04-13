use rusqlite::Connection;

use crate::cypher::ast::{BinOp, Expr, LiteralValue};
use crate::cypher::record::Record;
use crate::types::Value;

/// Evaluate an expression against a record, producing a Value.
///
/// The `conn` parameter is needed for EXISTS subquery evaluation.
pub fn eval_expr(expr: &Expr, record: &Record, conn: &Connection) -> crate::types::Result<Value> {
    match expr {
        Expr::Literal(lit) => Ok(literal_to_value(lit)),
        Expr::Variable(name) => Ok(record
            .get(name)
            .cloned()
            .unwrap_or(Value::Null)),
        Expr::Property(var, prop) => {
            // Look up "var.prop" as a flattened key in the record.
            let key = format!("{var}.{prop}");
            Ok(record.get(&key).cloned().unwrap_or(Value::Null))
        }
        Expr::List(items) => {
            let values: crate::types::Result<Vec<Value>> =
                items.iter().map(|e| eval_expr(e, record, conn)).collect();
            Ok(Value::List(values?))
        }
        Expr::Star => Ok(Value::Null),
        Expr::BinaryOp { left, op, right } => {
            let lval = eval_expr(left, record, conn)?;
            let rval = eval_expr(right, record, conn)?;
            eval_binop(&lval, *op, &rval)
        }
        Expr::Not(inner) => {
            let val = eval_expr(inner, record, conn)?;
            match val {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                _ => Ok(Value::Null),
            }
        }
        Expr::IsNull(inner) => {
            let val = eval_expr(inner, record, conn)?;
            Ok(Value::Bool(matches!(val, Value::Null)))
        }
        Expr::IsNotNull(inner) => {
            let val = eval_expr(inner, record, conn)?;
            Ok(Value::Bool(!matches!(val, Value::Null)))
        }
        Expr::Case { alternatives, default } => {
            for (cond, result) in alternatives {
                if eval_predicate(cond, record, conn)? {
                    return eval_expr(result, record, conn);
                }
            }
            match default {
                Some(expr) => eval_expr(expr, record, conn),
                None => Ok(Value::Null),
            }
        }
        Expr::ListComprehension { variable, list_expr, filter, map_expr } => {
            eval_list_comprehension(variable, list_expr, filter.as_deref(), map_expr.as_deref(), record, conn)
        }
        Expr::Exists { patterns, where_clause } => {
            eval_exists(patterns, where_clause.as_deref(), record, conn)
        }
        Expr::FunctionCall { name, args } => {
            eval_function_call(name, args, record, conn)
        }
    }
}

/// Evaluate a boolean expression, returning true/false.
pub fn eval_predicate(expr: &Expr, record: &Record, conn: &Connection) -> crate::types::Result<bool> {
    let val = eval_expr(expr, record, conn)?;
    Ok(matches!(val, Value::Bool(true)))
}

/// Evaluate a function call.
///
/// Aggregate functions (count, sum, avg, etc.) are handled by the Aggregate
/// operator, not here. Scalar functions like length() and nodes() are evaluated inline.
fn eval_function_call(
    name: &str,
    args: &[Expr],
    record: &Record,
    conn: &Connection,
) -> crate::types::Result<Value> {
    match name.as_ref() {
        "length" => {
            let arg = args.first().map(|a| eval_expr(a, record, conn)).transpose()?;
            match arg {
                Some(Value::Path(nodes)) => Ok(Value::I64(nodes.len().saturating_sub(1) as i64)),
                Some(Value::String(s)) => Ok(Value::I64(s.len() as i64)),
                Some(Value::List(items)) => Ok(Value::I64(items.len() as i64)),
                _ => Ok(Value::Null),
            }
        }
        "nodes" => {
            let arg = args.first().map(|a| eval_expr(a, record, conn)).transpose()?;
            match arg {
                Some(Value::Path(node_ids)) => {
                    let list = node_ids.iter().map(|id| Value::I64(id.0 as i64)).collect();
                    Ok(Value::List(list))
                }
                _ => Ok(Value::Null),
            }
        }
        // Aggregate functions are handled by the Aggregate operator.
        _ => Ok(Value::Null),
    }
}

/// Evaluate a list comprehension: [x IN list WHERE pred | expr].
fn eval_list_comprehension(
    variable: &str,
    list_expr: &Expr,
    filter: Option<&Expr>,
    map_expr: Option<&Expr>,
    record: &Record,
    conn: &Connection,
) -> crate::types::Result<Value> {
    let list_val = eval_expr(list_expr, record, conn)?;
    let items = match list_val {
        Value::List(items) => items,
        Value::Null => return Ok(Value::List(vec![])),
        _ => {
            return Err(crate::types::GraphError::Serialization(
                "list comprehension requires a list input".to_string(),
            ))
        }
    };

    let mut results = Vec::new();
    for item in items {
        let mut local = record.clone();
        local.set(variable.to_string(), item.clone());

        if let Some(pred) = filter {
            if !eval_predicate(pred, &local, conn)? {
                continue;
            }
        }

        let val = match map_expr {
            Some(expr) => eval_expr(expr, &local, conn)?,
            None => item,
        };
        results.push(val);
    }

    Ok(Value::List(results))
}

/// Evaluate an EXISTS { pattern [WHERE expr] } subquery.
///
/// Plans and executes the subquery patterns against the current record's
/// bindings. Returns true if any row matches, false otherwise.
fn eval_exists(
    patterns: &[crate::cypher::ast::Pattern],
    where_clause: Option<&Expr>,
    record: &Record,
    conn: &Connection,
) -> crate::types::Result<Value> {
    use crate::cypher::executor::execute_first_match;
    use crate::cypher::ir::LogicalOp;
    use crate::cypher::planner::plan_patterns;

    // Plan the subquery patterns into a scan/expand chain.
    let mut op = plan_patterns(conn, patterns)?;

    // Apply the optional WHERE filter from within the EXISTS block.
    if let Some(predicate) = where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    // Extract correlated bindings from the outer record (bare alias keys, no dots).
    let outer_bindings: Vec<(String, Value)> = record
        .fields
        .iter()
        .filter(|(k, _)| !k.contains('.'))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Short-circuit: return true on the first matching row.
    let found = execute_first_match(conn, &op, &outer_bindings)?;
    Ok(Value::Bool(found))
}

fn eval_binop(left: &Value, op: BinOp, right: &Value) -> crate::types::Result<Value> {
    match op {
        // Three-valued AND: NULL AND false → false, NULL AND true → NULL
        BinOp::And => {
            match (to_tribool(left), to_tribool(right)) {
                (Some(false), _) | (_, Some(false)) => Ok(Value::Bool(false)),
                (Some(true), Some(true)) => Ok(Value::Bool(true)),
                _ => Ok(Value::Null), // at least one NULL, none false
            }
        }
        // Three-valued OR: NULL OR true → true, NULL OR false → NULL
        BinOp::Or => {
            match (to_tribool(left), to_tribool(right)) {
                (Some(true), _) | (_, Some(true)) => Ok(Value::Bool(true)),
                (Some(false), Some(false)) => Ok(Value::Bool(false)),
                _ => Ok(Value::Null), // at least one NULL, none true
            }
        }
        BinOp::Eq => Ok(values_equal(left, right)),
        BinOp::Neq => match values_equal(left, right) {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            other => Ok(other), // propagate Null
        },
        BinOp::Lt => Ok(compare_to_value(left, right, |o| o == std::cmp::Ordering::Less)),
        BinOp::Gt => Ok(compare_to_value(left, right, |o| o == std::cmp::Ordering::Greater)),
        BinOp::Lte => Ok(compare_to_value(left, right, |o| matches!(o, std::cmp::Ordering::Less | std::cmp::Ordering::Equal))),
        BinOp::Gte => Ok(compare_to_value(left, right, |o| matches!(o, std::cmp::Ordering::Greater | std::cmp::Ordering::Equal))),
        BinOp::StartsWith => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::String(l), Value::String(r)) => Ok(Value::Bool(l.starts_with(r.as_str()))),
            _ => Ok(Value::Null),
        },
        BinOp::EndsWith => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::String(l), Value::String(r)) => Ok(Value::Bool(l.ends_with(r.as_str()))),
            _ => Ok(Value::Null),
        },
        BinOp::Contains => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::String(l), Value::String(r)) => Ok(Value::Bool(l.contains(r.as_str()))),
            _ => Ok(Value::Null),
        },
    }
}

/// Convert a Value to a three-valued boolean: Some(true), Some(false), or None (null).
fn to_tribool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Null => None,
        _ => Some(false), // non-boolean non-null → falsy
    }
}

/// Three-valued equality: returns Null if either operand is null.
fn values_equal(a: &Value, b: &Value) -> Value {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    let eq = match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::I64(a), Value::I64(b)) => a == b,
        (Value::F64(a), Value::F64(b)) => a == b,
        (Value::I64(a), Value::F64(b)) => (*a as f64) == *b,
        (Value::F64(a), Value::I64(b)) => *a == (*b as f64),
        (Value::String(a), Value::String(b)) => a == b,
        (Value::List(a), Value::List(b)) => a == b,
        _ => false,
    };
    Value::Bool(eq)
}

/// Compare two values, returning Null if either is null or types are incomparable.
fn compare_to_value(a: &Value, b: &Value, pred: impl Fn(std::cmp::Ordering) -> bool) -> Value {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    match compare_values(a, b) {
        Some(ord) => Value::Bool(pred(ord)),
        None => Value::Null,
    }
}

fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::I64(a), Value::I64(b)) => Some(a.cmp(b)),
        (Value::F64(a), Value::F64(b)) => a.partial_cmp(b),
        (Value::I64(a), Value::F64(b)) => (*a as f64).partial_cmp(b),
        (Value::F64(a), Value::I64(b)) => a.partial_cmp(&(*b as f64)),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

fn literal_to_value(lit: &LiteralValue) -> Value {
    match lit {
        LiteralValue::Null => Value::Null,
        LiteralValue::Bool(b) => Value::Bool(*b),
        LiteralValue::I64(n) => Value::I64(*n),
        LiteralValue::F64(n) => Value::F64(*n),
        LiteralValue::String(s) => Value::String(s.clone()),
    }
}

/// Resolve an expression to a column name for RETURN projections.
pub fn expr_to_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(var, prop) => format!("{var}.{prop}"),
        Expr::FunctionCall { name, args } => {
            if args.is_empty() || matches!(args[0], Expr::Star) {
                format!("{name}(*)")
            } else {
                format!("{name}({})", expr_to_column_name(&args[0]))
            }
        }
        Expr::Star => "*".to_string(),
        Expr::Literal(lit) => format!("{lit:?}"),
        Expr::Case { .. } => "CASE".to_string(),
        Expr::List(_) => "list".to_string(),
        _ => "_expr".to_string(),
    }
}
