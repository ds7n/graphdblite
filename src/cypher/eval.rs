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
        Expr::FunctionCall { .. } => {
            // Aggregate functions are handled by the Aggregate operator, not here.
            Ok(Value::Null)
        }
    }
}

/// Evaluate a boolean expression, returning true/false.
pub fn eval_predicate(expr: &Expr, record: &Record, conn: &Connection) -> crate::types::Result<bool> {
    let val = eval_expr(expr, record, conn)?;
    Ok(matches!(val, Value::Bool(true)))
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
    use crate::cypher::executor::execute;
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

    // Execute the subquery.
    let results = execute(conn, &op)?;

    // Check if any result matches the outer record's correlated bindings.
    // Correlated variables: if the outer record binds a variable (bare alias key,
    // no dots) and the subquery also produces that variable, the values must match.
    let outer_bindings: Vec<(String, Value)> = record
        .fields
        .iter()
        .filter(|(k, _)| !k.contains('.'))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    for sub_rec in &results {
        let matches = outer_bindings.iter().all(|(key, outer_val)| {
            match sub_rec.get(key) {
                Some(inner_val) => inner_val == outer_val,
                None => true, // subquery doesn't produce this variable — no constraint
            }
        });
        if matches {
            return Ok(Value::Bool(true));
        }
    }

    Ok(Value::Bool(false))
}

fn eval_binop(left: &Value, op: BinOp, right: &Value) -> crate::types::Result<Value> {
    match op {
        BinOp::And => {
            let l = matches!(left, Value::Bool(true));
            let r = matches!(right, Value::Bool(true));
            Ok(Value::Bool(l && r))
        }
        BinOp::Or => {
            let l = matches!(left, Value::Bool(true));
            let r = matches!(right, Value::Bool(true));
            Ok(Value::Bool(l || r))
        }
        BinOp::Eq => Ok(Value::Bool(values_equal(left, right))),
        BinOp::Neq => Ok(Value::Bool(!values_equal(left, right))),
        BinOp::Lt => Ok(Value::Bool(compare_values(left, right) == Some(std::cmp::Ordering::Less))),
        BinOp::Gt => Ok(Value::Bool(compare_values(left, right) == Some(std::cmp::Ordering::Greater))),
        BinOp::Lte => Ok(Value::Bool(matches!(
            compare_values(left, right),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ))),
        BinOp::Gte => Ok(Value::Bool(matches!(
            compare_values(left, right),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ))),
        BinOp::StartsWith => match (left, right) {
            (Value::String(l), Value::String(r)) => Ok(Value::Bool(l.starts_with(r.as_str()))),
            _ => Ok(Value::Null),
        },
        BinOp::EndsWith => match (left, right) {
            (Value::String(l), Value::String(r)) => Ok(Value::Bool(l.ends_with(r.as_str()))),
            _ => Ok(Value::Null),
        },
        BinOp::Contains => match (left, right) {
            (Value::String(l), Value::String(r)) => Ok(Value::Bool(l.contains(r.as_str()))),
            _ => Ok(Value::Null),
        },
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::I64(a), Value::I64(b)) => a == b,
        (Value::F64(a), Value::F64(b)) => a == b,
        (Value::I64(a), Value::F64(b)) => (*a as f64) == *b,
        (Value::F64(a), Value::I64(b)) => *a == (*b as f64),
        (Value::String(a), Value::String(b)) => a == b,
        (Value::List(a), Value::List(b)) => a == b,
        _ => false,
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
