mod column_name;
mod comparison;
mod comprehension;
mod functions;
mod subquery;
mod temporal_ops;

use std::cell::Cell;
use std::collections::HashMap;

use rusqlite::Connection;

use crate::cypher::ast::{BinOp, Expr, ExprKind};
use crate::cypher::record_view::RecordView;
use crate::types::{ErrorCode, GraphError, QueryPhase, Value};

// Thread-local pointer to the current query's `$param` map.
//
// Set by `ParamScope::enter` at the top of `crate::cypher::execute_cypher`
// and restored on drop. Read by `EvalCx::new` so every existing
// `EvalCx::new(conn)` call site picks up params without touching iter
// structs / executor helpers individually.
//
// Thread-locality is sound here because `rusqlite::Connection` is `!Send`:
// the entire executor call tree runs on the thread that called
// `Database::execute_*`. The pointer is non-null only while a `ParamScope`
// guard is alive on that thread's stack; on drop the previous value is
// restored, so nested `execute_cypher` calls (e.g. EXISTS subqueries) see
// each other's params correctly.
thread_local! {
    static CURRENT_PARAMS: Cell<*const HashMap<String, Value>> =
        const { Cell::new(std::ptr::null()) };
}

/// RAII guard that publishes a `&HashMap<String, Value>` to the thread-local
/// for the lifetime of the guard, restoring the previous value on drop.
pub(crate) struct ParamScope {
    prev: *const HashMap<String, Value>,
}

impl ParamScope {
    pub(crate) fn enter(params: Option<&HashMap<String, Value>>) -> Self {
        let new_ptr = params.map_or(std::ptr::null(), |p| p as *const _);
        let prev = CURRENT_PARAMS.with(|c| c.replace(new_ptr));
        Self { prev }
    }
}

impl Drop for ParamScope {
    fn drop(&mut self) {
        CURRENT_PARAMS.with(|c| c.set(self.prev));
    }
}

/// Evaluation context: shared inputs threaded through every `eval_*` call.
///
/// Bundles the SQLite connection (needed for EXISTS subqueries) and the
/// optional `$param` map (looked up by [`ExprKind::Parameter`] resolution).
/// Construct via [`EvalCx::new`] — the params map comes from the thread-local
/// published by [`ParamScope`].
#[derive(Clone, Copy)]
pub struct EvalCx<'a> {
    pub conn: &'a Connection,
    pub params: Option<&'a HashMap<String, Value>>,
}

impl<'a> EvalCx<'a> {
    /// Build a context. Looks up the active `$param` map from the thread-local
    /// published by [`ParamScope`]. When no scope is active (tests, ad-hoc
    /// callers), `Parameter` references error as `MissingParameter`.
    pub fn new(conn: &'a Connection) -> Self {
        let raw = CURRENT_PARAMS.with(|c| c.get());
        let params = if raw.is_null() {
            None
        } else {
            // SAFETY: `ParamScope` keeps the underlying `HashMap` alive for
            // the entire executor invocation. The lifetime `'a` is bounded by
            // `conn`, which is borrowed from the same `execute_cypher` stack
            // frame that holds the `ParamScope` — so the params reference is
            // valid for at least `'a`. The pointer is only non-null between
            // `ParamScope::enter` and its `Drop`.
            Some(unsafe { &*raw })
        };
        Self { conn, params }
    }

    /// Test-only constructor that explicitly carries a borrowed param map,
    /// bypassing the thread-local. Used in eval-level unit tests where no
    /// `ParamScope` is active.
    #[cfg(test)]
    pub fn with_params(conn: &'a Connection, params: Option<&'a HashMap<String, Value>>) -> Self {
        Self { conn, params }
    }
}

/// Look up a `$param` value via the active [`ParamScope`] thread-local.
/// Used by non-eval executor sites (e.g. `IndexLookup` value resolution)
/// that don't construct an [`EvalCx`] but still need parameter values.
/// Returns `None` if no scope is active or the name is absent.
pub(crate) fn lookup_param(name: &str) -> Option<Value> {
    CURRENT_PARAMS.with(|c| {
        let raw = c.get();
        if raw.is_null() {
            None
        } else {
            // SAFETY: identical to `EvalCx::new` — the pointer is valid for
            // the lifetime of the surrounding `ParamScope` guard, which
            // brackets the entire executor invocation.
            unsafe { (*raw).get(name).cloned() }
        }
    })
}

use comparison::{compare_to_value, literal_to_value, to_tribool, value_type_name, values_equal};
use temporal_ops::{
    eval_duration_div, eval_duration_mul, eval_temporal_add, eval_temporal_sub, temporal_accessor,
};

// Functions that moved out of mod.rs in the eval split — sibling
// submodules expose them with `pub(in crate::cypher::eval)` visibility.
use comprehension::{eval_list_comprehension, eval_pattern_comprehension, eval_quantifier};
use functions::eval_function_call;
use subquery::{eval_exists, eval_exists_subquery};

pub use column_name::expr_to_column_name;

/// All scalar and aggregate function names dispatched by the Cypher evaluator,
/// in lower-case. Used for compile-time typo detection in the planner.
pub const KNOWN_FUNCTION_NAMES: &[&str] = &[
    // Scalar functions (eval_function_call match arms)
    "length",
    "nodes",
    "tolower",
    "toupper",
    "tostring",
    "toboolean",
    "tointeger",
    "tofloat",
    "keys",
    "labels",
    "id",
    "type",
    "properties",
    "relationships",
    "coalesce",
    "head",
    "last",
    "tail",
    "size",
    "abs",
    "sqrt",
    "sign",
    "ceil",
    "floor",
    "round",
    "log",
    "log10",
    "exp",
    "e",
    "pi",
    "substring",
    "replace",
    "split",
    "trim",
    "ltrim",
    "rtrim",
    "left",
    "right",
    "startnode",
    "endnode",
    "exists",
    "reverse",
    "range",
    "rand",
    // Temporal constructors and statement-time variants
    "date",
    "date.transaction",
    "date.statement",
    "date.realtime",
    "localtime",
    "localtime.transaction",
    "localtime.statement",
    "localtime.realtime",
    "time",
    "time.transaction",
    "time.statement",
    "time.realtime",
    "localdatetime",
    "localdatetime.transaction",
    "localdatetime.statement",
    "localdatetime.realtime",
    "datetime",
    "datetime.transaction",
    "datetime.statement",
    "datetime.realtime",
    "duration",
    "datetime.fromepoch",
    "datetime.fromepochmillis",
    "duration.between",
    "duration.inmonths",
    "duration.indays",
    "duration.inseconds",
    "date.truncate",
    "localtime.truncate",
    "time.truncate",
    "localdatetime.truncate",
    "datetime.truncate",
    // Aggregate functions (handled by Aggregate operator, but recognized here)
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "collect",
    "percentiledisc",
    "percentilecont",
    "stdev",
    "stdevp",
];

/// Returns true if `name` is a recognized Cypher function (scalar or aggregate).
/// Comparison is case-insensitive.
pub fn is_known_function(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    KNOWN_FUNCTION_NAMES.iter().any(|n| *n == lc)
}

/// Evaluate an expression against a record, producing a Value.
pub fn eval_expr(
    expr: &Expr,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<Value> {
    let conn = ecx.conn;
    match &expr.kind {
        ExprKind::Literal(lit) => Ok(literal_to_value(lit)),
        ExprKind::Variable(name) => {
            // Try to reconstruct a full Node/Edge value from compound bindings
            // (e.g. n.__id, n.__label, n.prop) so that variables resolve to rich
            // objects when used in RETURN lists, maps, or comparisons.
            if let Some(compound) = crate::cypher::executor::build_compound_binding(record, name) {
                Ok(compound)
            } else {
                Ok(record.get(name).cloned().unwrap_or(Value::Null))
            }
        }
        ExprKind::Property(var, prop) => {
            // Check if the entity has been deleted (e.g. DELETE n RETURN n.prop).
            if record.get(&format!("{var}.__deleted")) == Some(&Value::Bool(true)) {
                return Err(GraphError::Query(crate::types::QueryError::EntityNotFound {
                    phase: crate::types::QueryPhase::Runtime,
                    message: format!(
                        "DeletedEntityAccess: cannot access property `{prop}` on deleted entity `{var}`"
                    ),
                    code: ErrorCode::Other,
                    hint: None,
                    span: None,
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
            // Node property access: when variable is bound to a Value::Node directly
            // (e.g. from quantifier iterating over nodes(p) list).
            if let Some(Value::Node(node)) = record.get(var) {
                if prop == "labels" {
                    return Ok(Value::List(
                        node.labels
                            .iter()
                            .map(|l| Value::String(l.clone()))
                            .collect(),
                    ));
                }
                return Ok(node.properties.get(prop).cloned().unwrap_or(Value::Null));
            }
            // Edge property access: when variable is bound to a Value::Edge directly.
            if let Some(Value::Edge(edge)) = record.get(var) {
                return Ok(edge.properties.get(prop).cloned().unwrap_or(Value::Null));
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
                                format!("property access on {}", value_type_name(val)),
                            )
                            .with_code(ErrorCode::InvalidArgumentType));
                        }
                        _ => {}
                    }
                }
            }
            Ok(Value::Null)
        }
        ExprKind::List(items) => {
            let values: crate::types::Result<Vec<Value>> =
                items.iter().map(|e| eval_expr(e, record, ecx)).collect();
            Ok(Value::List(values?))
        }
        ExprKind::Index { expr, index } => {
            // If the base is a variable, first try to build a compound binding
            // for dynamic property access (n['name']).
            if let ExprKind::Variable(var) = &expr.as_ref().kind {
                let idx_val = eval_expr(index, record, ecx)?;
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
            let base = eval_expr(expr, record, ecx)?;
            let idx = eval_expr(index, record, ecx)?;
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
                    "cannot index a non-list value".to_string(),
                )
                .with_code(ErrorCode::InvalidArgumentType)),
                // Indexing a list with a non-integer.
                (Value::List(_), _) => Err(GraphError::type_error(
                    crate::types::QueryPhase::Runtime,
                    "list index must be an integer".to_string(),
                )
                .with_code(ErrorCode::InvalidArgumentType)),
                // Indexing a map with a non-string.
                (Value::Map(_), _) | (Value::Node(_), _) | (Value::Edge(_), _) => {
                    Err(GraphError::type_error(
                        crate::types::QueryPhase::Runtime,
                        "map index must be a string".to_string(),
                    )
                    .with_code(ErrorCode::MapElementAccessByNonString))
                }
                _ => Ok(Value::Null),
            }
        }
        ExprKind::DotAccess { expr, key } => {
            let base = eval_expr(expr, record, ecx)?;
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
        ExprKind::Slice { expr, start, end } => {
            let base = eval_expr(expr, record, ecx)?;
            let start_val = start
                .as_ref()
                .map(|e| eval_expr(e, record, ecx))
                .transpose()?;
            let end_val = end
                .as_ref()
                .map(|e| eval_expr(e, record, ecx))
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
        ExprKind::HasLabel(var, labels) => {
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
                // Check if var is an edge binding — compare __type against labels.
                let type_key = format!("{var}.__type");
                if let Some(Value::String(rel_type)) = record.get(&type_key) {
                    let has_all = labels.iter().all(|lbl| lbl == rel_type);
                    return Ok(Value::Bool(has_all));
                }
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
        ExprKind::Star => Ok(Value::Null),
        ExprKind::Parameter(name) => match ecx.params.and_then(|p| p.get(name)) {
            Some(v) => Ok(v.clone()),
            None => Err(crate::types::GraphError::Query(
                crate::types::QueryError::ArgumentError {
                    phase: crate::types::QueryPhase::Runtime,
                    code: crate::types::ErrorCode::MissingParameter,
                    message: format!("unresolved parameter: ${name}"),
                    hint: Some(format!(
                        "pass `{name}` via execute_with_params or set it before running the query"
                    )),
                    span: None,
                },
            )),
        },
        ExprKind::BinaryOp { left, op, right } => {
            let lval = eval_expr(left, record, ecx)?;
            let rval = eval_expr(right, record, ecx)?;
            eval_binop(&lval, *op, &rval)
        }
        ExprKind::Not(inner) => {
            let val = eval_expr(inner, record, ecx)?;
            match to_tribool(&val)? {
                Some(b) => Ok(Value::Bool(!b)),
                None => Ok(Value::Null),
            }
        }
        ExprKind::IsNull(inner) => {
            let val = eval_expr(inner, record, ecx)?;
            Ok(Value::Bool(matches!(val, Value::Null)))
        }
        ExprKind::IsNotNull(inner) => {
            let val = eval_expr(inner, record, ecx)?;
            Ok(Value::Bool(!matches!(val, Value::Null)))
        }
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            if let Some(op_expr) = operand {
                // Simple CASE: CASE operand WHEN value THEN result ...
                let op_val = eval_expr(op_expr, record, ecx)?;
                for (when_val_expr, result) in alternatives {
                    let when_val = eval_expr(when_val_expr, record, ecx)?;
                    if values_equal(&op_val, &when_val) == Value::Bool(true) {
                        return eval_expr(result, record, ecx);
                    }
                }
            } else {
                // Searched CASE: CASE WHEN cond THEN result ...
                for (cond, result) in alternatives {
                    if eval_predicate(cond, record, ecx)? {
                        return eval_expr(result, record, ecx);
                    }
                }
            }
            match default {
                Some(expr) => eval_expr(expr, record, ecx),
                None => Ok(Value::Null),
            }
        }
        ExprKind::ListComprehension {
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
            ecx,
        ),
        ExprKind::Quantifier {
            kind,
            variable,
            list_expr,
            predicate,
        } => eval_quantifier(*kind, variable, list_expr, predicate, record, ecx),
        ExprKind::PatternComprehension {
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
            ecx,
        ),
        ExprKind::Exists {
            patterns,
            where_clause,
        } => eval_exists(patterns, where_clause.as_deref(), record, ecx),
        ExprKind::PatternPredicate(pattern) => {
            eval_exists(std::slice::from_ref(pattern), None, record, ecx)
        }
        ExprKind::ExistsSubquery(stmt) => eval_exists_subquery(stmt, record, ecx),
        ExprKind::MapLiteral(pairs) => {
            let mut map = std::collections::BTreeMap::new();
            for (k, expr) in pairs {
                let val = eval_expr(expr, record, ecx)?;
                map.insert(k.clone(), val);
            }
            Ok(Value::Map(map))
        }
        ExprKind::FunctionCall {
            name,
            args,
            original_text,
            ..
        } => eval_function_call(name, args, original_text.as_deref(), record, ecx),
    }
}

/// Evaluate a boolean expression, returning true/false.
pub fn eval_predicate(
    expr: &Expr,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<bool> {
    let val = eval_expr(expr, record, ecx)?;
    Ok(matches!(val, Value::Bool(true)))
}

pub(in crate::cypher::eval) fn eval_binop(
    left: &Value,
    op: BinOp,
    right: &Value,
) -> crate::types::Result<Value> {
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
                    "IN requires a list on the right side, got {}",
                    value_type_name(rhs)
                ),
            )
            .with_code(ErrorCode::InvalidArgumentType)),
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
                    _ => eval_arithmetic(left, right, "+", i64::checked_add, |a, b| a + b),
                }
            }
        }
        BinOp::Sub => {
            if let Some(result) = eval_temporal_sub(left, right) {
                result
            } else {
                eval_arithmetic(left, right, "-", i64::checked_sub, |a, b| a - b)
            }
        }
        BinOp::Mul => {
            // Duration * Number
            if let Some(result) = eval_duration_mul(left, right) {
                result
            } else {
                eval_arithmetic(left, right, "*", i64::checked_mul, |a, b| a * b)
            }
        }
        BinOp::Div => {
            // Duration / Number — flatten to total nanos, divide, decompose.
            if let Some(result) = eval_duration_div(left, right) {
                return result;
            }
            // Division by zero → Null (Cypher semantics).
            match (left, right) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::I64(a), Value::I64(b)) => {
                    if *b == 0 {
                        Ok(Value::Null)
                    } else {
                        a.checked_div(*b).map(Value::I64).ok_or_else(|| {
                            GraphError::number_out_of_range(
                                QueryPhase::Runtime,
                                format!("integer overflow: {a} / {b}"),
                            )
                        })
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
                _ => Err(arithmetic_type_error("/", left, right)),
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
                        a.checked_rem(*b).map(Value::I64).ok_or_else(|| {
                            GraphError::number_out_of_range(
                                QueryPhase::Runtime,
                                format!("integer overflow: {a} % {b}"),
                            )
                        })
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
                _ => Err(arithmetic_type_error("%", left, right)),
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
                _ => Err(arithmetic_type_error("^", left, right)),
            }
        }
    }
}

/// Evaluate an arithmetic binary operation with numeric coercion.
///
/// Integer ops use checked arithmetic and return `NumberOutOfRange` on overflow.
pub(in crate::cypher::eval) fn eval_arithmetic(
    left: &Value,
    right: &Value,
    op_name: &str,
    int_op: impl Fn(i64, i64) -> Option<i64>,
    float_op: impl Fn(f64, f64) -> f64,
) -> crate::types::Result<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::I64(a), Value::I64(b)) => int_op(*a, *b).map(Value::I64).ok_or_else(|| {
            GraphError::number_out_of_range(
                QueryPhase::Runtime,
                format!("integer overflow: {a} {op_name} {b}"),
            )
        }),
        (Value::F64(a), Value::F64(b)) => Ok(Value::F64(float_op(*a, *b))),
        (Value::I64(a), Value::F64(b)) => Ok(Value::F64(float_op(*a as f64, *b))),
        (Value::F64(a), Value::I64(b)) => Ok(Value::F64(float_op(*a, *b as f64))),
        // String concatenation with +.
        (Value::String(a), Value::String(b)) => Ok(Value::String(format!("{a}{b}"))),
        _ => Err(arithmetic_type_error(op_name, left, right)),
    }
}

/// Build a TypeError for an arithmetic op applied to incompatible non-null operands.
pub(in crate::cypher::eval) fn arithmetic_type_error(
    op_name: &str,
    left: &Value,
    right: &Value,
) -> GraphError {
    GraphError::type_error(
        QueryPhase::Runtime,
        format!(
            "Type mismatch: cannot apply `{op_name}` to {} and {}",
            value_type_name(left),
            value_type_name(right)
        ),
    )
    .with_code(ErrorCode::InvalidArgumentType)
}

/// Return operator precedence (higher = binds tighter).
pub(in crate::cypher::eval) fn binop_precedence(op: &BinOp) -> u8 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{Expr, ExprKind};
    use crate::cypher::record::NamedRecord;

    #[test]
    fn parameter_resolves_from_evalcx_params() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let rec = NamedRecord::new();
        let expr = Expr::synthetic(ExprKind::Parameter("x".to_string()));

        let mut params = HashMap::new();
        params.insert("x".to_string(), Value::I64(42));

        let ecx = EvalCx::with_params(&conn, Some(&params));
        let val = eval_expr(&expr, &rec, ecx).unwrap();
        assert_eq!(val, Value::I64(42));
    }

    #[test]
    fn parameter_missing_errors_with_missing_parameter_code() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let rec = NamedRecord::new();
        let expr = Expr::synthetic(ExprKind::Parameter("missing".to_string()));

        let ecx = EvalCx::new(&conn);
        let err = eval_expr(&expr, &rec, ecx).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("MissingParameter"), "got: {msg}");
        assert!(msg.contains("missing"), "got: {msg}");
    }

    #[test]
    fn parameter_with_empty_params_map_errors() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let rec = NamedRecord::new();
        let expr = Expr::synthetic(ExprKind::Parameter("y".to_string()));

        let empty: HashMap<String, Value> = HashMap::new();
        let ecx = EvalCx::with_params(&conn, Some(&empty));
        assert!(eval_expr(&expr, &rec, ecx).is_err());
    }
}
