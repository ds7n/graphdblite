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

/// Format a float for toString() output the way Neo4j (Java's
/// `Double.toString`) does: plain decimal notation with at least one
/// fractional digit for magnitudes in `[1e-3, 1e7)` (e.g. "3.0"), otherwise
/// scientific notation with an upper-case `E` (e.g. "1.0E20", "1.5E-7").
/// Non-finite values print as "NaN", "Infinity", "-Infinity".
pub(in crate::cypher::eval) fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let mag = f.abs();
    if mag == 0.0 || (1e-3..1e7).contains(&mag) {
        // Rust's `Display` is the shortest round-trip decimal form.
        let s = format!("{f}");
        return if s.contains('.') { s } else { format!("{s}.0") };
    }
    // `{:e}` gives the shortest round-trip mantissa, e.g. "1e20", "1.5e-7".
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    if mantissa.contains('.') {
        format!("{mantissa}E{exp}")
    } else {
        format!("{mantissa}.0E{exp}")
    }
}

/// Parse a string as a Cypher float. Rust's `f64::from_str` also accepts
/// "inf", "infinity" and "nan" in any case; Cypher (Java) only knows the
/// exact spellings "NaN" and "[+-]Infinity", so other non-finite results are
/// treated as unparseable.
fn parse_cypher_float(s: &str) -> Option<f64> {
    let f = s.trim().parse::<f64>().ok()?;
    if f.is_finite() {
        return Some(f);
    }
    matches!(s.trim(), "NaN" | "Infinity" | "+Infinity" | "-Infinity").then_some(f)
}

/// Rounding modes accepted by `round(value, precision, mode)` (the names of
/// Java's `RoundingMode`, which Neo4j uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cypher::eval) enum RoundMode {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
}

impl RoundMode {
    /// Parse a mode name (case-insensitive). `None` for unknown names.
    fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "UP" => Some(Self::Up),
            "DOWN" => Some(Self::Down),
            "CEILING" => Some(Self::Ceiling),
            "FLOOR" => Some(Self::Floor),
            "HALF_UP" => Some(Self::HalfUp),
            "HALF_DOWN" => Some(Self::HalfDown),
            "HALF_EVEN" => Some(Self::HalfEven),
            _ => None,
        }
    }
}

/// Round `value` to `precision` decimal places with `mode`, like Neo4j's
/// `round(value, precision, mode)` (Java `BigDecimal.valueOf(value)
/// .setScale(precision, mode)`).
///
/// Works on the shortest decimal representation of `value`, so
/// `round(2.675, 2)` is 2.68 even though the binary double is 2.67499...
/// A negative `precision` rounds to tens, hundreds, and so on.
pub(in crate::cypher::eval) fn round_decimal(value: f64, precision: i64, mode: RoundMode) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    // Beyond +-400 places every finite double behaves the same (all digits
    // kept, or all dropped), so clamping keeps the digit buffer small.
    let precision = precision.clamp(-400, 400);
    let negative = value < 0.0;
    // `Display` never uses exponent notation for f64.
    let text = format!("{}", value.abs());
    let (int_part, frac_part) = text.split_once('.').unwrap_or((&text, ""));
    let mut digits: Vec<u8> = int_part
        .bytes()
        .chain(frac_part.bytes())
        .map(|b| b - b'0')
        .collect();
    let mut int_len = int_part.len() as i64;
    // Number of leading digits to keep. Pad with leading zeros when the
    // rounding position lies left of the most significant digit.
    let mut keep = int_len + precision;
    if keep < 0 {
        let pad = (-keep) as usize;
        digits.splice(0..0, std::iter::repeat_n(0, pad));
        int_len += pad as i64;
        keep = 0;
    }
    let keep = keep as usize;
    if keep >= digits.len() {
        return value;
    }
    let dropped = &digits[keep..];
    let first = dropped[0];
    let rest_nonzero = dropped[1..].iter().any(|&d| d != 0);
    let any_dropped = first != 0 || rest_nonzero;
    let last_kept_odd = keep > 0 && digits[keep - 1] % 2 == 1;
    let round_away = match mode {
        RoundMode::Up => any_dropped,
        RoundMode::Down => false,
        RoundMode::Ceiling => any_dropped && !negative,
        RoundMode::Floor => any_dropped && negative,
        RoundMode::HalfUp => first >= 5,
        RoundMode::HalfDown => first > 5 || (first == 5 && rest_nonzero),
        RoundMode::HalfEven => first > 5 || (first == 5 && (rest_nonzero || last_kept_odd)),
    };
    let mut kept: Vec<u8> = digits[..keep].to_vec();
    if round_away {
        // Decimal increment with carry; a carry out of the top digit adds a
        // new leading 1 (e.g. 9.99 -> 10.0).
        let mut i = kept.len();
        loop {
            if i == 0 {
                kept.insert(0, 1);
                int_len += 1;
                break;
            }
            i -= 1;
            if kept[i] == 9 {
                kept[i] = 0;
            } else {
                kept[i] += 1;
                break;
            }
        }
    }
    // Rebuild "<int>.<frac>" from the kept digits; dropped integer positions
    // become zeros.
    let int_len = int_len as usize;
    let mut out = String::with_capacity(int_len + kept.len() + 2);
    for i in 0..int_len {
        out.push(char::from(b'0' + kept.get(i).copied().unwrap_or(0)));
    }
    if kept.len() > int_len {
        out.push('.');
        for d in &kept[int_len..] {
            out.push(char::from(b'0' + d));
        }
    }
    let magnitude: f64 = out.parse().unwrap_or(0.0);
    if negative {
        -magnitude
    } else {
        magnitude
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
                Some(Value::String(s)) => Ok(Value::I64(s.chars().count() as i64)),
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
                // NaN has no integer value; `as` would silently give 0.
                Value::F64(f) if f.is_nan() => Ok(Value::Null),
                Value::F64(f) => Ok(Value::I64(f as i64)),
                Value::String(s) => {
                    // Try integer first, then float (truncated).
                    if let Ok(n) = s.parse::<i64>() {
                        Ok(Value::I64(n))
                    } else {
                        match parse_cypher_float(&s) {
                            Some(f) if !f.is_nan() => Ok(Value::I64(f as i64)),
                            _ => Ok(Value::Null),
                        }
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
                Value::String(s) => Ok(parse_cypher_float(&s)
                    .map(Value::F64)
                    .unwrap_or(Value::Null)),
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
                Value::String(s) => Ok(Value::I64(s.chars().count() as i64)),
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
            let value = match arg {
                Value::I64(n) => n as f64,
                Value::F64(n) => n,
                _ => return Ok(Value::Null),
            };
            // round(value): nearest integer, ties away from zero.
            let Some(precision_expr) = args.get(1) else {
                return Ok(Value::F64(value.round()));
            };
            // round(value, precision [, mode]): HALF_UP unless a mode is given.
            let precision = match eval_expr(precision_expr, record, ecx)? {
                Value::I64(p) => p,
                Value::Null => return Ok(Value::Null),
                other => return Err(invalid_argument_type("round()", &other)),
            };
            let mode = match args.get(2).map(|a| eval_expr(a, record, ecx)).transpose()? {
                None => RoundMode::HalfUp,
                Some(Value::Null) => return Ok(Value::Null),
                Some(Value::String(name)) => RoundMode::parse(&name).ok_or_else(|| {
                    GraphError::invalid_argument_value(
                        QueryPhase::Runtime,
                        "round()",
                        format!(
                            "unknown rounding mode '{name}' (expected one of UP, DOWN, \
                             CEILING, FLOOR, HALF_UP, HALF_DOWN, HALF_EVEN)"
                        ),
                    )
                })?,
                Some(other) => return Err(invalid_argument_type("round()", &other)),
            };
            Ok(Value::F64(round_decimal(value, precision, mode)))
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
                    // Cypher substring is CHARACTER-indexed, not byte-indexed.
                    // Slicing by byte offset (`s[start..]`) panics when `start`
                    // falls inside a multibyte codepoint (e.g. substring('é', 1)),
                    // which would unwind across the FFI boundary. Operate on
                    // chars to stay panic-free and Unicode-correct.
                    let start = start.max(0) as usize;
                    let chars: Vec<char> = s.chars().collect();
                    if start >= chars.len() {
                        return Ok(Value::String(String::new()));
                    }
                    let out: String = match len {
                        Some(Value::I64(l)) => {
                            let take = l.max(0) as usize;
                            chars[start..].iter().take(take).collect()
                        }
                        // An explicit null length propagates like any null argument.
                        Some(Value::Null) => return Ok(Value::Null),
                        _ => chars[start..].iter().collect(),
                    };
                    Ok(Value::String(out))
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
                // An empty delimiter splits into single characters (Rust's
                // `split("")` would add empty strings at both ends).
                (Value::String(s), Some(Value::String(d))) if d.is_empty() => Ok(Value::List(
                    s.chars().map(|c| Value::String(c.to_string())).collect(),
                )),
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
            // `exists(pattern)` is the pattern predicate's own boolean;
            // `exists(n.prop)` asks whether the value is non-null.
            let is_pattern = args.first().is_some_and(|a| {
                matches!(
                    a.kind,
                    ExprKind::PatternPredicate(_)
                        | ExprKind::Exists { .. }
                        | ExprKind::ExistsSubquery(_)
                )
            });
            match arg {
                Value::Bool(b) if is_pattern => Ok(Value::Bool(b)),
                other => Ok(Value::Bool(other != Value::Null)),
            }
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
            // range(start, end [, step]); the sequence is validated and
            // size-checked before any element is built.
            match range::range_spec(args, record, ecx)? {
                Some(spec) => spec.into_list(),
                None => Ok(Value::Null),
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
        // `duration()` with no argument has no "current" value: an empty
        // map gives a zero duration.
        "duration" if args.is_empty() => {
            crate::temporal::CypherDuration::from_map(&Default::default()).map(Value::Duration)
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
                (Value::I64(s), Some(Value::I64(n))) => {
                    crate::temporal::CypherDateTime::from_epoch(s, n).map(Value::DateTime)
                }
                (Value::I64(s), None) => {
                    crate::temporal::CypherDateTime::from_epoch(s, 0).map(Value::DateTime)
                }
                _ => Ok(Value::Null),
            }
        }
        "datetime.fromepochmillis" => {
            let millis = eval_single_arg(args, record, ecx)?;
            match millis {
                Value::I64(ms) => {
                    crate::temporal::CypherDateTime::from_epoch_millis(ms).map(Value::DateTime)
                }
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
                "duration.between" => crate::temporal::duration_between(&lhs, &rhs)?,
                "duration.inmonths" => crate::temporal::duration_in_months(&lhs, &rhs)?,
                "duration.indays" => crate::temporal::duration_in_days(&lhs, &rhs)?,
                "duration.inseconds" => crate::temporal::duration_in_seconds(&lhs, &rhs)?,
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
            crate::temporal::truncate_date(&unit, &val, &map).map(Value::Date)
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
            crate::temporal::truncate_time(&unit, &val, &map).map(Value::LocalTime)
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
            let offset_seconds = if let Some(Value::String(tz_s)) = map.get("timezone") {
                crate::temporal::parse_offset_public(tz_s)?
            } else {
                // Inherit from source temporal.
                crate::temporal::extract_offset_secs(&val)
            };
            let time = crate::temporal::truncate_time(&unit, &val, &map)?;
            Ok(Value::Time(crate::temporal::CypherTime {
                time,
                offset_seconds,
            }))
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
            let date = crate::temporal::truncate_date(&unit, &val, &map)?;
            let time = crate::temporal::truncate_time(&unit, &val, &map)?;
            Ok(Value::LocalDateTime(crate::temporal::CypherLocalDateTime {
                date,
                time,
            }))
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
            let date = crate::temporal::truncate_date(&unit, &val, &map)?;
            let time = crate::temporal::truncate_time(&unit, &val, &map)?;
            let ndt = crate::temporal::CypherLocalDateTime { date, time };

            // Determine offset and timezone name.
            let (offset, tz_name) = if let Some(Value::String(tz_s)) = map.get("timezone") {
                // Check if it's an IANA name or an offset string.
                if tz_s.starts_with('+') || tz_s.starts_with('-') || tz_s == "Z" {
                    (crate::temporal::parse_offset_public(tz_s)?, None)
                } else {
                    // IANA timezone name: resolve at the truncated datetime.
                    let tz = crate::temporal::lookup_tz(tz_s)
                        .ok_or_else(|| GraphError::semantic("unknown timezone"))?;
                    let off = crate::temporal::earliest_offset(&tz, &ndt)?.ok_or_else(|| {
                        GraphError::semantic("ambiguous or invalid datetime in timezone")
                    })?;
                    (off, Some(tz_s.clone()))
                }
            } else {
                // Inherit offset from source temporal or default to UTC.
                match &val {
                    Value::DateTime(dt) => (dt.offset_seconds, dt.tz_name.clone()),
                    _ => (0, None),
                }
            };
            Ok(Value::DateTime(crate::temporal::CypherDateTime {
                local: ndt,
                offset_seconds: offset,
                tz_name,
            }))
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
        // `date()`, `datetime()`, ... with no argument: the statement's
        // current instant (UTC), projected like `date(datetime(...))`.
        let now = crate::temporal::CypherDateTime::from_utc_timestamp(super::statement_now());
        let mut m = std::collections::BTreeMap::new();
        m.insert("datetime".to_string(), Value::DateTime(now));
        return from_map(&m);
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
