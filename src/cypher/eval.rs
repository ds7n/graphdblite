use rusqlite::Connection;

use crate::cypher::ast::{BinOp, Expr, LiteralValue, QuantifierKind};
use crate::cypher::record::Record;
use crate::types::Value;

/// Evaluate an expression against a record, producing a Value.
///
/// The `conn` parameter is needed for EXISTS subquery evaluation.
pub fn eval_expr(expr: &Expr, record: &Record, conn: &Connection) -> crate::types::Result<Value> {
    match expr {
        Expr::Literal(lit) => Ok(literal_to_value(lit)),
        Expr::Variable(name) => Ok(record.get(name).cloned().unwrap_or(Value::Null)),
        Expr::Property(var, prop) => {
            // Look up "var.prop" as a flattened key in the record.
            let key = format!("{var}.{prop}");
            if let Some(val) = record.get(&key) {
                return Ok(val.clone());
            }
            // Fallback: if the record has var.__id (e.g. from CREATE/MERGE),
            // look up the property from the database.
            let id_key = format!("{var}.__id");
            if let Some(Value::I64(id)) = record.get(&id_key) {
                if let Ok(node) = crate::node::get_node(conn, crate::types::NodeId(*id as u64)) {
                    if prop == "labels" {
                        return Ok(Value::List(
                            node.labels.into_iter().map(Value::String).collect(),
                        ));
                    }
                    return Ok(node.properties.get(prop).cloned().unwrap_or(Value::Null));
                }
            }
            // Temporal component accessor: d.year, d.month, etc.
            if let Some(val) = record.get(var) {
                if let Some(result) = temporal_accessor(val, prop) {
                    return Ok(result);
                }
            }
            Ok(Value::Null)
        }
        Expr::List(items) => {
            let values: crate::types::Result<Vec<Value>> =
                items.iter().map(|e| eval_expr(e, record, conn)).collect();
            Ok(Value::List(values?))
        }
        Expr::Index { expr, index } => {
            let base = eval_expr(expr, record, conn)?;
            let idx = eval_expr(index, record, conn)?;
            match (base, idx) {
                (Value::List(items), Value::I64(i)) => {
                    let len = items.len() as i64;
                    let resolved = if i < 0 { len + i } else { i };
                    if resolved >= 0 && (resolved as usize) < items.len() {
                        Ok(items.into_iter().nth(resolved as usize).unwrap())
                    } else {
                        Ok(Value::Null)
                    }
                }
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        Expr::Slice { expr, start, end } => {
            let base = eval_expr(expr, record, conn)?;
            let start_val = start
                .as_ref()
                .map(|e| eval_expr(e, record, conn))
                .transpose()?;
            let end_val = end
                .as_ref()
                .map(|e| eval_expr(e, record, conn))
                .transpose()?;
            match base {
                Value::List(items) => {
                    let len = items.len() as i64;
                    let resolve = |v: i64| {
                        let r = if v < 0 { len + v } else { v };
                        r.clamp(0, len) as usize
                    };
                    let s = match &start_val {
                        Some(Value::I64(i)) => resolve(*i),
                        _ => 0,
                    };
                    let e = match &end_val {
                        Some(Value::I64(i)) => resolve(*i),
                        _ => len as usize,
                    };
                    if s >= e {
                        Ok(Value::List(vec![]))
                    } else {
                        Ok(Value::List(items[s..e].to_vec()))
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        Expr::Star => Ok(Value::Null),
        Expr::Parameter(name) => Err(crate::types::GraphError::argument(
            crate::types::QueryPhase::Runtime,
            format!("unresolved parameter: ${name}"),
        )),
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
        Expr::Case {
            alternatives,
            default,
        } => {
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
        Expr::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => eval_list_comprehension(
            variable,
            list_expr,
            filter.as_deref(),
            map_expr.as_deref(),
            record,
            conn,
        ),
        Expr::Quantifier {
            kind,
            variable,
            list_expr,
            predicate,
        } => eval_quantifier(*kind, variable, list_expr, predicate, record, conn),
        Expr::Exists {
            patterns,
            where_clause,
        } => eval_exists(patterns, where_clause.as_deref(), record, conn),
        Expr::MapLiteral(pairs) => {
            let mut map = std::collections::BTreeMap::new();
            for (k, expr) in pairs {
                let val = eval_expr(expr, record, conn)?;
                map.insert(k.clone(), val);
            }
            Ok(Value::Map(map))
        }
        Expr::FunctionCall { name, args } => eval_function_call(name, args, record, conn),
    }
}

/// Evaluate a boolean expression, returning true/false.
pub fn eval_predicate(
    expr: &Expr,
    record: &Record,
    conn: &Connection,
) -> crate::types::Result<bool> {
    let val = eval_expr(expr, record, conn)?;
    Ok(matches!(val, Value::Bool(true)))
}

/// Evaluate the first argument of a function call.
fn eval_single_arg(
    args: &[Expr],
    record: &Record,
    conn: &Connection,
) -> crate::types::Result<Value> {
    args.first()
        .map(|a| eval_expr(a, record, conn))
        .transpose()
        .map(|v| v.unwrap_or(Value::Null))
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
    match name {
        "length" => {
            let arg = args
                .first()
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match arg {
                Some(Value::Path(p)) => Ok(Value::I64(p.len() as i64)),
                Some(Value::String(s)) => Ok(Value::I64(s.len() as i64)),
                Some(Value::List(items)) => Ok(Value::I64(items.len() as i64)),
                _ => Ok(Value::Null),
            }
        }
        "nodes" => {
            let arg = args
                .first()
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match arg {
                Some(Value::Path(p)) => {
                    let list = p.nodes.into_iter().map(Value::Node).collect();
                    Ok(Value::List(list))
                }
                _ => Ok(Value::Null),
            }
        }
        "tolower" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.to_lowercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "toupper" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.to_uppercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "tostring" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Null => Ok(Value::Null),
                Value::String(s) => Ok(Value::String(s)),
                other => Ok(Value::String(other.to_string())),
            }
        }
        "tointeger" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(_) => Ok(arg),
                Value::F64(f) => Ok(Value::I64(f as i64)),
                Value::String(s) => Ok(s.parse::<i64>().map(Value::I64).unwrap_or(Value::Null)),
                Value::Bool(b) => Ok(Value::I64(if b { 1 } else { 0 })),
                _ => Ok(Value::Null),
            }
        }
        "tofloat" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::F64(_) => Ok(arg),
                Value::I64(n) => Ok(Value::F64(n as f64)),
                Value::String(s) => Ok(s.parse::<f64>().map(Value::F64).unwrap_or(Value::Null)),
                _ => Ok(Value::Null),
            }
        }
        "keys" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(id) => {
                    // keys(node_id) — return property keys for the node.
                    let node = crate::node::get_node(conn, crate::types::NodeId(id as u64))?;
                    let mut keys: Vec<String> = node.properties.keys().cloned().collect();
                    keys.sort();
                    Ok(Value::List(keys.into_iter().map(Value::String).collect()))
                }
                _ => Ok(Value::Null),
            }
        }
        "labels" => {
            // labels(n) — return label list from the __labels record field or database.
            if let Some(Expr::Variable(var)) = args.first() {
                let label_key = format!("{var}.__labels");
                if let Some(val @ Value::List(_)) = record.get(&label_key) {
                    return Ok(val.clone());
                }
                // Fallback: look up from database.
                let id_key = format!("{var}.__id");
                if let Some(Value::I64(id)) = record.get(&id_key).or_else(|| record.get(var)) {
                    if let Ok(node) = crate::node::get_node(conn, crate::types::NodeId(*id as u64))
                    {
                        return Ok(Value::List(
                            node.labels.into_iter().map(Value::String).collect(),
                        ));
                    }
                }
            }
            // Also handle labels(id_value) where the arg evaluates to an integer.
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(id) => {
                    if let Ok(node) = crate::node::get_node(conn, crate::types::NodeId(id as u64)) {
                        Ok(Value::List(
                            node.labels.into_iter().map(Value::String).collect(),
                        ))
                    } else {
                        Ok(Value::Null)
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "id" => {
            // id(n) — extract the __id field from the record binding.
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(_) => Ok(arg),
                _ => Ok(Value::Null),
            }
        }
        "type" => {
            // type(r) — extract the relationship type from a binding.
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::String(_) => Ok(arg),
                _ => Ok(Value::Null),
            }
        }
        "coalesce" => {
            // coalesce(a, b, c, ...) — return first non-null value.
            for a in args {
                let val = eval_expr(a, record, conn)?;
                if val != Value::Null {
                    return Ok(val);
                }
            }
            Ok(Value::Null)
        }
        "head" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::List(items) => Ok(items.into_iter().next().unwrap_or(Value::Null)),
                _ => Ok(Value::Null),
            }
        }
        "last" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::List(items) => Ok(items.into_iter().last().unwrap_or(Value::Null)),
                _ => Ok(Value::Null),
            }
        }
        "tail" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::List(mut items) => {
                    if items.is_empty() {
                        Ok(Value::List(vec![]))
                    } else {
                        items.remove(0);
                        Ok(Value::List(items))
                    }
                }
                _ => Ok(Value::Null),
            }
        }
        "size" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::List(items) => Ok(Value::I64(items.len() as i64)),
                Value::String(s) => Ok(Value::I64(s.len() as i64)),
                _ => Ok(Value::Null),
            }
        }
        "abs" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::I64(n.abs())),
                Value::F64(n) => Ok(Value::F64(n.abs())),
                _ => Ok(Value::Null),
            }
        }
        "substring" => {
            // substring(s, start [, length])
            let s = eval_single_arg(args, record, conn)?;
            let start = args
                .get(1)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            let len = args
                .get(2)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match (s, start) {
                (Value::String(s), Some(Value::I64(start))) => {
                    let start = start.max(0) as usize;
                    if start >= s.len() {
                        return Ok(Value::String(String::new()));
                    }
                    match len {
                        Some(Value::I64(l)) => {
                            let end = (start + l.max(0) as usize).min(s.len());
                            Ok(Value::String(s[start..end].to_string()))
                        }
                        _ => Ok(Value::String(s[start..].to_string())),
                    }
                }
                _ => Ok(Value::Null),
            }
        }
        "replace" => {
            // replace(s, search, replacement)
            let s = eval_single_arg(args, record, conn)?;
            let search = args
                .get(1)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            let replacement = args
                .get(2)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match (s, search, replacement) {
                (Value::String(s), Some(Value::String(search)), Some(Value::String(repl))) => {
                    Ok(Value::String(s.replace(&search, &repl)))
                }
                _ => Ok(Value::Null),
            }
        }
        "split" => {
            // split(s, delimiter)
            let s = eval_single_arg(args, record, conn)?;
            let delim = args
                .get(1)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match (s, delim) {
                (Value::String(s), Some(Value::String(d))) => {
                    let parts: Vec<Value> =
                        s.split(&d).map(|p| Value::String(p.to_string())).collect();
                    Ok(Value::List(parts))
                }
                _ => Ok(Value::Null),
            }
        }
        "trim" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.trim().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "reverse" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.chars().rev().collect())),
                Value::List(mut items) => {
                    items.reverse();
                    Ok(Value::List(items))
                }
                _ => Ok(Value::Null),
            }
        }
        "range" => {
            // range(start, end [, step])
            let start = eval_single_arg(args, record, conn)?;
            let end = args
                .get(1)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            let step = args
                .get(2)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match (start, end) {
                (Value::I64(s), Some(Value::I64(e))) => {
                    let step = match step {
                        Some(Value::I64(st)) if st != 0 => st,
                        _ => 1,
                    };
                    let mut result = Vec::new();
                    let mut i = s;
                    if step > 0 {
                        while i <= e {
                            result.push(Value::I64(i));
                            i += step;
                        }
                    } else {
                        while i >= e {
                            result.push(Value::I64(i));
                            i += step;
                        }
                    }
                    Ok(Value::List(result))
                }
                _ => Ok(Value::Null),
            }
        }
        // Temporal constructor functions.
        "date" => eval_temporal_constructor(
            args,
            record,
            conn,
            |s| crate::temporal::CypherDate::from_iso_string(s).map(Value::Date),
            |m| crate::temporal::CypherDate::from_map(m).map(Value::Date),
        ),
        "localtime" => eval_temporal_constructor(
            args,
            record,
            conn,
            |s| crate::temporal::CypherLocalTime::from_iso_string(s).map(Value::LocalTime),
            |m| crate::temporal::CypherLocalTime::from_map(m).map(Value::LocalTime),
        ),
        "time" => eval_temporal_constructor(
            args,
            record,
            conn,
            |s| crate::temporal::CypherTime::from_iso_string(s).map(Value::Time),
            |m| crate::temporal::CypherTime::from_map(m).map(Value::Time),
        ),
        "localdatetime" => eval_temporal_constructor(
            args,
            record,
            conn,
            |s| crate::temporal::CypherLocalDateTime::from_iso_string(s).map(Value::LocalDateTime),
            |m| crate::temporal::CypherLocalDateTime::from_map(m).map(Value::LocalDateTime),
        ),
        "datetime" => eval_temporal_constructor(
            args,
            record,
            conn,
            |s| crate::temporal::CypherDateTime::from_iso_string(s).map(Value::DateTime),
            |m| crate::temporal::CypherDateTime::from_map(m).map(Value::DateTime),
        ),
        "duration" => eval_temporal_constructor(
            args,
            record,
            conn,
            |s| crate::temporal::CypherDuration::from_iso_string(s).map(Value::Duration),
            |m| crate::temporal::CypherDuration::from_map(m).map(Value::Duration),
        ),
        "datetime.fromepoch" => {
            let secs = eval_single_arg(args, record, conn)?;
            let nanos = args
                .get(1)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match (secs, nanos) {
                (Value::I64(s), Some(Value::I64(n))) => Ok(Value::DateTime(
                    crate::temporal::CypherDateTime::from_epoch(s, n),
                )),
                (Value::I64(s), None) => Ok(Value::DateTime(
                    crate::temporal::CypherDateTime::from_epoch(s, 0),
                )),
                _ => Ok(Value::Null),
            }
        }
        "datetime.fromepochmillis" => {
            let millis = eval_single_arg(args, record, conn)?;
            match millis {
                Value::I64(ms) => Ok(Value::DateTime(
                    crate::temporal::CypherDateTime::from_epoch_millis(ms),
                )),
                _ => Ok(Value::Null),
            }
        }
        // Aggregate functions are handled by the Aggregate operator.
        _ => Ok(Value::Null),
    }
}

/// Helper for temporal constructor dispatch: string arg → parse, map arg → construct.
fn eval_temporal_constructor(
    args: &[Expr],
    record: &Record,
    conn: &Connection,
    from_str: impl Fn(&str) -> crate::types::Result<Value>,
    from_map: impl Fn(&std::collections::BTreeMap<String, Value>) -> crate::types::Result<Value>,
) -> crate::types::Result<Value> {
    let arg = eval_single_arg(args, record, conn)?;
    match arg {
        Value::String(s) => from_str(&s),
        Value::Map(m) => from_map(&m),
        Value::Null => Ok(Value::Null),
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

/// Evaluate a quantifier predicate: none/single/any/all(x IN list WHERE pred).
///
/// Uses three-valued logic (true/false/null) per Cypher semantics.
fn eval_quantifier(
    kind: QuantifierKind,
    variable: &str,
    list_expr: &Expr,
    predicate: &Expr,
    record: &Record,
    conn: &Connection,
) -> crate::types::Result<Value> {
    let list_val = eval_expr(list_expr, record, conn)?;
    let items = match list_val {
        Value::List(items) => items,
        Value::Null => return Ok(Value::Null),
        _ => {
            return Err(crate::types::GraphError::Serialization(
                "quantifier requires a list input".to_string(),
            ))
        }
    };

    let mut true_count: usize = 0;
    let mut false_count: usize = 0;
    let mut null_count: usize = 0;

    for item in &items {
        let mut local = record.clone();
        local.set(variable.to_string(), item.clone());
        let val = eval_expr(predicate, &local, conn)?;
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
        // Three-valued XOR: NULL XOR anything → NULL
        BinOp::Xor => {
            match (to_tribool(left), to_tribool(right)) {
                (Some(a), Some(b)) => Ok(Value::Bool(a ^ b)),
                _ => Ok(Value::Null), // at least one NULL
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
        BinOp::Lt => Ok(compare_to_value(left, right, |o| {
            o == std::cmp::Ordering::Less
        })),
        BinOp::Gt => Ok(compare_to_value(left, right, |o| {
            o == std::cmp::Ordering::Greater
        })),
        BinOp::Lte => Ok(compare_to_value(left, right, |o| {
            matches!(o, std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        })),
        BinOp::Gte => Ok(compare_to_value(left, right, |o| {
            matches!(o, std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        })),
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
        BinOp::In => match (left, right) {
            (_, Value::Null) => Ok(Value::Null),
            (Value::Null, Value::List(items)) => {
                // NULL IN [1, 2] → NULL; NULL IN [] → false
                if items.is_empty() {
                    Ok(Value::Bool(false))
                } else {
                    Ok(Value::Null)
                }
            }
            (val, Value::List(items)) => {
                let mut found = false;
                let mut has_null = false;
                for item in items {
                    if matches!(item, Value::Null) {
                        has_null = true;
                    } else if values_equal(val, item) == Value::Bool(true) {
                        found = true;
                        break;
                    }
                }
                if found {
                    Ok(Value::Bool(true))
                } else if has_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Bool(false))
                }
            }
            _ => Ok(Value::Null),
        },
        BinOp::Add => eval_arithmetic(left, right, |a, b| a + b, |a, b| a + b),
        BinOp::Sub => eval_arithmetic(left, right, |a, b| a - b, |a, b| a - b),
        BinOp::Mul => eval_arithmetic(left, right, |a, b| a * b, |a, b| a * b),
        BinOp::Div => {
            // Division by zero → Null (Cypher semantics).
            match (left, right) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::I64(a), Value::I64(b)) => {
                    if *b == 0 {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::I64(a / b))
                    }
                }
                (Value::F64(a), Value::F64(b)) => {
                    if *b == 0.0 {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::F64(a / b))
                    }
                }
                (Value::I64(a), Value::F64(b)) => {
                    if *b == 0.0 {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::F64(*a as f64 / b))
                    }
                }
                (Value::F64(a), Value::I64(b)) => {
                    if *b == 0 {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::F64(a / *b as f64))
                    }
                }
                _ => Ok(Value::Null),
            }
        }
        BinOp::Mod => {
            // Modulo with null propagation and div-by-zero → Null.
            match (left, right) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::I64(a), Value::I64(b)) => {
                    if *b == 0 {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::I64(a % b))
                    }
                }
                (Value::F64(a), Value::F64(b)) => {
                    if *b == 0.0 {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::F64(a % b))
                    }
                }
                (Value::I64(a), Value::F64(b)) => {
                    if *b == 0.0 {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::F64(*a as f64 % b))
                    }
                }
                (Value::F64(a), Value::I64(b)) => {
                    if *b == 0 {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::F64(a % *b as f64))
                    }
                }
                _ => Ok(Value::Null),
            }
        }
    }
}

/// Evaluate an arithmetic binary operation with numeric coercion.
fn eval_arithmetic(
    left: &Value,
    right: &Value,
    int_op: impl Fn(i64, i64) -> i64,
    float_op: impl Fn(f64, f64) -> f64,
) -> crate::types::Result<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::I64(a), Value::I64(b)) => Ok(Value::I64(int_op(*a, *b))),
        (Value::F64(a), Value::F64(b)) => Ok(Value::F64(float_op(*a, *b))),
        (Value::I64(a), Value::F64(b)) => Ok(Value::F64(float_op(*a as f64, *b))),
        (Value::F64(a), Value::I64(b)) => Ok(Value::F64(float_op(*a, *b as f64))),
        // String concatenation with +.
        (Value::String(a), Value::String(b)) => Ok(Value::String(format!("{a}{b}"))),
        _ => Ok(Value::Null),
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
        (Value::Map(a), Value::Map(b)) => a == b,
        (Value::Date(a), Value::Date(b)) => a == b,
        (Value::LocalTime(a), Value::LocalTime(b)) => a == b,
        (Value::Time(a), Value::Time(b)) => a == b,
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => a == b,
        (Value::DateTime(a), Value::DateTime(b)) => a == b,
        (Value::Duration(a), Value::Duration(b)) => a == b,
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
        (Value::Date(a), Value::Date(b)) => Some(a.0.cmp(&b.0)),
        (Value::LocalTime(a), Value::LocalTime(b)) => Some(a.0.cmp(&b.0)),
        (Value::Time(a), Value::Time(b)) => {
            // Compare by converting to UTC.
            let a_utc = a.0 - a.1;
            let b_utc = b.0 - b.1;
            Some(a_utc.cmp(&b_utc))
        }
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => Some(a.0.cmp(&b.0)),
        (Value::DateTime(a), Value::DateTime(b)) => {
            let a_utc = a.0 - a.1;
            let b_utc = b.0 - b.1;
            Some(a_utc.cmp(&b_utc))
        }
        // Duration is NOT orderable per Cypher spec.
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
        Expr::Literal(lit) => match lit {
            LiteralValue::Null => "null".to_string(),
            LiteralValue::Bool(b) => b.to_string(),
            LiteralValue::I64(n) => n.to_string(),
            LiteralValue::F64(n) => n.to_string(),
            LiteralValue::String(s) => format!("'{s}'"),
        },
        Expr::Case { .. } => "CASE".to_string(),
        Expr::List(items) => {
            let inner: Vec<String> = items.iter().map(expr_to_column_name).collect();
            format!("[{}]", inner.join(", "))
        }
        Expr::Index { expr, index } => {
            format!(
                "{}[{}]",
                expr_to_column_name(expr),
                expr_to_column_name(index)
            )
        }
        Expr::Slice { expr, start, end } => {
            let s = start
                .as_ref()
                .map(|e| expr_to_column_name(e))
                .unwrap_or_default();
            let e = end
                .as_ref()
                .map(|e| expr_to_column_name(e))
                .unwrap_or_default();
            format!("{}[{}..{}]", expr_to_column_name(expr), s, e)
        }
        _ => "_expr".to_string(),
    }
}

/// Extract a temporal component accessor (e.g., `d.year`, `t.hour`).
/// Returns None if the value is not temporal or the property is not a known accessor.
fn temporal_accessor(val: &Value, prop: &str) -> Option<Value> {
    use chrono::{Datelike, Timelike};

    match val {
        Value::Date(d) => match prop {
            "year" => Some(Value::I64(d.0.year() as i64)),
            "month" => Some(Value::I64(d.0.month() as i64)),
            "day" => Some(Value::I64(d.0.day() as i64)),
            "ordinalDay" => Some(Value::I64(d.0.ordinal() as i64)),
            "weekYear" | "week" => Some(Value::I64(d.0.iso_week().week() as i64)),
            "dayOfWeek" => Some(Value::I64(d.0.weekday().num_days_from_monday() as i64 + 1)),
            "quarter" => Some(Value::I64(((d.0.month() - 1) / 3 + 1) as i64)),
            _ => None,
        },
        Value::LocalTime(lt) => {
            let t = &lt.0;
            match prop {
                "hour" => Some(Value::I64(t.hour() as i64)),
                "minute" => Some(Value::I64(t.minute() as i64)),
                "second" => Some(Value::I64(t.second() as i64)),
                "nanosecond" => Some(Value::I64(t.nanosecond() as i64 % 1_000_000_000)),
                "microsecond" => Some(Value::I64((t.nanosecond() as i64 % 1_000_000_000) / 1_000)),
                "millisecond" => Some(Value::I64(
                    (t.nanosecond() as i64 % 1_000_000_000) / 1_000_000,
                )),
                _ => None,
            }
        }
        Value::Time(ct) => {
            let t = &ct.0;
            match prop {
                "hour" => Some(Value::I64(t.hour() as i64)),
                "minute" => Some(Value::I64(t.minute() as i64)),
                "second" => Some(Value::I64(t.second() as i64)),
                "nanosecond" => Some(Value::I64(t.nanosecond() as i64 % 1_000_000_000)),
                "microsecond" => Some(Value::I64((t.nanosecond() as i64 % 1_000_000_000) / 1_000)),
                "millisecond" => Some(Value::I64(
                    (t.nanosecond() as i64 % 1_000_000_000) / 1_000_000,
                )),
                "offset" => Some(Value::String(crate::temporal::fmt_offset_public(&ct.1))),
                "offsetMinutes" => Some(Value::I64(ct.1.local_minus_utc() as i64 / 60)),
                "offsetSeconds" => Some(Value::I64(ct.1.local_minus_utc() as i64)),
                _ => None,
            }
        }
        Value::LocalDateTime(dt) => {
            // Try date accessors first, then time accessors.
            let date_val = Value::Date(crate::temporal::CypherDate(dt.0.date()));
            if let Some(v) = temporal_accessor(&date_val, prop) {
                return Some(v);
            }
            let time_val = Value::LocalTime(crate::temporal::CypherLocalTime(dt.0.time()));
            temporal_accessor(&time_val, prop)
        }
        Value::DateTime(dt) => {
            // Try date accessors, then time accessors, then offset accessors.
            let date_val = Value::Date(crate::temporal::CypherDate(dt.0.date()));
            if let Some(v) = temporal_accessor(&date_val, prop) {
                return Some(v);
            }
            let time_val = Value::Time(crate::temporal::CypherTime(dt.0.time(), dt.1));
            temporal_accessor(&time_val, prop)
        }
        Value::Duration(d) => match prop {
            "years" => Some(Value::I64(d.months / 12)),
            "months" => Some(Value::I64(d.months)),
            "monthsOfYear" => Some(Value::I64(d.months % 12)),
            "days" => Some(Value::I64(d.days)),
            "hours" => Some(Value::I64(d.seconds / 3600)),
            "minutes" => Some(Value::I64(d.seconds / 60)),
            "seconds" => Some(Value::I64(d.seconds)),
            "nanoseconds" => Some(Value::I64(d.seconds * 1_000_000_000 + d.nanos)),
            "nanosecondsOfSecond" => Some(Value::I64(d.nanos)),
            "milliseconds" => Some(Value::I64(d.seconds * 1_000 + d.nanos / 1_000_000)),
            "microseconds" => Some(Value::I64(d.seconds * 1_000_000 + d.nanos / 1_000)),
            "minutesOfHour" => Some(Value::I64((d.seconds / 60) % 60)),
            "secondsOfMinute" => Some(Value::I64(d.seconds % 60)),
            _ => None,
        },
        _ => None,
    }
}
