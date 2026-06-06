//! Function-call dispatch (scalar + temporal constructors).

use crate::cypher::ast::*;
use crate::cypher::record_view::RecordView;
use crate::types::{ErrorCode, GraphError, QueryError, QueryPhase, Value};

use super::column_name::*;
use super::*;

/// Evaluate the first argument of a function call.
pub(in crate::cypher::eval) fn eval_single_arg(
    args: &[Expr],
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<Value> {
    args.first()
        .map(|a| eval_expr(a, record, ecx))
        .transpose()
        .map(|v| v.unwrap_or(Value::Null))
}

/// Build a TypeError for invalid argument types passed to type conversion functions.
pub(in crate::cypher::eval) fn invalid_argument_type(func_name: &str, value: &Value) -> GraphError {
    GraphError::Query(QueryError::TypeError {
        phase: QueryPhase::Runtime,
        message: format!(
            "{func_name}: invalid argument type {}",
            value_type_name(value)
        ),
        code: ErrorCode::Other,
        hint: None,
        span: None,
    })
}

/// Format a float for toString() output. Uses decimal notation without
/// trailing zeros, but always keeps at least one decimal digit (e.g. "3.0").
pub(in crate::cypher::eval) fn format_float(f: f64) -> String {
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
pub(in crate::cypher::eval) fn eval_function_call(
    name: &str,
    args: &[Expr],
    original_text: Option<&str>,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<Value> {
    let conn = ecx.conn;
    let name_lower = name.to_ascii_lowercase();

    // Check for deleted entity access in function arguments.
    // Note: type() and id() are allowed on deleted entities per openCypher spec.
    if matches!(name_lower.as_str(), "labels" | "keys" | "properties") {
        if let Some(Expr {
            kind: ExprKind::Variable(var),
            ..
        }) = args.first()
        {
            if record.get(&format!("{var}.__deleted")) == Some(&Value::Bool(true)) {
                return Err(GraphError::Query(
                    crate::types::QueryError::EntityNotFound {
                        phase: crate::types::QueryPhase::Runtime,
                        message: format!(
                            "DeletedEntityAccess: cannot call {name}() on deleted entity `{var}`"
                        ),
                        code: ErrorCode::Other,
                        hint: None,
                        span: None,
                    },
                ));
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
        let col = expr_to_column_name(&Expr::synthetic(ExprKind::FunctionCall {
            name: name.to_string(),
            args: args.to_vec(),
            distinct: false,
            original_text: None,
        }));
        if let Some(val) = record.get(&col) {
            return Ok(val.clone());
        }
        // Also try lookup with original_text — the aggregate operator stores
        // results under the original parsed text which may differ from the
        // reconstructed column name (e.g. extra parens in expressions).
        if let Some(orig) = original_text {
            if let Some(val) = record.get(orig) {
                return Ok(val.clone());
            }
        }
    }

    match name_lower.as_str() {
        "length" => {
            let arg = args
                .first()
                .map(|a| eval_expr(a, record, ecx))
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
                .map(|a| eval_expr(a, record, ecx))
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
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.to_lowercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "toupper" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.to_uppercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "tostring" => {
            let arg = eval_single_arg(args, record, ecx)?;
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
            let arg = eval_single_arg(args, record, ecx)?;
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
            let arg = eval_single_arg(args, record, ecx)?;
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
            let arg = eval_single_arg(args, record, ecx)?;
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
            if let Some(Expr {
                kind: ExprKind::Variable(var),
                ..
            }) = args.first()
            {
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
            let arg = eval_single_arg(args, record, ecx)?;
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
            if let Some(Expr {
                kind: ExprKind::Variable(var),
                ..
            }) = args.first()
            {
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
            let arg = eval_single_arg(args, record, ecx)?;
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
            if let Some(Expr {
                kind: ExprKind::Variable(var),
                ..
            }) = args.first()
            {
                let id_key = format!("{var}.__id");
                if let Some(val @ Value::I64(_)) = record.get(&id_key) {
                    return Ok(val.clone());
                }
            }
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(_) => Ok(arg),
                Value::Node(n) => Ok(Value::I64(n.id.0 as i64)),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "score" => {
            // score(n) — BM25 score for FTS-bound nodes. Planner validates
            // arity == 1 and arg is a Variable at plan time; the runtime
            // guards are defensive: return NULL for malformed shapes rather
            // than evaluating the arg (a literal/property/function-call has
            // no `__fts_score` flat key by construction).
            if args.len() != 1 {
                return Ok(Value::Null);
            }
            let ExprKind::Variable(var_name) = &args[0].kind else {
                return Ok(Value::Null);
            };
            let key = format!("{var_name}.__fts_score");
            Ok(record.get(&key).cloned().unwrap_or(Value::Null))
        }
        "type" => {
            // type(r) — extract the relationship type from a binding.
            if let Some(Expr {
                kind: ExprKind::Variable(var),
                ..
            }) = args.first()
            {
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
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::Edge(e) => Ok(Value::String(e.label)),
                Value::Null => Ok(Value::Null),
                other => Err(invalid_argument_type("type()", &other)),
            }
        }
        "properties" => {
            // properties(n) — return a map of all properties on a node/edge/map.
            if let Some(Expr {
                kind: ExprKind::Variable(var),
                ..
            }) = args.first()
            {
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
            let arg = eval_single_arg(args, record, ecx)?;
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
                .map(|a| eval_expr(a, record, ecx))
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
                let val = eval_expr(a, record, ecx)?;
                if val != Value::Null {
                    return Ok(val);
                }
            }
            Ok(Value::Null)
        }
        "head" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::List(items) => Ok(items.into_iter().next().unwrap_or(Value::Null)),
                _ => Ok(Value::Null),
            }
        }
        "last" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::List(items) => Ok(items.into_iter().last().unwrap_or(Value::Null)),
                _ => Ok(Value::Null),
            }
        }
        "tail" => {
            let arg = eval_single_arg(args, record, ecx)?;
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
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::List(items) => Ok(Value::I64(items.len() as i64)),
                Value::String(s) => Ok(Value::I64(s.len() as i64)),
                _ => Ok(Value::Null),
            }
        }
        "abs" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(n) => n.checked_abs().map(Value::I64).ok_or_else(|| {
                    GraphError::number_out_of_range(
                        QueryPhase::Runtime,
                        format!("integer overflow: abs({n})"),
                    )
                }),
                Value::F64(n) => Ok(Value::F64(n.abs())),
                _ => Ok(Value::Null),
            }
        }
        "sqrt" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(n) => Ok(Value::F64((n as f64).sqrt())),
                Value::F64(n) => Ok(Value::F64(n.sqrt())),
                Value::Null => Ok(Value::Null),
                other => Err(invalid_argument_type("sqrt()", &other)),
            }
        }
        "sign" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(n) => Ok(Value::I64(n.signum())),
                Value::F64(n) => Ok(Value::I64(if n.is_nan() { 0 } else { n.signum() as i64 })),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "ceil" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(n) => Ok(Value::F64(n as f64)),
                Value::F64(n) => Ok(Value::F64(n.ceil())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "floor" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(n) => Ok(Value::F64(n as f64)),
                Value::F64(n) => Ok(Value::F64(n.floor())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "round" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(n) => Ok(Value::F64(n as f64)),
                Value::F64(n) => Ok(Value::F64(n.round())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "log" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(n) => Ok(Value::F64((n as f64).ln())),
                Value::F64(n) => Ok(Value::F64(n.ln())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "log10" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::I64(n) => Ok(Value::F64((n as f64).log10())),
                Value::F64(n) => Ok(Value::F64(n.log10())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "exp" => {
            let arg = eval_single_arg(args, record, ecx)?;
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
            let s = eval_single_arg(args, record, ecx)?;
            let start = args.get(1).map(|a| eval_expr(a, record, ecx)).transpose()?;
            let len = args.get(2).map(|a| eval_expr(a, record, ecx)).transpose()?;
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
            let s = eval_single_arg(args, record, ecx)?;
            let search = args.get(1).map(|a| eval_expr(a, record, ecx)).transpose()?;
            let replacement = args.get(2).map(|a| eval_expr(a, record, ecx)).transpose()?;
            match (s, search, replacement) {
                (Value::String(s), Some(Value::String(search)), Some(Value::String(repl))) => {
                    Ok(Value::String(s.replace(&search, &repl)))
                }
                _ => Ok(Value::Null),
            }
        }
        "split" => {
            // split(s, delimiter)
            let s = eval_single_arg(args, record, ecx)?;
            let delim = args.get(1).map(|a| eval_expr(a, record, ecx)).transpose()?;
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
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.trim().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "lTrim" | "ltrim" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.trim_start().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "rTrim" | "rtrim" => {
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::String(s) => Ok(Value::String(s.trim_end().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "left" => {
            let s = eval_single_arg(args, record, ecx)?;
            let len_val = args.get(1).map(|a| eval_expr(a, record, ecx)).transpose()?;
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
            let s = eval_single_arg(args, record, ecx)?;
            let len_val = args.get(1).map(|a| eval_expr(a, record, ecx)).transpose()?;
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
            // startNode(r) — get the start node of a relationship.
            // First try the argument as a compound edge binding.
            if let Some(Expr {
                kind: ExprKind::Variable(var),
                ..
            }) = args.first()
            {
                let src_key = format!("{var}.__src");
                if let Some(Value::I64(src_id)) = record.get(&src_key) {
                    let node_id = crate::types::NodeId(*src_id as u64);
                    if let Ok(n) = crate::node::get_node(conn, node_id) {
                        return Ok(Value::Node(n));
                    }
                }
            }
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::Edge(e) => match crate::node::get_node(conn, e.src) {
                    Ok(n) => Ok(Value::Node(n)),
                    Err(_) => Ok(Value::Null),
                },
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "endNode" | "endnode" => {
            // endNode(r) — get the end node of a relationship.
            if let Some(Expr {
                kind: ExprKind::Variable(var),
                ..
            }) = args.first()
            {
                let dst_key = format!("{var}.__dst");
                if let Some(Value::I64(dst_id)) = record.get(&dst_key) {
                    let node_id = crate::types::NodeId(*dst_id as u64);
                    if let Ok(n) = crate::node::get_node(conn, node_id) {
                        return Ok(Value::Node(n));
                    }
                }
            }
            let arg = eval_single_arg(args, record, ecx)?;
            match arg {
                Value::Edge(e) => match crate::node::get_node(conn, e.dst) {
                    Ok(n) => Ok(Value::Node(n)),
                    Err(_) => Ok(Value::Null),
                },
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "exists" => {
            let arg = eval_single_arg(args, record, ecx)?;
            Ok(Value::Bool(arg != Value::Null))
        }
        "reverse" => {
            let arg = eval_single_arg(args, record, ecx)?;
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
            let start = eval_single_arg(args, record, ecx)?;
            let end = args.get(1).map(|a| eval_expr(a, record, ecx)).transpose()?;
            let step = args.get(2).map(|a| eval_expr(a, record, ecx)).transpose()?;
            // Validate argument types.
            for (label, val) in [("start", &start)]
                .into_iter()
                .chain(end.as_ref().map(|v| ("end", v)))
                .chain(step.as_ref().map(|v| ("step", v)))
            {
                if !matches!(val, Value::I64(_) | Value::Null) {
                    return Err(GraphError::Query(QueryError::ArgumentError {
                        phase: QueryPhase::Runtime,
                        message: format!(
                            "InvalidArgumentType: range() {label} argument must be an integer, got {}",
                            value_type_name(val)
                        ),
                        code: ErrorCode::Other,
                        hint: None,
                        span: None,
                    }));
                }
            }
            match (start, end) {
                (Value::I64(s), Some(Value::I64(e))) => {
                    let step = match step {
                        Some(Value::I64(st)) => {
                            if st == 0 {
                                return Err(GraphError::Query(QueryError::ArgumentError {
                                    phase: QueryPhase::Runtime,
                                    message:
                                        "NumberOutOfRange: step argument to range() cannot be zero"
                                            .to_string(),
                                    code: ErrorCode::Other,
                                    hint: None,
                                    span: None,
                                }));
                            }
                            st
                        }
                        _ => 1,
                    };
                    // Pre-compute expected length in i128 to reject huge
                    // allocations up front (security finding H2). Cap at a
                    // generous 10M entries — a real query producing more is
                    // almost certainly a mistake or an attack.
                    const MAX_RANGE_LEN: i128 = 10_000_000;
                    let len: i128 = if (step > 0 && s <= e) || (step < 0 && s >= e) {
                        let span = (e as i128) - (s as i128);
                        span / (step as i128) + 1
                    } else {
                        0
                    };
                    if len > MAX_RANGE_LEN {
                        return Err(GraphError::Query(QueryError::ArgumentError {
                            phase: QueryPhase::Runtime,
                            message: format!(
                                "NumberOutOfRange: range({s}, {e}, {step}) would produce \
                                 {len} elements, exceeding the {MAX_RANGE_LEN}-element cap"
                            ),
                            code: ErrorCode::NumberOutOfRange,
                            hint: None,
                            span: None,
                        }));
                    }
                    let mut result = Vec::with_capacity(len as usize);
                    let mut i = s;
                    if (step > 0 && s <= e) || (step < 0 && s >= e) {
                        if step > 0 {
                            while i <= e {
                                result.push(Value::I64(i));
                                match i.checked_add(step) {
                                    Some(n) => i = n,
                                    None => break,
                                }
                            }
                        } else {
                            while i >= e {
                                result.push(Value::I64(i));
                                match i.checked_add(step) {
                                    Some(n) => i = n,
                                    None => break,
                                }
                            }
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
                ecx,
                |s| crate::temporal::CypherDate::from_iso_string(s).map(Value::Date),
                |m| crate::temporal::CypherDate::from_map(m).map(Value::Date),
            )
        }
        "localtime" | "localtime.transaction" | "localtime.statement" | "localtime.realtime" => {
            eval_temporal_constructor(
                args,
                record,
                ecx,
                |s| crate::temporal::CypherLocalTime::from_iso_string(s).map(Value::LocalTime),
                |m| crate::temporal::CypherLocalTime::from_map(m).map(Value::LocalTime),
            )
        }
        "time" | "time.transaction" | "time.statement" | "time.realtime" => {
            eval_temporal_constructor(
                args,
                record,
                ecx,
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
            ecx,
            |s| crate::temporal::CypherLocalDateTime::from_iso_string(s).map(Value::LocalDateTime),
            |m| crate::temporal::CypherLocalDateTime::from_map(m).map(Value::LocalDateTime),
        ),
        "datetime" | "datetime.transaction" | "datetime.statement" | "datetime.realtime" => {
            eval_temporal_constructor(
                args,
                record,
                ecx,
                |s| crate::temporal::CypherDateTime::from_iso_string(s).map(Value::DateTime),
                |m| crate::temporal::CypherDateTime::from_map(m).map(Value::DateTime),
            )
        }
        "duration" => eval_temporal_constructor(
            args,
            record,
            ecx,
            |s| crate::temporal::CypherDuration::from_iso_string(s).map(Value::Duration),
            |m| crate::temporal::CypherDuration::from_map(m).map(Value::Duration),
        ),
        "datetime.fromepoch" => {
            let secs = eval_single_arg(args, record, ecx)?;
            let nanos = args.get(1).map(|a| eval_expr(a, record, ecx)).transpose()?;
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
            let millis = eval_single_arg(args, record, ecx)?;
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
            let lhs = eval_expr(&args[0], record, ecx)?;
            let rhs = eval_expr(&args[1], record, ecx)?;
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
            let unit = match eval_expr(&args[0], record, ecx)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, ecx)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, ecx)? {
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
            let unit = match eval_expr(&args[0], record, ecx)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, ecx)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, ecx)? {
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
            let unit = match eval_expr(&args[0], record, ecx)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, ecx)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, ecx)? {
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
            let unit = match eval_expr(&args[0], record, ecx)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, ecx)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, ecx)? {
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
            let unit = match eval_expr(&args[0], record, ecx)? {
                Value::String(s) => s,
                _ => return Ok(Value::Null),
            };
            let val = eval_expr(&args[1], record, ecx)?;
            let map = if args.len() > 2 {
                match eval_expr(&args[2], record, ecx)? {
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
                    let tz: Tz = tz_s
                        .parse()
                        .map_err(|_| GraphError::semantic(format!("unknown timezone: {tz_s}")))?;
                    let aware = tz.from_local_datetime(&ndt).earliest().ok_or_else(|| {
                        GraphError::semantic(format!(
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
pub(in crate::cypher::eval) fn eval_temporal_constructor(
    args: &[Expr],
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
    from_str: impl Fn(&str) -> crate::types::Result<Value>,
    from_map: impl Fn(&std::collections::BTreeMap<String, Value>) -> crate::types::Result<Value>,
) -> crate::types::Result<Value> {
    if args.is_empty() {
        return from_map(&std::collections::BTreeMap::new());
    }
    let arg = eval_single_arg(args, record, ecx)?;
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
