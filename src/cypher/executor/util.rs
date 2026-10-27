//! Shared helpers — value type ranking, sort comparison, agg name/column mapping, public correlated wrappers, node→record helpers.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::expr_to_column_name;
use crate::cypher::record::NamedRecord;
use crate::node;
use crate::types::*;

use super::*;

pub(crate) fn literal_to_value(lit: &LiteralValue) -> Value {
    match lit {
        LiteralValue::Null => Value::Null,
        LiteralValue::Bool(b) => Value::Bool(*b),
        LiteralValue::I64(n) => Value::I64(*n),
        LiteralValue::F64(n) => Value::F64(*n),
        LiteralValue::String(s) => Value::String(s.clone()),
    }
}

/// Resolve an [`IndexLookup`] key to a concrete `Value`. `Literal` keys
/// inline; `Param(name)` keys look up the active `$param` map via
/// [`crate::cypher::eval::lookup_param`] and error with `MissingParameter`
/// if absent.
pub(crate) fn resolve_lookup_key(key: &LookupKey) -> Result<Value> {
    match key {
        LookupKey::Literal(lv) => Ok(literal_to_value(lv)),
        LookupKey::Param(name) => crate::cypher::eval::lookup_param(name).ok_or_else(|| {
            GraphError::query(
                QueryPhase::Runtime,
                ErrorCode::MissingParameter,
                format!("missing parameter: ${name}"),
            )
        }),
    }
}

/// Cypher type ordering rank for cross-type comparisons.
/// Order: Map < Node < Relationship < Path < List < String < Bool < Number < Null
pub(in crate::cypher::executor) fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Map(_) => 0,
        Value::Node(_) => 1,
        Value::Edge(_) => 2,
        Value::List(_) => 3,
        Value::Path(_) => 4,
        Value::String(_) => 5,
        Value::Bool(_) => 6,
        Value::I64(_) | Value::F64(_) => 7,
        Value::Null => 8,
        // Temporal types grouped after numbers.
        Value::Date(_)
        | Value::LocalTime(_)
        | Value::Time(_)
        | Value::LocalDateTime(_)
        | Value::DateTime(_)
        | Value::Duration(_) => 7,
    }
}

pub(in crate::cypher) fn compare_values_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::I64(a), Value::I64(b)) => a.cmp(b),
        (Value::F64(a), Value::F64(b)) => match (a.is_nan(), b.is_nan()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater, // NaN sorts after numbers
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        },
        (Value::I64(_), Value::F64(b)) if b.is_nan() => std::cmp::Ordering::Less,
        (Value::F64(a), Value::I64(_)) if a.is_nan() => std::cmp::Ordering::Greater,
        (Value::I64(a), Value::F64(b)) => (*a as f64)
            .partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::F64(a), Value::I64(b)) => a
            .partial_cmp(&(*b as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::List(a), Value::List(b)) => {
            for (x, y) in a.iter().zip(b.iter()) {
                let ord = compare_values_for_sort(x, y);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            a.len().cmp(&b.len())
        }
        // Temporal types.
        (Value::Date(a), Value::Date(b)) => a.0.cmp(&b.0),
        (Value::LocalTime(a), Value::LocalTime(b)) => a.0.cmp(&b.0),
        (Value::Time(a), Value::Time(b)) => {
            let a_utc = a.0 - a.1;
            let b_utc = b.0 - b.1;
            a_utc.cmp(&b_utc)
        }
        (Value::LocalDateTime(a), Value::LocalDateTime(b)) => a.0.cmp(&b.0),
        (Value::DateTime(a), Value::DateTime(b)) => {
            let a_utc = a.0 - a.1;
            let b_utc = b.0 - b.1;
            a_utc.cmp(&b_utc)
        }
        (Value::Duration(a), Value::Duration(b)) => {
            // Duration ordering: months first, then days, then seconds, then nanos.
            a.months
                .cmp(&b.months)
                .then(a.days.cmp(&b.days))
                .then(a.seconds.cmp(&b.seconds))
                .then(a.nanos.cmp(&b.nanos))
        }
        // Different types: compare by type rank.
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

/// Execute a plan as a correlated subquery, pushing the outer record's
/// bindings down so inner scans and filters can see them. Returns true
/// if at least one row is produced. Used by EXISTS subqueries and pattern
/// predicates.
pub fn exec_correlated_exists(
    conn: &Connection,
    plan: &LogicalOp,
    outer: &NamedRecord,
) -> Result<bool> {
    let ctx = ExecContext::for_subquery();
    validate_traversal_depth(plan, &ctx)?;
    let rows = exec_correlated(conn, plan, outer, &ctx)?;
    Ok(!rows.is_empty())
}

/// Execute a correlated subquery and return all matching rows.
/// Used by pattern comprehensions to collect all matches.
pub fn exec_correlated_subquery(
    conn: &Connection,
    plan: &LogicalOp,
    outer: &NamedRecord,
) -> Result<Vec<NamedRecord>> {
    let ctx = ExecContext::for_subquery();
    validate_traversal_depth(plan, &ctx)?;
    let rows = exec_correlated(conn, plan, outer, &ctx)?;
    check_row_limit(&rows, &ctx)?;
    Ok(rows)
}

/// `outer` overlaid with `rec`: the row a correlated operator evaluates
/// expressions against. Keys of `rec` win over keys of `outer`.
pub(in crate::cypher::executor) fn merge_outer(
    outer: &NamedRecord,
    rec: &NamedRecord,
) -> NamedRecord {
    let mut merged = outer.clone();
    for (k, v) in &rec.fields {
        merged.set(k.clone(), v.clone());
    }
    merged
}

/// How the enclosing (correlated) scope constrains the alias of a
/// node-producing leaf operator (Scan, IndexLookup, IdLookup, FullTextLookup).
pub(in crate::cypher::executor) enum AliasBinding {
    /// The alias is not bound outside: the operator runs unconstrained.
    Free,
    /// The alias is bound to null (e.g. an unmatched OPTIONAL MATCH): no match.
    Null,
    /// The alias is bound to this node: only it may match.
    Node(NodeId),
}

impl AliasBinding {
    /// Classify `alias` in `outer`.
    pub(in crate::cypher::executor) fn of(outer: &NamedRecord, alias: &str) -> Self {
        match outer.get(alias) {
            Some(Value::Null) => AliasBinding::Null,
            Some(v) => value_to_node_id(v).map_or(AliasBinding::Free, AliasBinding::Node),
            None => AliasBinding::Free,
        }
    }

    /// The bound node, if any.
    pub(in crate::cypher::executor) fn node(&self) -> Option<NodeId> {
        match self {
            AliasBinding::Node(id) => Some(*id),
            _ => None,
        }
    }
}

/// Build a `NamedRecord` from a `Node`, keyed under the given alias.
///
/// Note: NodeId (u64) is transmitted as i64. This wraps for IDs above
/// i64::MAX (~9.2e18), which is practically unreachable — sequential IDs
/// would take thousands of years at millions of inserts per second.
/// Crate-internal alias so `iter_slot::UnwindSlotIter` can populate
/// flat node bindings the same way `exec_unwind` does, without
/// duplicating the field list.
pub(crate) fn node_to_record_pub(n: &crate::types::Node, alias: &str) -> NamedRecord {
    node_to_record(n, alias)
}

pub(crate) fn node_to_record(n: &crate::types::Node, alias: &str) -> NamedRecord {
    debug_assert!(n.id.0 <= i64::MAX as u64, "NodeId exceeds i64::MAX");
    let mut rec = NamedRecord::new();
    populate_node_bindings(&mut rec, n, alias);
    rec
}

/// Write the standard set of node bindings (`alias`, `alias.<prop>`,
/// `alias.__id`, `alias.__label`, `alias.__labels`) into `rec`. Used both by
/// `node_to_record` (fresh record) and by Expand-style operators that merge
/// new bindings into an existing record.
pub(crate) fn populate_node_bindings(rec: &mut NamedRecord, n: &crate::types::Node, alias: &str) {
    rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
    for (key, val) in &n.properties {
        rec.set(format!("{alias}.{key}"), val.clone());
    }
    // Store the colon-joined labels for label matching in filters.
    rec.set(
        format!("{alias}.__label"),
        Value::String(n.labels.join(":")),
    );
    // Store individual labels as a list for multi-label filtering.
    rec.set(
        format!("{alias}.__labels"),
        Value::List(n.labels.iter().map(|l| Value::String(l.clone())).collect()),
    );
    rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
}

/// Fetch a node by ID and populate `rec` with its bindings under `alias`.
/// Returns the fetched Node so callers can inspect it (e.g. to read labels
/// or build edge bindings).
pub(crate) fn fetch_and_populate(
    conn: &Connection,
    rec: &mut NamedRecord,
    id: NodeId,
    alias: &str,
) -> Result<crate::types::Node> {
    let n = node::get_node(conn, id)?;
    populate_node_bindings(rec, &n, alias);
    Ok(n)
}

/// Write the flat relationship bindings for `e` under `alias`
/// (`alias.__src`, `__dst`, `__type`, `__edge_seq` and `alias.<prop>`).
/// The bare `alias` key is left to the caller.
pub(crate) fn populate_edge_bindings(rec: &mut NamedRecord, e: &crate::types::Edge, alias: &str) {
    bind_edge_fields(rec, alias, e.src, e.dst, &e.label, e.seq, &e.properties);
}

/// [`populate_edge_bindings`] from the parts of a stored edge, for callers
/// that have not built an `Edge` value.
pub(in crate::cypher::executor) fn bind_edge_fields(
    rec: &mut NamedRecord,
    alias: &str,
    src: NodeId,
    dst: NodeId,
    label: &str,
    seq: u64,
    properties: &Properties,
) {
    rec.set(format!("{alias}.__src"), Value::I64(src.0 as i64));
    rec.set(format!("{alias}.__dst"), Value::I64(dst.0 as i64));
    rec.set(format!("{alias}.__type"), Value::String(label.to_string()));
    rec.set(format!("{alias}.__edge_seq"), Value::I64(seq as i64));
    for (k, v) in properties {
        rec.set(format!("{alias}.{k}"), v.clone());
    }
}

pub(in crate::cypher::executor) fn agg_fn_name(f: AggregateFunction) -> &'static str {
    match f {
        AggregateFunction::Count => "count",
        AggregateFunction::Sum => "sum",
        AggregateFunction::Avg => "avg",
        AggregateFunction::Min => "min",
        AggregateFunction::Max => "max",
        AggregateFunction::Collect => "collect",
        AggregateFunction::PercentileDisc => "percentileDisc",
        AggregateFunction::PercentileCont => "percentileCont",
        AggregateFunction::StDev => "stDev",
        AggregateFunction::StDevP => "stDevP",
    }
}

/// Compute the column name for an aggregate expression, matching what
/// `expr_to_column_name` produces for the original `FunctionCall` expression.
pub(crate) fn agg_col_name(agg: &AggregateExpr) -> String {
    agg.alias.clone().unwrap_or_else(|| {
        if let Some(ref text) = agg.original_call_text {
            return text.clone();
        }
        let name = if agg.original_name.is_empty() {
            agg_fn_name(agg.function).to_string()
        } else {
            agg.original_name.clone()
        };
        let expr =
            crate::cypher::ast::Expr::synthetic(crate::cypher::ast::ExprKind::FunctionCall {
                name,
                args: vec![agg.input.clone()],
                distinct: agg.distinct,
                original_text: None,
            });
        expr_to_column_name(&expr)
    })
}
