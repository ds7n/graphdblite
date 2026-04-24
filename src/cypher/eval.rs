use rusqlite::Connection;

use crate::cypher::ast::{BinOp, Expr, LiteralValue, QuantifierKind};
use crate::cypher::record::Record;
use crate::types::{GraphError, QueryError, QueryPhase, Value};

/// Evaluate an expression against a record, producing a Value.
///
/// The `conn` parameter is needed for EXISTS subquery evaluation.
pub fn eval_expr(expr: &Expr, record: &Record, conn: &Connection) -> crate::types::Result<Value> {
    match expr {
        Expr::Literal(lit) => Ok(literal_to_value(lit)),
        Expr::Variable(name) => Ok(record.get(name).cloned().unwrap_or(Value::Null)),
        Expr::Property(var, prop) => {
            // Check if the entity has been deleted (e.g. DELETE n RETURN n.prop).
            if record.get(&format!("{var}.__deleted")) == Some(&Value::Bool(true)) {
                return Err(GraphError::Query(crate::types::QueryError::EntityNotFound {
                    phase: crate::types::QueryPhase::Runtime,
                    message: format!(
                        "DeletedEntityAccess: cannot access property `{prop}` on deleted entity `{var}`"
                    ),
                }));
            }
            // If the variable is explicitly bound to Null (e.g. from OPTIONAL MATCH),
            // property access should return Null per Cypher's null propagation rules.
            if record.get(var) == Some(&Value::Null) {
                return Ok(Value::Null);
            }
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
            // Map property access: map.key
            if let Some(Value::Map(map)) = record.get(var) {
                return Ok(map.get(prop).cloned().unwrap_or(Value::Null));
            }
            // Temporal component accessor: d.year, d.month, etc.
            if let Some(val) = record.get(var) {
                if let Some(result) = temporal_accessor(val, prop) {
                    return Ok(result);
                }
                // If the variable is a pure scalar (no compound node/edge
                // metadata), property access is a type error.
                let has_metadata = record.get(&format!("{var}.__id")).is_some()
                    || record.get(&format!("{var}.__src")).is_some()
                    || record.get(&format!("{var}.__type")).is_some();
                if !has_metadata {
                    match val {
                        Value::Bool(_)
                        | Value::I64(_)
                        | Value::F64(_)
                        | Value::String(_)
                        | Value::List(_) => {
                            return Err(GraphError::type_error(
                                crate::types::QueryPhase::Runtime,
                                format!(
                                    "InvalidArgumentType: property access on {}",
                                    value_type_name(val)
                                ),
                            ));
                        }
                        _ => {}
                    }
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
            // If the base is a variable, first try to build a compound binding
            // for dynamic property access (n['name']).
            if let Expr::Variable(var) = expr.as_ref() {
                let idx_val = eval_expr(index, record, conn)?;
                if let Value::String(key) = &idx_val {
                    // Dynamic property access on a node/edge variable.
                    let prop_key = format!("{var}.{key}");
                    if let Some(val) = record.get(&prop_key) {
                        return Ok(val.clone());
                    }
                    // Check if var is null.
                    if record.get(var) == Some(&Value::Null) {
                        return Ok(Value::Null);
                    }
                    // Try map access.
                    if let Some(Value::Map(map)) = record.get(var) {
                        return Ok(map.get(key).cloned().unwrap_or(Value::Null));
                    }
                    // Fallback: look up from database if var has __id metadata.
                    let id_key = format!("{var}.__id");
                    if let Some(Value::I64(id)) = record.get(&id_key) {
                        if let Ok(node) =
                            crate::node::get_node(conn, crate::types::NodeId(*id as u64))
                        {
                            return Ok(node
                                .properties
                                .get(key.as_str())
                                .cloned()
                                .unwrap_or(Value::Null));
                        }
                    }
                }
            }
            let base = eval_expr(expr, record, conn)?;
            let idx = eval_expr(index, record, conn)?;
            match (&base, &idx) {
                (Value::List(items), Value::I64(i)) => {
                    let len = items.len() as i64;
                    let resolved = if *i < 0 { len + *i } else { *i };
                    if resolved >= 0 && (resolved as usize) < items.len() {
                        Ok(items[resolved as usize].clone())
                    } else {
                        Ok(Value::Null)
                    }
                }
                // Dynamic property access on node/edge/map values.
                (Value::Node(n), Value::String(key)) => {
                    Ok(n.properties.get(key).cloned().unwrap_or(Value::Null))
                }
                (Value::Edge(e), Value::String(key)) => {
                    Ok(e.properties.get(key).cloned().unwrap_or(Value::Null))
                }
                (Value::Map(m), Value::String(key)) => {
                    Ok(m.get(key).cloned().unwrap_or(Value::Null))
                }
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                // Indexing a non-list/non-map/non-node/non-edge with an integer.
                (_, Value::I64(_)) => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "InvalidArgumentType: cannot index a non-list value".to_string(),
                )),
                // Indexing a list with a non-integer.
                (Value::List(_), _) => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "InvalidArgumentType: list index must be an integer".to_string(),
                )),
                _ => Ok(Value::Null),
            }
        }
        Expr::DotAccess { expr, key } => {
            let base = eval_expr(expr, record, conn)?;
            match &base {
                Value::Map(m) => Ok(m.get(key).cloned().unwrap_or(Value::Null)),
                Value::Node(n) => Ok(n.properties.get(key).cloned().unwrap_or(Value::Null)),
                Value::Edge(e) => Ok(e.properties.get(key).cloned().unwrap_or(Value::Null)),
                Value::Null => Ok(Value::Null),
                _ => {
                    if let Some(result) = temporal_accessor(&base, key) {
                        return Ok(result);
                    }
                    Ok(Value::Null)
                }
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
                    // Null bounds → null result
                    if matches!(&start_val, Some(Value::Null))
                        || matches!(&end_val, Some(Value::Null))
                    {
                        return Ok(Value::Null);
                    }
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
        Expr::HasLabel(var, labels) => {
            // n:Label — check if the node bound to var has all specified labels.
            if record.get(var) == Some(&Value::Null) {
                return Ok(Value::Null);
            }
            let label_key = format!("{var}.__labels");
            if let Some(Value::List(node_labels)) = record.get(&label_key) {
                let has_all = labels
                    .iter()
                    .all(|lbl| node_labels.contains(&Value::String(lbl.clone())));
                Ok(Value::Bool(has_all))
            } else {
                // Fallback: look up from database.
                let id_key = format!("{var}.__id");
                if let Some(Value::I64(id)) = record.get(&id_key) {
                    if let Ok(node) = crate::node::get_node(conn, crate::types::NodeId(*id as u64))
                    {
                        let has_all = labels.iter().all(|lbl| node.labels.contains(lbl));
                        return Ok(Value::Bool(has_all));
                    }
                }
                Ok(Value::Bool(false))
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
            match to_tribool(&val)? {
                Some(b) => Ok(Value::Bool(!b)),
                None => Ok(Value::Null),
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
            operand,
            alternatives,
            default,
        } => {
            if let Some(op_expr) = operand {
                // Simple CASE: CASE operand WHEN value THEN result ...
                let op_val = eval_expr(op_expr, record, conn)?;
                for (when_val_expr, result) in alternatives {
                    let when_val = eval_expr(when_val_expr, record, conn)?;
                    if values_equal(&op_val, &when_val) == Value::Bool(true) {
                        return eval_expr(result, record, conn);
                    }
                }
            } else {
                // Searched CASE: CASE WHEN cond THEN result ...
                for (cond, result) in alternatives {
                    if eval_predicate(cond, record, conn)? {
                        return eval_expr(result, record, conn);
                    }
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
        Expr::PatternComprehension {
            path_variable,
            pattern,
            where_clause,
            map_expr,
        } => eval_pattern_comprehension(
            path_variable.as_deref(),
            pattern,
            where_clause.as_deref(),
            map_expr,
            record,
            conn,
        ),
        Expr::Exists {
            patterns,
            where_clause,
        } => eval_exists(patterns, where_clause.as_deref(), record, conn),
        Expr::PatternPredicate(pattern) => {
            eval_exists(std::slice::from_ref(pattern), None, record, conn)
        }
        Expr::ExistsSubquery(stmt) => eval_exists_subquery(stmt, record, conn),
        Expr::MapLiteral(pairs) => {
            let mut map = std::collections::BTreeMap::new();
            for (k, expr) in pairs {
                let val = eval_expr(expr, record, conn)?;
                map.insert(k.clone(), val);
            }
            Ok(Value::Map(map))
        }
        Expr::FunctionCall { name, args, .. } => eval_function_call(name, args, record, conn),
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

/// Build a TypeError for invalid argument types passed to type conversion functions.
fn invalid_argument_type(func_name: &str, value: &Value) -> GraphError {
    GraphError::Query(QueryError::TypeError {
        phase: QueryPhase::Runtime,
        message: format!(
            "{func_name}: invalid argument type {}",
            value_type_name(value)
        ),
    })
}

/// Format a float for toString() output. Uses decimal notation without
/// trailing zeros, but always keeps at least one decimal digit (e.g. "3.0").
fn format_float(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
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
    let name_lower = name.to_ascii_lowercase();

    // Check for deleted entity access in function arguments.
    // Note: type() and id() are allowed on deleted entities per openCypher spec.
    if matches!(name_lower.as_str(), "labels" | "keys" | "properties") {
        if let Some(Expr::Variable(var)) = args.first() {
            if record.get(&format!("{var}.__deleted")) == Some(&Value::Bool(true)) {
                return Err(GraphError::Query(crate::types::QueryError::EntityNotFound {
                    phase: crate::types::QueryPhase::Runtime,
                    message: format!(
                        "DeletedEntityAccess: cannot call {name}() on deleted entity `{var}`"
                    ),
                }));
            }
        }
    }

    // If this is an aggregate function, check for a pre-computed value in the
    // record (placed by the Aggregate executor). This allows expressions like
    // `count(a) > 0` to reference the aggregate result during projection.
    if matches!(
        name_lower.as_str(),
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
        let col = expr_to_column_name(&Expr::FunctionCall {
            name: name.to_string(),
            args: args.to_vec(),
            distinct: false,
        });
        if let Some(val) = record.get(&col) {
            return Ok(val.clone());
        }
    }

    match name_lower.as_str() {
        "length" => {
            let arg = args
                .first()
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match arg {
                Some(Value::Path(p)) => Ok(Value::I64(p.len() as i64)),
                Some(Value::String(s)) => Ok(Value::I64(s.len() as i64)),
                Some(Value::List(items)) => Ok(Value::I64(items.len() as i64)),
                Some(Value::Null) | None => Ok(Value::Null),
                Some(other) => Err(invalid_argument_type("length()", &other)),
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
                Value::I64(n) => Ok(Value::String(n.to_string())),
                Value::F64(f) => Ok(Value::String(format_float(f))),
                Value::Bool(b) => Ok(Value::String(b.to_string())),
                Value::Date(d) => Ok(Value::String(d.to_string())),
                Value::LocalTime(t) => Ok(Value::String(t.to_string())),
                Value::Time(t) => Ok(Value::String(t.to_string())),
                Value::LocalDateTime(dt) => Ok(Value::String(dt.to_string())),
                Value::DateTime(dt) => Ok(Value::String(dt.to_string())),
                Value::Duration(d) => Ok(Value::String(d.to_string())),
                other => Err(invalid_argument_type("toString()", &other)),
            }
        }
        "toboolean" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Null => Ok(Value::Null),
                Value::Bool(_) => Ok(arg),
                Value::String(s) => match s.to_lowercase().as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    _ => Ok(Value::Null),
                },
                other => Err(invalid_argument_type("toBoolean()", &other)),
            }
        }
        "tointeger" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Null => Ok(Value::Null),
                Value::I64(_) => Ok(arg),
                Value::F64(f) => Ok(Value::I64(f as i64)),
                Value::String(s) => {
                    // Try integer first, then float (truncated).
                    if let Ok(n) = s.parse::<i64>() {
                        Ok(Value::I64(n))
                    } else if let Ok(f) = s.parse::<f64>() {
                        Ok(Value::I64(f as i64))
                    } else {
                        Ok(Value::Null)
                    }
                }
                Value::Bool(b) => Ok(Value::I64(if b { 1 } else { 0 })),
                other => Err(invalid_argument_type("toInteger()", &other)),
            }
        }
        "tofloat" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Null => Ok(Value::Null),
                Value::F64(_) => Ok(arg),
                Value::I64(n) => Ok(Value::F64(n as f64)),
                Value::String(s) => Ok(s.parse::<f64>().map(Value::F64).unwrap_or(Value::Null)),
                other => Err(invalid_argument_type("toFloat()", &other)),
            }
        }
        "keys" => {
            // keys(n) — return property keys for a node, edge, or map.
            // For variable-based lookup, prefer the database (source of truth
            // after mutations like REMOVE).
            if let Some(Expr::Variable(var)) = args.first() {
                let id_key = format!("{var}.__id");
                if let Some(Value::I64(id)) = record.get(&id_key) {
                    if let Ok(node) = crate::node::get_node(conn, crate::types::NodeId(*id as u64))
                    {
                        let mut keys: Vec<String> = node.properties.keys().cloned().collect();
                        keys.sort();
                        return Ok(Value::List(keys.into_iter().map(Value::String).collect()));
                    }
                }
                // Try edge binding.
                let type_key = format!("{var}.__type");
                let src_key = format!("{var}.__src");
                if record.get(&type_key).is_some() && record.get(&src_key).is_some() {
                    if let Some(Value::Edge(e)) =
                        crate::cypher::executor::build_compound_binding(record, var)
                    {
                        let mut keys: Vec<String> = e.properties.keys().cloned().collect();
                        keys.sort();
                        return Ok(Value::List(keys.into_iter().map(Value::String).collect()));
                    }
                }
                // Map binding.
                if let Some(Value::Map(m)) = record.get(var) {
                    let mut keys: Vec<String> = m.keys().cloned().collect();
                    keys.sort();
                    return Ok(Value::List(keys.into_iter().map(Value::String).collect()));
                }
            }
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Node(n) => {
                    let mut keys: Vec<String> = n.properties.keys().cloned().collect();
                    keys.sort();
                    Ok(Value::List(keys.into_iter().map(Value::String).collect()))
                }
                Value::Edge(e) => {
                    let mut keys: Vec<String> = e.properties.keys().cloned().collect();
                    keys.sort();
                    Ok(Value::List(keys.into_iter().map(Value::String).collect()))
                }
                Value::Map(m) => {
                    let mut keys: Vec<String> = m.keys().cloned().collect();
                    keys.sort();
                    Ok(Value::List(keys.into_iter().map(Value::String).collect()))
                }
                Value::I64(id) => {
                    // Legacy: keys(node_id) — look up from database.
                    let node = crate::node::get_node(conn, crate::types::NodeId(id as u64))?;
                    let mut keys: Vec<String> = node.properties.keys().cloned().collect();
                    keys.sort();
                    Ok(Value::List(keys.into_iter().map(Value::String).collect()))
                }
                Value::Null => Ok(Value::Null),
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
            // Handle compound Value::Node.
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Node(n) => Ok(Value::List(
                    n.labels.into_iter().map(Value::String).collect(),
                )),
                Value::Null => Ok(Value::Null),
                other => Err(invalid_argument_type("labels()", &other)),
            }
        }
        "id" => {
            // id(n) — extract the node/edge ID.
            if let Some(Expr::Variable(var)) = args.first() {
                let id_key = format!("{var}.__id");
                if let Some(val @ Value::I64(_)) = record.get(&id_key) {
                    return Ok(val.clone());
                }
            }
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(_) => Ok(arg),
                Value::Node(n) => Ok(Value::I64(n.id.0 as i64)),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "type" => {
            // type(r) — extract the relationship type from a binding.
            if let Some(Expr::Variable(var)) = args.first() {
                let type_key = format!("{var}.__type");
                if let Some(Value::String(s)) = record.get(&type_key) {
                    return Ok(Value::String(s.clone()));
                }
                // If the variable itself is bound to Null (e.g. OPTIONAL MATCH with no match)
                if record.get(var) == Some(&Value::Null) {
                    return Ok(Value::Null);
                }
                // Fallback: if the variable is bound to a relationship type string
                // (legacy flat-record shape where var = type_name).
                if let Some(Value::String(s)) = record.get(var) {
                    if record.get(&format!("{var}.__src")).is_some() {
                        return Ok(Value::String(s.clone()));
                    }
                }
            }
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Edge(e) => Ok(Value::String(e.label)),
                // A relationship variable in flat records is stored as String(type_name).
                Value::String(_) => Ok(arg),
                Value::Null => Ok(Value::Null),
                other => Err(invalid_argument_type("type()", &other)),
            }
        }
        "properties" => {
            // properties(n) — return a map of all properties on a node/edge/map.
            if let Some(Expr::Variable(var)) = args.first() {
                if let Some(compound) = crate::cypher::executor::build_compound_binding(record, var)
                {
                    return match compound {
                        Value::Node(n) => {
                            let map = n.properties.into_iter().collect();
                            Ok(Value::Map(map))
                        }
                        Value::Edge(e) => {
                            let map = e.properties.into_iter().collect();
                            Ok(Value::Map(map))
                        }
                        _ => Ok(Value::Null),
                    };
                }
                // If variable is null (OPTIONAL MATCH with no match), return null.
                if record.get(var) == Some(&Value::Null) {
                    return Ok(Value::Null);
                }
            }
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Node(n) => {
                    let map = n.properties.into_iter().collect();
                    Ok(Value::Map(map))
                }
                Value::Edge(e) => {
                    let map = e.properties.into_iter().collect();
                    Ok(Value::Map(map))
                }
                Value::Map(_) => Ok(arg),
                Value::Null => Ok(Value::Null),
                other => Err(invalid_argument_type("properties()", &other)),
            }
        }
        "relationships" => {
            // relationships(p) — return list of edges in a path.
            let arg = args
                .first()
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match arg {
                Some(Value::Path(p)) => {
                    let list = p.edges.into_iter().map(Value::Edge).collect();
                    Ok(Value::List(list))
                }
                Some(Value::Null) | None => Ok(Value::Null),
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
        "sqrt" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::F64((n as f64).sqrt())),
                Value::F64(n) => Ok(Value::F64(n.sqrt())),
                Value::Null => Ok(Value::Null),
                other => Err(invalid_argument_type("sqrt()", &other)),
            }
        }
        "sign" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::I64(n.signum())),
                Value::F64(n) => Ok(Value::I64(if n.is_nan() { 0 } else { n.signum() as i64 })),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "ceil" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::F64(n as f64)),
                Value::F64(n) => Ok(Value::F64(n.ceil())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "floor" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::F64(n as f64)),
                Value::F64(n) => Ok(Value::F64(n.floor())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "round" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::F64(n as f64)),
                Value::F64(n) => Ok(Value::F64(n.round())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "log" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::F64((n as f64).ln())),
                Value::F64(n) => Ok(Value::F64(n.ln())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "log10" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::F64((n as f64).log10())),
                Value::F64(n) => Ok(Value::F64(n.log10())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "exp" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::I64(n) => Ok(Value::F64((n as f64).exp())),
                Value::F64(n) => Ok(Value::F64(n.exp())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "e" => Ok(Value::F64(std::f64::consts::E)),
        "pi" => Ok(Value::F64(std::f64::consts::PI)),
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
        "lTrim" | "ltrim" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.trim_start().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "rTrim" | "rtrim" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.trim_end().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "left" => {
            let s = eval_single_arg(args, record, conn)?;
            let len_val = args
                .get(1)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match (s, len_val) {
                (Value::String(s), Some(Value::I64(n))) => {
                    let n = n.max(0) as usize;
                    Ok(Value::String(s.chars().take(n).collect()))
                }
                (Value::Null, _) | (_, Some(Value::Null)) => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "right" => {
            let s = eval_single_arg(args, record, conn)?;
            let len_val = args
                .get(1)
                .map(|a| eval_expr(a, record, conn))
                .transpose()?;
            match (s, len_val) {
                (Value::String(s), Some(Value::I64(n))) => {
                    let n = n.max(0) as usize;
                    let chars: Vec<char> = s.chars().collect();
                    let start = chars.len().saturating_sub(n);
                    Ok(Value::String(chars[start..].iter().collect()))
                }
                (Value::Null, _) | (_, Some(Value::Null)) => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "startNode" | "startnode" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "endNode" | "endnode" => {
            let arg = eval_single_arg(args, record, conn)?;
            match arg {
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "exists" => {
            let arg = eval_single_arg(args, record, conn)?;
            Ok(Value::Bool(arg != Value::Null))
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
        "rand" => {
            // rand() — returns a random float in [0, 1).
            use std::collections::hash_map::RandomState;
            use std::hash::{BuildHasher, Hasher};
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(0);
            let bits = hasher.finish();
            // Convert to f64 in [0, 1)
            let val = (bits >> 11) as f64 / (1u64 << 53) as f64;
            Ok(Value::F64(val))
        }
        // Temporal constructor functions.
        "date" | "date.transaction" | "date.statement" | "date.realtime" => {
            eval_temporal_constructor(
                args,
                record,
                conn,
                |s| crate::temporal::CypherDate::from_iso_string(s).map(Value::Date),
                |m| crate::temporal::CypherDate::from_map(m).map(Value::Date),
            )
        }
        "localtime" | "localtime.transaction" | "localtime.statement" | "localtime.realtime" => {
            eval_temporal_constructor(
                args,
                record,
                conn,
                |s| crate::temporal::CypherLocalTime::from_iso_string(s).map(Value::LocalTime),
                |m| crate::temporal::CypherLocalTime::from_map(m).map(Value::LocalTime),
            )
        }
        "time" | "time.transaction" | "time.statement" | "time.realtime" => {
            eval_temporal_constructor(
                args,
                record,
                conn,
                |s| crate::temporal::CypherTime::from_iso_string(s).map(Value::Time),
                |m| crate::temporal::CypherTime::from_map(m).map(Value::Time),
            )
        }
        "localdatetime"
        | "localdatetime.transaction"
        | "localdatetime.statement"
        | "localdatetime.realtime" => eval_temporal_constructor(
            args,
            record,
            conn,
            |s| crate::temporal::CypherLocalDateTime::from_iso_string(s).map(Value::LocalDateTime),
            |m| crate::temporal::CypherLocalDateTime::from_map(m).map(Value::LocalDateTime),
        ),
        "datetime" | "datetime.transaction" | "datetime.statement" | "datetime.realtime" => {
            eval_temporal_constructor(
                args,
                record,
                conn,
                |s| crate::temporal::CypherDateTime::from_iso_string(s).map(Value::DateTime),
                |m| crate::temporal::CypherDateTime::from_map(m).map(Value::DateTime),
            )
        }
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
        "duration.between" | "duration.inmonths" | "duration.indays" | "duration.inseconds" => {
            if args.len() < 2 {
                return Ok(Value::Null);
            }
            let lhs = eval_expr(&args[0], record, conn)?;
            let rhs = eval_expr(&args[1], record, conn)?;
            if lhs == Value::Null || rhs == Value::Null {
                return Ok(Value::Null);
            }
            let dur = match name {
                "duration.between" => crate::temporal::duration_between(&lhs, &rhs),
                "duration.inmonths" => crate::temporal::duration_in_months(&lhs, &rhs),
                "duration.indays" => crate::temporal::duration_in_days(&lhs, &rhs),
                "duration.inseconds" => crate::temporal::duration_in_seconds(&lhs, &rhs),
                _ => unreachable!(),
            };
            Ok(Value::Duration(dur))
        }
        // Temporal truncation functions.
        "date.truncate" => {
            if args.len() < 2 {
                return Ok(Value::Null);
            }
            let unit = match eval_expr(&args[0], record, conn)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, conn)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, conn)? {
                    Value::Map(m) => m,
                    _ => std::collections::BTreeMap::new(),
                }
            } else {
                std::collections::BTreeMap::new()
            };
            let d = crate::temporal::truncate_date(&unit, &val, &map)?;
            Ok(Value::Date(crate::temporal::CypherDate(d)))
        }
        "localtime.truncate" => {
            if args.len() < 2 {
                return Ok(Value::Null);
            }
            let unit = match eval_expr(&args[0], record, conn)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, conn)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, conn)? {
                    Value::Map(m) => m,
                    _ => std::collections::BTreeMap::new(),
                }
            } else {
                std::collections::BTreeMap::new()
            };
            let t = crate::temporal::truncate_time(&unit, &val, &map)?;
            Ok(Value::LocalTime(crate::temporal::CypherLocalTime(t)))
        }
        "time.truncate" => {
            if args.len() < 2 {
                return Ok(Value::Null);
            }
            let unit = match eval_expr(&args[0], record, conn)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, conn)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, conn)? {
                    Value::Map(m) => m,
                    _ => std::collections::BTreeMap::new(),
                }
            } else {
                std::collections::BTreeMap::new()
            };
            // Determine offset: from map timezone override, or from source value, or UTC.
            let offset = if let Some(Value::String(tz_s)) = map.get("timezone") {
                let off_secs = crate::temporal::parse_offset_public(tz_s)?;
                chrono::FixedOffset::east_opt(off_secs).unwrap()
            } else {
                // Inherit from source temporal.
                let off_secs = crate::temporal::extract_offset_secs(&val);
                chrono::FixedOffset::east_opt(off_secs).unwrap()
            };
            let t = crate::temporal::truncate_time(&unit, &val, &map)?;
            Ok(Value::Time(crate::temporal::CypherTime(t, offset)))
        }
        "localdatetime.truncate" => {
            if args.len() < 2 {
                return Ok(Value::Null);
            }
            let unit = match eval_expr(&args[0], record, conn)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, conn)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, conn)? {
                    Value::Map(m) => m,
                    _ => std::collections::BTreeMap::new(),
                }
            } else {
                std::collections::BTreeMap::new()
            };
            let d = crate::temporal::truncate_date(&unit, &val, &map)?;
            let t = crate::temporal::truncate_time(&unit, &val, &map)?;
            let ndt = d.and_time(t);
            Ok(Value::LocalDateTime(crate::temporal::CypherLocalDateTime(
                ndt,
            )))
        }
        "datetime.truncate" => {
            if args.len() < 2 {
                return Ok(Value::Null);
            }
            let unit = match eval_expr(&args[0], record, conn)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, conn)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, conn)? {
                    Value::Map(m) => m,
                    _ => std::collections::BTreeMap::new(),
                }
            } else {
                std::collections::BTreeMap::new()
            };
            let d = crate::temporal::truncate_date(&unit, &val, &map)?;
            let t = crate::temporal::truncate_time(&unit, &val, &map)?;
            let ndt = d.and_time(t);

            // Determine offset and timezone name.
            let (offset, tz_name) = if let Some(Value::String(tz_s)) = map.get("timezone") {
                // Check if it's an IANA name or an offset string.
                if tz_s.starts_with('+') || tz_s.starts_with('-') || tz_s == "Z" {
                    let off_secs = crate::temporal::parse_offset_public(tz_s)?;
                    (chrono::FixedOffset::east_opt(off_secs).unwrap(), None)
                } else {
                    // IANA timezone name — resolve at the truncated datetime.
                    use chrono::TimeZone;
                    use chrono_tz::Tz;
                    let tz: Tz = tz_s.parse().map_err(|_| {
                        crate::types::GraphError::Serialization(format!("unknown timezone: {tz_s}"))
                    })?;
                    let aware = tz.from_local_datetime(&ndt).earliest().ok_or_else(|| {
                        crate::types::GraphError::Serialization(format!(
                            "ambiguous or invalid datetime in timezone: {tz_s}"
                        ))
                    })?;
                    let off = chrono::Offset::fix(aware.offset());
                    (off, Some(tz_s.clone()))
                }
            } else {
                // Inherit offset from source temporal or default to UTC.
                match &val {
                    Value::DateTime(dt) => (dt.1, dt.2.clone()),
                    _ => (chrono::FixedOffset::east_opt(0).unwrap(), None),
                }
            };
            Ok(Value::DateTime(crate::temporal::CypherDateTime(
                ndt, offset, tz_name,
            )))
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
    if args.is_empty() {
        return from_map(&std::collections::BTreeMap::new());
    }
    let arg = eval_single_arg(args, record, conn)?;
    match arg {
        Value::String(s) => from_str(&s),
        Value::Map(m) => from_map(&m),
        Value::Null => Ok(Value::Null),
        Value::Date(_)
        | Value::LocalTime(_)
        | Value::Time(_)
        | Value::LocalDateTime(_)
        | Value::DateTime(_) => {
            let mut m = std::collections::BTreeMap::new();
            match &arg {
                Value::Date(_) => {
                    m.insert("date".to_string(), arg);
                }
                Value::LocalDateTime(_) | Value::DateTime(_) => {
                    // Use `datetime` key so both date_from_map and time_from_map pick it up.
                    m.insert("datetime".to_string(), arg);
                }
                Value::LocalTime(_) | Value::Time(_) => {
                    m.insert("time".to_string(), arg);
                }
                _ => {}
            }
            from_map(&m)
        }
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

/// Evaluate a pattern comprehension: [(p = )? pattern (WHERE pred)? | expr].
///
/// Plans and executes the pattern as a correlated subquery, evaluating the
/// map expression for each matching row and collecting results into a list.
fn eval_pattern_comprehension(
    path_variable: Option<&str>,
    pattern: &crate::cypher::ast::Pattern,
    where_clause: Option<&Expr>,
    map_expr: &Expr,
    record: &Record,
    conn: &Connection,
) -> crate::types::Result<Value> {
    use crate::cypher::executor::exec_correlated_subquery;
    use crate::cypher::ir::LogicalOp;
    use crate::cypher::planner::plan_patterns;

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
    let mut outer_rec = Record::new();
    for (k, v) in &record.fields {
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
    for row in &rows {
        // Merge the outer record with the inner row so the map expression
        // can reference both outer and inner variables.
        let mut merged = record.clone();
        for (k, v) in &row.fields {
            merged.set(k.clone(), v.clone());
        }

        let val = eval_expr(map_expr, &merged, conn)?;
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

/// Evaluate an EXISTS { MATCH ... [WITH ...] [RETURN ...] } full subquery predicate.
/// Plans the inner statement and executes it as a correlated subquery using the
/// outer record's bindings. Returns true if at least one row is produced.
fn eval_exists_subquery(
    stmt: &crate::cypher::ast::Statement,
    record: &Record,
    conn: &Connection,
) -> crate::types::Result<Value> {
    use crate::cypher::executor::exec_correlated_exists;
    use crate::cypher::planner::plan;

    let base_plan = plan(conn, stmt)?;

    // Execute the subquery as a correlated subquery, pushing the outer
    // record's bindings down so nested expressions can see them.
    let found = exec_correlated_exists(conn, &base_plan, record)?;
    Ok(Value::Bool(found))
}

fn eval_binop(left: &Value, op: BinOp, right: &Value) -> crate::types::Result<Value> {
    match op {
        // Three-valued AND: NULL AND false → false, NULL AND true → NULL
        BinOp::And => {
            match (to_tribool(left)?, to_tribool(right)?) {
                (Some(false), _) | (_, Some(false)) => Ok(Value::Bool(false)),
                (Some(true), Some(true)) => Ok(Value::Bool(true)),
                _ => Ok(Value::Null), // at least one NULL, none false
            }
        }
        // Three-valued XOR: NULL XOR anything → NULL
        BinOp::Xor => {
            match (to_tribool(left)?, to_tribool(right)?) {
                (Some(a), Some(b)) => Ok(Value::Bool(a ^ b)),
                _ => Ok(Value::Null), // at least one NULL
            }
        }
        // Three-valued OR: NULL OR true → true, NULL OR false → NULL
        BinOp::Or => {
            match (to_tribool(left)?, to_tribool(right)?) {
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
                let mut has_null = false;
                for item in items {
                    match values_equal(val, item) {
                        Value::Bool(true) => return Ok(Value::Bool(true)),
                        Value::Null => has_null = true,
                        _ => {}
                    }
                }
                if has_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Bool(false))
                }
            }
            (_, rhs) => Err(GraphError::type_error(
                crate::types::QueryPhase::Runtime,
                format!(
                    "InvalidArgumentType: IN requires a list on the right side, got {}",
                    value_type_name(rhs)
                ),
            )),
        },
        BinOp::Add => {
            if let Some(result) = eval_temporal_add(left, right) {
                result
            } else {
                // List concatenation and append/prepend.
                match (left, right) {
                    (Value::List(a), Value::List(b)) => {
                        let mut result = a.clone();
                        result.extend(b.iter().cloned());
                        Ok(Value::List(result))
                    }
                    (Value::List(a), val) => {
                        let mut result = a.clone();
                        result.push(val.clone());
                        Ok(Value::List(result))
                    }
                    (val, Value::List(b)) => {
                        let mut result = vec![val.clone()];
                        result.extend(b.iter().cloned());
                        Ok(Value::List(result))
                    }
                    _ => eval_arithmetic(left, right, |a, b| a + b, |a, b| a + b),
                }
            }
        }
        BinOp::Sub => {
            if let Some(result) = eval_temporal_sub(left, right) {
                result
            } else {
                eval_arithmetic(left, right, |a, b| a - b, |a, b| a - b)
            }
        }
        BinOp::Mul => {
            // Duration * Number
            if let Some(result) = eval_duration_mul(left, right) {
                result
            } else {
                eval_arithmetic(left, right, |a, b| a * b, |a, b| a * b)
            }
        }
        BinOp::Div => {
            // Duration / Number.
            if let (Value::Duration(d), Value::I64(n)) = (left, right) {
                if *n == 0 {
                    return Ok(Value::Null);
                }
                return Ok(Value::Duration(crate::temporal::CypherDuration {
                    months: d.months / n,
                    days: d.days / n,
                    seconds: d.seconds / n,
                    nanos: d.nanos / n,
                }));
            }
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
                    // Float division: 0.0/0.0 → NaN, x/0.0 → ±Inf (IEEE 754)
                    Ok(Value::F64(a / b))
                }
                (Value::I64(a), Value::F64(b)) => {
                    if *b == 0.0 && *a == 0 {
                        Ok(Value::F64(f64::NAN))
                    } else if *b == 0.0 {
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
        BinOp::Pow => {
            // Exponentiation: always returns Float.
            match (left, right) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::I64(a), Value::I64(b)) => Ok(Value::F64((*a as f64).powf(*b as f64))),
                (Value::F64(a), Value::F64(b)) => Ok(Value::F64(a.powf(*b))),
                (Value::I64(a), Value::F64(b)) => Ok(Value::F64((*a as f64).powf(*b))),
                (Value::F64(a), Value::I64(b)) => Ok(Value::F64(a.powf(*b as f64))),
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
/// Returns Err for non-boolean non-null values (InvalidArgumentType).
fn to_tribool(v: &Value) -> crate::types::Result<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        _ => Err(GraphError::Query(QueryError::SyntaxError {
            phase: QueryPhase::Runtime,
            message: format!(
                "Type mismatch: expected Boolean but was {}",
                value_type_name(v)
            ),
        })),
    }
}

/// Get a human-readable type name for a value.
fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "Boolean",
        Value::I64(_) => "Integer",
        Value::F64(_) => "Float",
        Value::String(_) => "String",
        Value::List(_) => "List",
        Value::Map(_) => "Map",
        Value::Null => "Null",
        Value::Node(_) => "Node",
        Value::Edge(_) => "Relationship",
        Value::Path(_) => "Path",
        Value::Date(_) => "Date",
        Value::LocalTime(_) => "LocalTime",
        Value::Time(_) => "Time",
        Value::LocalDateTime(_) => "LocalDateTime",
        Value::DateTime(_) => "DateTime",
        Value::Duration(_) => "Duration",
    }
}

/// Three-valued equality: returns Null if either operand is null.
fn values_equal(a: &Value, b: &Value) -> Value {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => Value::Bool(a == b),
        (Value::I64(a), Value::I64(b)) => Value::Bool(a == b),
        (Value::F64(a), Value::F64(b)) => Value::Bool(a == b),
        (Value::I64(a), Value::F64(b)) => Value::Bool((*a as f64) == *b),
        (Value::F64(a), Value::I64(b)) => Value::Bool(*a == (*b as f64)),
        (Value::String(a), Value::String(b)) => Value::Bool(a == b),
        (Value::List(a), Value::List(b)) => lists_equal(a, b),
        (Value::Map(a), Value::Map(b)) => maps_equal(a, b),
        (Value::Node(a), Value::Node(b)) => Value::Bool(a.id == b.id),
        (Value::Edge(a), Value::Edge(b)) => Value::Bool(a == b),
        (Value::Date(a), Value::Date(b)) => Value::Bool(a == b),
        (Value::LocalTime(a), Value::LocalTime(b)) => Value::Bool(a == b),
        (Value::Time(a), Value::Time(b)) => Value::Bool(a == b),
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => Value::Bool(a == b),
        (Value::DateTime(a), Value::DateTime(b)) => Value::Bool(a == b),
        (Value::Duration(a), Value::Duration(b)) => Value::Bool(a == b),
        _ => Value::Bool(false),
    }
}

/// Three-valued list equality: propagates null if any element comparison yields null.
fn lists_equal(a: &[Value], b: &[Value]) -> Value {
    if a.len() != b.len() {
        return Value::Bool(false);
    }
    let mut has_null = false;
    for (ae, be) in a.iter().zip(b.iter()) {
        match values_equal(ae, be) {
            Value::Bool(false) => return Value::Bool(false),
            Value::Null => has_null = true,
            _ => {} // true, continue
        }
    }
    if has_null {
        Value::Null
    } else {
        Value::Bool(true)
    }
}

/// Three-valued map equality: propagates null if values with matching keys compare as null.
fn maps_equal(
    a: &std::collections::BTreeMap<String, Value>,
    b: &std::collections::BTreeMap<String, Value>,
) -> Value {
    // Maps with different key sets are not equal (keys with null values still count).
    let a_keys: std::collections::BTreeSet<&String> = a.keys().collect();
    let b_keys: std::collections::BTreeSet<&String> = b.keys().collect();
    if a_keys != b_keys {
        return Value::Bool(false);
    }
    let mut has_null = false;
    for key in &a_keys {
        let av = a.get(*key).unwrap();
        let bv = b.get(*key).unwrap();
        match values_equal(av, bv) {
            Value::Bool(false) => return Value::Bool(false),
            Value::Null => has_null = true,
            _ => {}
        }
    }
    if has_null {
        Value::Null
    } else {
        Value::Bool(true)
    }
}

/// Compare two values, returning Null if either is null or types are incomparable.
fn compare_to_value(a: &Value, b: &Value, pred: impl Fn(std::cmp::Ordering) -> bool) -> Value {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    match compare_values(a, b) {
        Some(ord) => Value::Bool(pred(ord)),
        None => {
            // NaN comparisons with numeric types yield false (not null).
            let both_numeric = is_numeric(a) && is_numeric(b);
            if both_numeric {
                Value::Bool(false) // NaN involved
            } else {
                Value::Null // cross-type → null
            }
        }
    }
}

/// Check if a value is a numeric type (integer or float).
fn is_numeric(v: &Value) -> bool {
    matches!(v, Value::I64(_) | Value::F64(_))
}

fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
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
        // List comparison: lexicographic ordering.
        (Value::List(a), Value::List(b)) => {
            for (ae, be) in a.iter().zip(b.iter()) {
                match compare_values(ae, be) {
                    Some(std::cmp::Ordering::Equal) => continue,
                    other => return other,
                }
            }
            Some(a.len().cmp(&b.len()))
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

/// Return operator precedence (higher = binds tighter).
fn binop_precedence(op: &BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::Xor => 2,
        BinOp::And => 3,
        BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte => 5,
        BinOp::In | BinOp::StartsWith | BinOp::EndsWith | BinOp::Contains => 5,
        BinOp::Add | BinOp::Sub => 6,
        BinOp::Mul | BinOp::Div | BinOp::Mod => 7,
        BinOp::Pow => 8,
    }
}

/// Format a child expression, wrapping in parens if its precedence is lower.
fn format_child_expr(expr: &Expr, parent_prec: u8, is_left: bool) -> String {
    let needs_parens = if let Expr::BinaryOp { op, .. } = expr {
        let child_prec = binop_precedence(op);
        // Parenthesize if child has lower precedence, or same precedence
        // on the right side (to preserve left-to-right grouping).
        child_prec < parent_prec || (child_prec == parent_prec && !is_left)
    } else {
        false
    };
    let s = expr_to_column_name(expr);
    if needs_parens {
        format!("({s})")
    } else {
        s
    }
}

/// Resolve an expression to a column name for RETURN projections.
pub fn expr_to_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(var, prop) => format!("{var}.{prop}"),
        Expr::FunctionCall {
            name,
            args,
            distinct,
        } => {
            let dist_prefix = if *distinct { "DISTINCT " } else { "" };
            if args.is_empty() || matches!(args[0], Expr::Star) {
                format!("{name}(*)")
            } else {
                let arg_names: Vec<String> = args.iter().map(expr_to_column_name).collect();
                format!("{name}({dist_prefix}{})", arg_names.join(", "))
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
        Expr::DotAccess { expr, key } => {
            format!("{}.{}", expr_to_column_name(expr), key)
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
        Expr::BinaryOp { left, op, right } => {
            let op_str = match op {
                BinOp::Add => " + ",
                BinOp::Sub => " - ",
                BinOp::Mul => " * ",
                BinOp::Div => " / ",
                BinOp::Mod => " % ",
                BinOp::Pow => " ^ ",
                BinOp::Eq => " = ",
                BinOp::Neq => " <> ",
                BinOp::Lt => " < ",
                BinOp::Gt => " > ",
                BinOp::Lte => " <= ",
                BinOp::Gte => " >= ",
                BinOp::And => " AND ",
                BinOp::Or => " OR ",
                BinOp::Xor => " XOR ",
                BinOp::In => " IN ",
                BinOp::StartsWith => " STARTS WITH ",
                BinOp::EndsWith => " ENDS WITH ",
                BinOp::Contains => " CONTAINS ",
            };
            let prec = binop_precedence(op);
            let l = format_child_expr(left, prec, true);
            let r = format_child_expr(right, prec, false);
            format!("{l}{op_str}{r}")
        }
        Expr::IsNull(inner) => format!("{} IS NULL", expr_to_column_name(inner)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", expr_to_column_name(inner)),
        Expr::Not(inner) => format!("NOT {}", expr_to_column_name(inner)),
        Expr::PatternComprehension { .. } => "_expr".to_string(),
        Expr::HasLabel(var, labels) => {
            let label_str: Vec<String> = labels.iter().map(|l| format!(":{l}")).collect();
            format!("{var}{}", label_str.join(""))
        }
        _ => "_expr".to_string(),
    }
}

/// Temporal + Duration arithmetic. Returns Some if handled, None to fall through.
fn eval_temporal_add(left: &Value, right: &Value) -> Option<crate::types::Result<Value>> {
    use crate::temporal::{
        CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime,
        CypherTime,
    };
    use chrono::{Months, NaiveDateTime};

    match (left, right) {
        (Value::Duration(a), Value::Duration(b)) => Some(Ok(Value::Duration(CypherDuration {
            months: a.months + b.months,
            days: a.days + b.days,
            seconds: a.seconds + b.seconds,
            nanos: a.nanos + b.nanos,
        }))),
        (Value::Date(d), Value::Duration(dur)) | (Value::Duration(dur), Value::Date(d)) => {
            let mut date = d.0;
            if dur.months != 0 {
                if dur.months > 0 {
                    date = date
                        .checked_add_months(Months::new(dur.months as u32))
                        .unwrap_or(date);
                } else {
                    date = date
                        .checked_sub_months(Months::new((-dur.months) as u32))
                        .unwrap_or(date);
                }
            }
            date += chrono::Duration::days(dur.days);
            date = date
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::Date(CypherDate(date))))
        }
        (Value::LocalTime(t), Value::Duration(dur))
        | (Value::Duration(dur), Value::LocalTime(t)) => {
            let time = t.0
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::LocalTime(CypherLocalTime(time))))
        }
        (Value::Time(t), Value::Duration(dur)) | (Value::Duration(dur), Value::Time(t)) => {
            let time = t.0
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::Time(CypherTime(time, t.1))))
        }
        (Value::LocalDateTime(dt), Value::Duration(dur))
        | (Value::Duration(dur), Value::LocalDateTime(dt)) => {
            let mut date = dt.0.date();
            if dur.months != 0 {
                if dur.months > 0 {
                    date = date
                        .checked_add_months(Months::new(dur.months as u32))
                        .unwrap_or(date);
                } else {
                    date = date
                        .checked_sub_months(Months::new((-dur.months) as u32))
                        .unwrap_or(date);
                }
            }
            date += chrono::Duration::days(dur.days);
            let ndt = NaiveDateTime::new(date, dt.0.time())
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::LocalDateTime(CypherLocalDateTime(ndt))))
        }
        (Value::DateTime(dt), Value::Duration(dur))
        | (Value::Duration(dur), Value::DateTime(dt)) => {
            let mut date = dt.0.date();
            if dur.months != 0 {
                if dur.months > 0 {
                    date = date
                        .checked_add_months(Months::new(dur.months as u32))
                        .unwrap_or(date);
                } else {
                    date = date
                        .checked_sub_months(Months::new((-dur.months) as u32))
                        .unwrap_or(date);
                }
            }
            date += chrono::Duration::days(dur.days);
            let ndt = NaiveDateTime::new(date, dt.0.time())
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::DateTime(CypherDateTime(ndt, dt.1, dt.2.clone()))))
        }
        _ => None,
    }
}

/// Temporal - Duration subtraction. Returns Some if handled.
fn eval_temporal_sub(left: &Value, right: &Value) -> Option<crate::types::Result<Value>> {
    use crate::temporal::CypherDuration;

    match (left, right) {
        (Value::Duration(a), Value::Duration(b)) => Some(Ok(Value::Duration(CypherDuration {
            months: a.months - b.months,
            days: a.days - b.days,
            seconds: a.seconds - b.seconds,
            nanos: a.nanos - b.nanos,
        }))),
        (_, Value::Duration(dur)) => {
            // Temporal - Duration → negate duration and add.
            let neg = CypherDuration {
                months: -dur.months,
                days: -dur.days,
                seconds: -dur.seconds,
                nanos: -dur.nanos,
            };
            eval_temporal_add(left, &Value::Duration(neg))
        }
        _ => None,
    }
}

/// Duration * Number. Returns Some if handled.
fn eval_duration_mul(left: &Value, right: &Value) -> Option<crate::types::Result<Value>> {
    use crate::temporal::CypherDuration;

    let (dur, n) = match (left, right) {
        (Value::Duration(d), Value::I64(n)) | (Value::I64(n), Value::Duration(d)) => (d, *n),
        (Value::Duration(d), Value::F64(n)) | (Value::F64(n), Value::Duration(d)) => {
            return Some(Ok(Value::Duration(CypherDuration {
                months: (d.months as f64 * n) as i64,
                days: (d.days as f64 * n) as i64,
                seconds: (d.seconds as f64 * n) as i64,
                nanos: (d.nanos as f64 * n) as i64,
            })));
        }
        _ => return None,
    };
    Some(Ok(Value::Duration(CypherDuration {
        months: dur.months * n,
        days: dur.days * n,
        seconds: dur.seconds * n,
        nanos: dur.nanos * n,
    })))
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
            "weekYear" => Some(Value::I64(d.0.iso_week().year() as i64)),
            "week" => Some(Value::I64(d.0.iso_week().week() as i64)),
            "dayOfWeek" | "weekDay" => {
                Some(Value::I64(d.0.weekday().num_days_from_monday() as i64 + 1))
            }
            "quarter" => Some(Value::I64(((d.0.month() - 1) / 3 + 1) as i64)),
            "dayOfQuarter" => {
                let q_start_month = ((d.0.month() - 1) / 3) * 3 + 1;
                let q_start =
                    chrono::NaiveDate::from_ymd_opt(d.0.year(), q_start_month, 1).unwrap();
                Some(Value::I64((d.0 - q_start).num_days() + 1))
            }
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
                "offset" | "timezone" => {
                    Some(Value::String(crate::temporal::fmt_offset_public(&ct.1)))
                }
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
            // DateTime-specific accessors first.
            match prop {
                "timezone" => {
                    // Return named tz if available, otherwise the offset string.
                    return Some(Value::String(
                        dt.2.clone()
                            .unwrap_or_else(|| crate::temporal::fmt_offset_public(&dt.1)),
                    ));
                }
                "epochSeconds" => {
                    let epoch = dt.0.and_utc().timestamp();
                    return Some(Value::I64(epoch));
                }
                "epochMillis" => {
                    let epoch = dt.0.and_utc().timestamp_millis();
                    return Some(Value::I64(epoch));
                }
                _ => {}
            }
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
            "quarters" => Some(Value::I64(d.months / 3)),
            "months" => Some(Value::I64(d.months)),
            "weeks" => Some(Value::I64(d.days / 7)),
            "days" => Some(Value::I64(d.days)),
            "hours" => Some(Value::I64(d.seconds / 3600)),
            "minutes" => Some(Value::I64(d.seconds / 60)),
            "seconds" => Some(Value::I64(d.seconds)),
            "nanoseconds" => Some(Value::I64(d.seconds * 1_000_000_000 + d.nanos)),
            "milliseconds" => Some(Value::I64(d.seconds * 1_000 + d.nanos / 1_000_000)),
            "microseconds" => Some(Value::I64(d.seconds * 1_000_000 + d.nanos / 1_000)),
            "quartersOfYear" => Some(Value::I64((d.months % 12) / 3)),
            "monthsOfQuarter" => Some(Value::I64(d.months % 3)),
            "monthsOfYear" => Some(Value::I64(d.months % 12)),
            "daysOfWeek" => Some(Value::I64(d.days % 7)),
            "minutesOfHour" => Some(Value::I64((d.seconds / 60) % 60)),
            "secondsOfMinute" => Some(Value::I64(d.seconds % 60)),
            "millisecondsOfSecond" => Some(Value::I64(d.nanos / 1_000_000)),
            "microsecondsOfSecond" => Some(Value::I64(d.nanos / 1_000)),
            "nanosecondsOfSecond" => Some(Value::I64(d.nanos)),
            _ => None,
        },
        _ => None,
    }
}
