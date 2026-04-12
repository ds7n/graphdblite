use crate::cypher::ast::{BinOp, Expr, LiteralValue};
use crate::cypher::record::Record;
use crate::types::Value;

/// Evaluate an expression against a record, producing a Value.
pub fn eval_expr(expr: &Expr, record: &Record) -> crate::types::Result<Value> {
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
        Expr::Star => Ok(Value::Null),
        Expr::BinaryOp { left, op, right } => {
            let lval = eval_expr(left, record)?;
            let rval = eval_expr(right, record)?;
            eval_binop(&lval, *op, &rval)
        }
        Expr::Not(inner) => {
            let val = eval_expr(inner, record)?;
            match val {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                _ => Ok(Value::Null),
            }
        }
        Expr::IsNull(inner) => {
            let val = eval_expr(inner, record)?;
            Ok(Value::Bool(matches!(val, Value::Null)))
        }
        Expr::IsNotNull(inner) => {
            let val = eval_expr(inner, record)?;
            Ok(Value::Bool(!matches!(val, Value::Null)))
        }
        Expr::FunctionCall { .. } => {
            // Aggregate functions are handled by the Aggregate operator, not here.
            Ok(Value::Null)
        }
    }
}

/// Evaluate a boolean expression, returning true/false.
pub fn eval_predicate(expr: &Expr, record: &Record) -> crate::types::Result<bool> {
    let val = eval_expr(expr, record)?;
    Ok(matches!(val, Value::Bool(true)))
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
        _ => "_expr".to_string(),
    }
}
