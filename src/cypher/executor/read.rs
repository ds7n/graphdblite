//! Read operators — Scan, IndexLookup, Expand, Filter, Project, CrossProduct, Unwind, plus compound-binding helpers.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::ir::LookupKey;
use crate::cypher::record::NamedRecord;
use crate::cypher::record_view::RecordView;
use crate::types::*;
use crate::{edge, index, node};

use super::util::*;
use super::*;

pub(in crate::cypher::executor) fn exec_scan(
    conn: &Connection,
    label: &str,
    alias: &str,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let nodes = node::find_nodes_by_label(conn, label)?;
    let mut records = Vec::with_capacity(nodes.len());
    for n in nodes {
        records.push(node_to_record(&n, alias));
    }
    check_row_limit(&records, ctx)?;
    Ok(records)
}

/// Run an `IdLookup`: the node whose id equals `value_expr` (evaluated
/// against `record`, the correlated outer row). `bound` is the node the
/// alias is already bound to in an enclosing scope, if any; a different id
/// then matches nothing.
pub(in crate::cypher::executor) fn exec_id_lookup(
    conn: &Connection,
    alias: &str,
    value_expr: &Expr,
    record: &NamedRecord,
    bound: Option<NodeId>,
) -> Result<Vec<NamedRecord>> {
    let evaluated = eval_expr(value_expr, record, crate::cypher::eval::EvalCx::new(conn))?;
    // `id(n) = 2.0` holds for node 2 (Cypher numeric equality). Anything that
    // cannot equal a node id (negative, fractional, null, non-numeric) yields
    // zero rows rather than an error, like an unmatched WHERE predicate.
    let id = match evaluated {
        Value::I64(n) if n >= 0 => NodeId(n as u64),
        Value::F64(f) if f >= 0.0 && f.fract() == 0.0 && f < i64::MAX as f64 => NodeId(f as u64),
        _ => return Ok(Vec::new()),
    };
    if bound.is_some_and(|b| b != id) {
        return Ok(Vec::new());
    }
    match node::get_node(conn, id) {
        Ok(n) => Ok(vec![node_to_record(&n, alias)]),
        Err(GraphError::NodeNotFound { .. }) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Whether a candidate row passes a leaf operator's residual filter.
/// The predicate may reference variables of the enclosing (correlated)
/// scope, so it is evaluated over `outer` merged with the candidate.
fn residual_passes(
    conn: &Connection,
    filter: Option<&Expr>,
    candidate: &NamedRecord,
    outer: &NamedRecord,
) -> Result<bool> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    let cx = crate::cypher::eval::EvalCx::new(conn);
    if outer.is_empty() {
        return eval_predicate(filter, candidate, cx);
    }
    eval_predicate(filter, &merge_outer(outer, candidate), cx)
}

/// Run a `FullTextLookup`. `record` is the correlated outer row (empty at
/// top level): the term and the residual filter are evaluated against it.
/// `bound` restricts the result to the node the alias is already bound to.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_fulltext_lookup(
    conn: &Connection,
    label: &str,
    alias: &str,
    property: &str,
    op: crate::cypher::ir::FullTextOp,
    case_insensitive: bool,
    term_expr: &Expr,
    remaining_filters: Option<&Expr>,
    record: &NamedRecord,
    bound: Option<NodeId>,
) -> Result<Vec<NamedRecord>> {
    let evaluated = eval_expr(term_expr, record, crate::cypher::eval::EvalCx::new(conn))?;
    // A non-string term makes `CONTAINS` / `STARTS WITH` / `ENDS WITH` (and
    // `toLower(<non-string>)`) null on every row, exactly as the label scan
    // this lookup replaces would evaluate it: no rows, not an error.
    let Value::String(term) = evaluated else {
        return Ok(Vec::new());
    };

    // The FTS index only narrows candidates; `fts_predicate_matches` decides.
    // A case-insensitive lookup with non-ASCII text could disagree with Rust's
    // `to_lowercase` (FTS5 uses its own Unicode folding), so scan instead.
    let candidates = if case_insensitive && !term.is_ascii() {
        None
    } else {
        crate::fts::fulltext_lookup(conn, label, property, &term)?
    };
    let scored_ids = match candidates {
        Some(rows) => rows,
        None => {
            // Term is below the trigram floor (<3 codepoints). Fall back to
            // a full label scan with per-row predicate evaluation.
            return exec_fulltext_fallback(
                conn,
                label,
                alias,
                property,
                op,
                case_insensitive,
                &term,
                remaining_filters,
                record,
                bound,
            );
        }
    };

    let mut records = Vec::new();
    for (id, rank) in scored_ids {
        if bound.is_some_and(|b| b != id) {
            continue;
        }
        let n = node::get_node(conn, id)?;
        // Re-check every candidate with the exact predicate: anchored ops need
        // a position check, and a case-insensitive index returns candidates
        // that a case-sensitive predicate must reject (and vice versa).
        let Some(Value::String(s)) = n.properties.get(property) else {
            continue;
        };
        if !fts_predicate_matches(s, op, case_insensitive, &term) {
            continue;
        }
        let mut rec = node_to_record(&n, alias);
        // Surface BM25 score for `score(<alias>)` projections. FTS5's
        // `bm25()` is negative-signed (lower = better); negate so the
        // user-facing convention is higher = better, matching the
        // `fts.search` procedure path.
        rec.set(format!("{alias}.__fts_score"), Value::F64(-rank));
        if !residual_passes(conn, remaining_filters, &rec, record)? {
            continue;
        }
        records.push(rec);
    }
    Ok(records)
}

/// Evaluate the string predicate a `FullTextLookup` stands for:
/// `s <op> term` (case-sensitive), or `toLower(s) <op> toLower(term)`.
fn fts_predicate_matches(
    s: &str,
    op: crate::cypher::ir::FullTextOp,
    case_insensitive: bool,
    term: &str,
) -> bool {
    use crate::cypher::ir::FullTextOp;
    let (s, term): (std::borrow::Cow<str>, std::borrow::Cow<str>) = if case_insensitive {
        (s.to_lowercase().into(), term.to_lowercase().into())
    } else {
        (s.into(), term.into())
    };
    match op {
        FullTextOp::Contains => s.contains(term.as_ref()),
        FullTextOp::StartsWith => s.starts_with(term.as_ref()),
        FullTextOp::EndsWith => s.ends_with(term.as_ref()),
    }
}

/// Fallback for when the term is below the trigram floor (<3 codepoints).
/// Performs a full label scan with per-row string predicate evaluation.
#[allow(clippy::too_many_arguments)]
fn exec_fulltext_fallback(
    conn: &Connection,
    label: &str,
    alias: &str,
    property: &str,
    op: crate::cypher::ir::FullTextOp,
    case_insensitive: bool,
    term: &str,
    remaining_filters: Option<&Expr>,
    outer: &NamedRecord,
    bound: Option<NodeId>,
) -> Result<Vec<NamedRecord>> {
    let nodes = match bound {
        Some(id) => match node::get_node(conn, id) {
            Ok(n) if n.labels.iter().any(|l| l == label) => vec![n],
            Ok(_) | Err(GraphError::NodeNotFound { .. }) => Vec::new(),
            Err(e) => return Err(e),
        },
        None => node::find_nodes_by_label(conn, label)?,
    };
    let mut records = Vec::new();
    for n in nodes {
        let Some(Value::String(s)) = n.properties.get(property) else {
            continue;
        };
        if !fts_predicate_matches(s, op, case_insensitive, term) {
            continue;
        }
        let mut rec = node_to_record(&n, alias);
        // Trigram-floor fallback rows still matched via an FTS-eligible op,
        // so emit a concrete 0.0 score rather than leaving `score(n)` NULL.
        rec.set(format!("{alias}.__fts_score"), Value::F64(0.0));
        if !residual_passes(conn, remaining_filters, &rec, outer)? {
            continue;
        }
        records.push(rec);
    }
    Ok(records)
}

/// Run an `IndexLookup`. `outer` is the correlated outer row (empty at top
/// level), visible to the residual filter; `bound` restricts the result to
/// the node the alias is already bound to. Equality follows Cypher
/// semantics either way (see [`index_lookup_ids`]).
#[allow(clippy::too_many_arguments)]
pub(in crate::cypher::executor) fn exec_index_lookup(
    conn: &Connection,
    label: &str,
    alias: &str,
    index_properties: &[String],
    lookups: &[(String, LookupKey)],
    remaining_filters: Option<&Expr>,
    outer: &NamedRecord,
    bound: Option<NodeId>,
) -> Result<Vec<NamedRecord>> {
    let mut node_ids = index_lookup_ids(conn, label, index_properties, lookups)?;
    if let Some(b) = bound {
        node_ids.retain(|id| *id == b);
    }
    let mut records = Vec::new();

    for id in node_ids {
        let n = node::get_node(conn, id)?;
        // Defense-in-depth: only emit nodes that actually carry the queried
        // label. Index table names historically could collide across
        // (label, property) pairs containing underscores, so a lookup could
        // surface a node stored under a different label; re-checking membership
        // here guarantees index-served results never leak a cross-label node.
        if !label.is_empty() && !n.labels.iter().any(|l| l == label) {
            continue;
        }
        let rec = node_to_record(&n, alias);
        if !residual_passes(conn, remaining_filters, &rec, outer)? {
            continue;
        }
        records.push(rec);
    }

    Ok(records)
}

/// Evaluate `lookups` to concrete values and call `composite_index_prefix_lookup`.
/// `index_properties` is the full property list of the chosen index (from the IR),
/// while `lookups` is the matched prefix (len <= index_properties.len()).
/// Works for N=1 (single-prop) and N>=2 (composite prefix) uniformly.
pub(crate) fn index_lookup_ids(
    conn: &Connection,
    label: &str,
    index_properties: &[String],
    lookups: &[(String, LookupKey)],
) -> Result<Vec<NodeId>> {
    let values: Vec<Value> = lookups
        .iter()
        .map(|(_, k)| crate::cypher::executor::resolve_lookup_key(k))
        .collect::<Result<_>>()?;
    let full_props_refs: Vec<&str> = index_properties.iter().map(String::as_str).collect();

    // Cypher equality semantics, which the index (keyed on exact msgpack bytes)
    // does not provide on its own:
    // - `x = null` (and anything containing null / NaN) is never true;
    // - integers and floats compare numerically (`1 = 1.0`, `0.0 = -0.0`).
    // Expand each value into every stored representation it is equal to and
    // union the lookups. Pathologically large expansions fall back to a label
    // scan with an explicit equality check.
    let mut per_value: Vec<Vec<Value>> = Vec::with_capacity(values.len());
    let mut combos: usize = 1;
    for v in &values {
        match index_equality_variants(v) {
            EqVariants::Never => return Ok(Vec::new()),
            EqVariants::TooMany => combos = usize::MAX,
            EqVariants::Values(vs) => {
                combos = combos.saturating_mul(vs.len());
                per_value.push(vs);
            }
        }
    }
    if combos > MAX_INDEX_LOOKUP_VARIANTS {
        let props: Vec<&str> = lookups.iter().map(|(p, _)| p.as_str()).collect();
        return Ok(node::find_nodes_by_label(conn, label)?
            .into_iter()
            .filter(|n| {
                props.iter().zip(&values).all(|(p, v)| {
                    n.properties
                        .get(*p)
                        .is_some_and(|stored| crate::cypher::eval::cypher_equals(stored, v))
                })
            })
            .map(|n| n.id)
            .collect());
    }
    if combos == 1 {
        let single: Vec<Value> = per_value.into_iter().map(|mut v| v.remove(0)).collect();
        return index::composite_index_prefix_lookup(conn, label, &full_props_refs, &single);
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut combo: Vec<Value> = Vec::with_capacity(per_value.len());
    fn walk(
        conn: &Connection,
        label: &str,
        props: &[&str],
        per_value: &[Vec<Value>],
        combo: &mut Vec<Value>,
        seen: &mut std::collections::HashSet<NodeId>,
        out: &mut Vec<NodeId>,
    ) -> Result<()> {
        if combo.len() == per_value.len() {
            for id in index::composite_index_prefix_lookup(conn, label, props, combo)? {
                if seen.insert(id) {
                    out.push(id);
                }
            }
            return Ok(());
        }
        for v in &per_value[combo.len()] {
            combo.push(v.clone());
            walk(conn, label, props, per_value, combo, seen, out)?;
            combo.pop();
        }
        Ok(())
    }
    walk(
        conn,
        label,
        &full_props_refs,
        &per_value,
        &mut combo,
        &mut seen,
        &mut out,
    )?;
    Ok(out)
}

/// Upper bound on the number of exact-representation lookups a single index
/// probe may expand into (see [`index_equality_variants`]).
const MAX_INDEX_LOOKUP_VARIANTS: usize = 64;

/// Result of [`index_equality_variants`].
enum EqVariants {
    /// Nothing is Cypher-equal to the value (null, NaN, or a list containing one).
    Never,
    /// Every exact stored representation that is Cypher-equal to the value.
    Values(Vec<Value>),
    /// Too many representations to enumerate; the caller should scan.
    TooMany,
}

/// Every stored value that is Cypher-equal to `v` (e.g. `1` is equal to the
/// stored `1` and `1.0`; a list expands element-wise).
fn index_equality_variants(v: &Value) -> EqVariants {
    const EXACT_F64_INT_LIMIT: i64 = 1 << 53;
    match v {
        Value::Null => EqVariants::Never,
        Value::F64(f) if f.is_nan() => EqVariants::Never,
        Value::I64(i) => {
            let mut out = vec![Value::I64(*i)];
            if i.unsigned_abs() <= EXACT_F64_INT_LIMIT as u64 {
                out.push(Value::F64(*i as f64));
                if *i == 0 {
                    out.push(Value::F64(-0.0));
                }
            }
            EqVariants::Values(out)
        }
        Value::F64(f) => {
            let mut out = vec![Value::F64(*f)];
            if *f == 0.0 {
                out.push(Value::F64(if f.is_sign_negative() { 0.0 } else { -0.0 }));
            }
            if f.fract() == 0.0 && f.abs() <= EXACT_F64_INT_LIMIT as f64 {
                out.push(Value::I64(*f as i64));
            }
            EqVariants::Values(out)
        }
        Value::List(items) => {
            let mut acc: Vec<Vec<Value>> = vec![Vec::with_capacity(items.len())];
            for item in items {
                let vs = match index_equality_variants(item) {
                    EqVariants::Values(vs) => vs,
                    other => return other,
                };
                if acc.len().saturating_mul(vs.len()) > MAX_INDEX_LOOKUP_VARIANTS {
                    return EqVariants::TooMany;
                }
                let mut next = Vec::with_capacity(acc.len() * vs.len());
                for prefix in &acc {
                    for alt in &vs {
                        let mut p = prefix.clone();
                        p.push(alt.clone());
                        next.push(p);
                    }
                }
                acc = next;
            }
            EqVariants::Values(acc.into_iter().map(Value::List).collect())
        }
        other => EqVariants::Values(vec![other.clone()]),
    }
}

/// Run the materialized `Expand` operator (`plan` must be one).
pub(in crate::cypher::executor) fn exec_expand(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let LogicalOp::Expand { input, .. } = plan else {
        unreachable!("exec_expand called with {}", plan.op_name());
    };
    let input_records = exec(conn, input, ctx)?;
    expand_all(conn, plan, &input_records, &NamedRecord::new(), ctx)
}

/// Expand every input record through the `Expand` operator `plan`. `outer`
/// is the enclosing correlated row (empty at top level): it supplies
/// variables for relationship property maps and pre-bound destination /
/// relationship variables. Shared by the materialized and correlated
/// executors so both apply identical semantics.
pub(in crate::cypher::executor) fn expand_all(
    conn: &Connection,
    plan: &LogicalOp,
    input_records: &[NamedRecord],
    outer: &NamedRecord,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let LogicalOp::Expand {
        src_alias,
        dst_alias,
        rel_alias,
        edge_types,
        direction,
        min_hops,
        max_hops,
        var_length,
        var_length_prop_filters,
        result_cap,
        ..
    } = plan
    else {
        unreachable!("expand_all called with {}", plan.op_name());
    };
    let result_cap = result_cap.map(|c| c as usize);
    let mut results = Vec::new();

    for rec in input_records {
        if result_cap.is_some_and(|cap| results.len() >= cap) {
            break;
        }
        // Relationship property constraints on a var-length pattern may be
        // any expression (parameters, variables, arithmetic); evaluate them
        // against the current row rather than keeping only literals.
        let prop_filter_values = if var_length_prop_filters.is_empty() {
            HashMap::new()
        } else if outer.is_empty() {
            eval_var_length_prop_filters(conn, var_length_prop_filters, rec)?
        } else {
            eval_var_length_prop_filters(conn, var_length_prop_filters, &merge_outer(outer, rec))?
        };
        let per_call_cap = result_cap.map(|c| c.saturating_sub(results.len()));
        let expanded = expand_record(
            conn,
            rec,
            outer,
            src_alias,
            dst_alias,
            rel_alias.as_deref(),
            edge_types,
            *direction,
            *min_hops,
            *max_hops,
            *var_length,
            &prop_filter_values,
            per_call_cap,
            ctx.max_traversal_work,
        )?;
        results.extend(expanded);
        // Stop as soon as the cap is passed, not after every input row.
        check_row_limit(&results, ctx)?;
    }

    check_row_limit(&results, ctx)?;
    Ok(results)
}

/// Evaluate the relationship property map of a var-length pattern against
/// `rec`. Every entry is kept: dropping non-literal entries would silently
/// widen the match.
pub(crate) fn eval_var_length_prop_filters(
    conn: &Connection,
    filters: &HashMap<String, Expr>,
    rec: &NamedRecord,
) -> Result<HashMap<String, Value>> {
    let mut out = HashMap::with_capacity(filters.len());
    for (k, expr) in filters {
        let v = match &expr.kind {
            ExprKind::Literal(lit) => literal_to_value(lit),
            _ => eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?,
        };
        out.insert(k.clone(), v);
    }
    Ok(out)
}

/// A relationship variable that an Expand finds already bound, either
/// earlier in the current row or in the enclosing (correlated) scope.
enum BoundRel {
    /// Bound to null (e.g. an unmatched OPTIONAL MATCH): nothing matches.
    Null,
    /// A single relationship.
    One(EdgeKey),
    /// A list of relationships (var-length binding such as
    /// `WITH [r1, r2] AS rs MATCH ()-[rs*]->()`).
    List(Vec<Edge>),
}

/// Look up `alias` as an already-bound relationship in `rec`, then `outer`.
fn bound_relationship(rec: &NamedRecord, outer: &NamedRecord, alias: &str) -> Option<BoundRel> {
    for scope in [rec, outer] {
        // Every relationship binding sets the bare alias; checking it first
        // keeps the common unbound case allocation-free.
        let Some(bare) = scope.get(alias) else {
            continue;
        };
        if let Some(key) = flat_edge_key(scope, alias) {
            return Some(BoundRel::One(key));
        }
        match bare {
            Value::Null => return Some(BoundRel::Null),
            Value::Edge(e) => return Some(BoundRel::One(edge_key(e))),
            Value::List(items) if !items.is_empty() => {
                let edges: Vec<Edge> = items
                    .iter()
                    .filter_map(|v| match v {
                        Value::Edge(e) => Some(e.clone()),
                        _ => None,
                    })
                    .collect();
                if edges.len() == items.len() {
                    return Some(BoundRel::List(edges));
                }
            }
            _ => {}
        }
    }
    None
}

/// Follow a pre-bound relationship list from `src` in `direction`; returns
/// the node the chain ends at, or `None` if the edges do not form a path
/// starting at `src`.
fn walk_bound_edges(src: NodeId, edges: &[Edge], direction: Direction) -> Option<NodeId> {
    let mut current = src;
    for e in edges {
        current = match direction {
            Direction::Outgoing if e.src == current => e.dst,
            Direction::Incoming if e.dst == current => e.src,
            Direction::Both if e.src == current => e.dst,
            Direction::Both if e.dst == current => e.src,
            _ => return None,
        };
    }
    Some(current)
}

/// Load the relationships of a var-length traversal as `Value::Edge`s,
/// each carrying its parallel-edge `seq`.
fn steps_to_edges(conn: &Connection, steps: &[edge::PathStep]) -> Result<Vec<Value>> {
    steps
        .iter()
        .map(|step| {
            let properties = edge::get_edge_properties_at(
                conn,
                step.edge_src,
                step.edge_dst,
                &step.edge_label,
                step.edge_seq,
            )?;
            Ok(Value::Edge(Edge {
                src: step.edge_src,
                dst: step.edge_dst,
                label: step.edge_label.clone(),
                seq: step.edge_seq,
                properties,
            }))
        })
        .collect()
}

/// Expand one input record into zero or more output records. This is the
/// single implementation of Expand semantics, shared by the materialized
/// executor, the correlated executor and the pull-based `ExpandIter`:
///
/// - labeled or untyped relationships, single-hop or var-length;
/// - every stored edge is its own row: parallel edges each appear, and
///   [`Direction::Both`] yields edges stored either way (a self-loop once);
/// - relationship uniqueness against relationships already bound in `rec`
///   (earlier segments of the same pattern), keyed on the directed
///   `(src, dst, type, seq)` identity;
/// - destination / relationship variables already bound in `rec` or in the
///   enclosing correlated row `outer` constrain the expansion;
/// - `result_cap` (LIMIT pushdown) counts only rows that survive every
///   filter above.
#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_record(
    conn: &Connection,
    rec: &NamedRecord,
    outer: &NamedRecord,
    src_alias: &str,
    dst_alias: &str,
    rel_alias: Option<&str>,
    edge_types: &[String],
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    var_length: bool,
    var_length_prop_filters: &HashMap<String, Value>,
    result_cap: Option<usize>,
    max_traversal_work: u64,
) -> Result<Vec<NamedRecord>> {
    let mut results = Vec::new();
    let Some(src_id) = rec.get(src_alias).and_then(value_to_node_id) else {
        return Ok(results);
    };
    let capped = |results: &Vec<NamedRecord>| result_cap.is_some_and(|c| results.len() >= c);

    // A destination already bound, by an earlier element of this pattern
    // (cycle such as `(a)-[:R]->(b)-[:S]->(a)`) or by the enclosing scope,
    // keeps only expansions that end there.
    let bound_dst = rec
        .get(dst_alias)
        .and_then(value_to_node_id)
        .or_else(|| outer.get(dst_alias).and_then(value_to_node_id));
    let bound_rel = rel_alias.and_then(|ra| bound_relationship(rec, outer, ra));
    if matches!(bound_rel, Some(BoundRel::Null)) {
        return Ok(results);
    }
    // Relationships already used by this pattern may not be reused.
    let prior = bound_edge_keys(rec, rel_alias);

    if var_length {
        if let Some(BoundRel::List(edges)) = bound_rel {
            let Some(dst_id) = walk_bound_edges(src_id, &edges, direction) else {
                return Ok(results);
            };
            if bound_dst.is_some_and(|b| b != dst_id)
                || edges.iter().any(|e| prior.contains(&edge_key(e)))
            {
                return Ok(results);
            }
            let mut new_rec = rec.clone();
            fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
            if let Some(ra) = rel_alias {
                new_rec.set(
                    ra.to_string(),
                    Value::List(edges.into_iter().map(Value::Edge).collect()),
                );
            }
            results.push(new_rec);
            return Ok(results);
        }

        // An empty type list tells traverse_paths to discover types per hop
        // (different nodes may have different edge types).
        let labels: Vec<&str> = edge_types.iter().map(String::as_str).collect();
        // The cap may only be pushed into the traversal when no path is
        // dropped afterwards; otherwise it would count rejected paths.
        let post_filtered = bound_dst.is_some() || !prior.is_empty();
        let paths = edge::traverse_paths(
            conn,
            src_id,
            &labels,
            direction,
            min_hops,
            max_hops,
            var_length_prop_filters,
            if post_filtered { None } else { result_cap },
            max_traversal_work,
        )?;
        for (dst_id, steps) in paths {
            if capped(&results) {
                break;
            }
            if bound_dst.is_some_and(|b| b != dst_id) {
                continue;
            }
            if steps.iter().any(|s| {
                prior
                    .iter()
                    .any(|k| is_edge(k, s.edge_src, s.edge_dst, &s.edge_label, s.edge_seq))
            }) {
                continue;
            }
            let mut new_rec = rec.clone();
            fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
            if let Some(ra) = rel_alias {
                new_rec.set(ra.to_string(), Value::List(steps_to_edges(conn, &steps)?));
            }
            results.push(new_rec);
        }
        return Ok(results);
    }

    // Single hop. `emit` turns one stored edge (storage orientation) into an
    // output row unless a bound destination or relationship uniqueness
    // rules it out.
    let mut dst_node: Option<Node> = None;
    let mut emit = |s: NodeId,
                    d: NodeId,
                    label: &str,
                    seq: u64,
                    props: &Properties,
                    results: &mut Vec<NamedRecord>|
     -> Result<()> {
        // The far end: for a self-loop both ends are `src`.
        let dst_id = if s == src_id { d } else { s };
        if bound_dst.is_some_and(|b| b != dst_id)
            || prior.iter().any(|k| is_edge(k, s, d, label, seq))
        {
            return Ok(());
        }
        if dst_node.as_ref().is_none_or(|n| n.id != dst_id) {
            dst_node = Some(node::get_node(conn, dst_id)?);
        }
        let mut new_rec = rec.clone();
        if let Some(n) = &dst_node {
            populate_node_bindings(&mut new_rec, n, dst_alias);
        }
        if let Some(ra) = rel_alias {
            new_rec.set(ra.to_string(), Value::String(label.to_string()));
            bind_edge_fields(&mut new_rec, ra, s, d, label, seq, props);
        }
        results.push(new_rec);
        Ok(())
    };

    match bound_rel {
        Some(BoundRel::One((s, d, t, seq))) => {
            // Only the bound relationship can match: check type and that it
            // touches `src` in the pattern's direction.
            if !edge_types.is_empty() && !edge_types.contains(&t) {
                return Ok(results);
            }
            let (s, d) = (NodeId(s), NodeId(d));
            let touches = match direction {
                Direction::Outgoing => s == src_id,
                Direction::Incoming => d == src_id,
                Direction::Both => s == src_id || d == src_id,
            };
            if touches {
                let props = edge::get_edge_properties_at(conn, s, d, &t, seq)?;
                emit(s, d, &t, seq, &props, &mut results)?;
            }
        }
        Some(BoundRel::List(_)) | Some(BoundRel::Null) => {}
        None => {
            // No types given: follow every type stored on `src`.
            let discovered: Vec<String> = if edge_types.is_empty() {
                edge::get_all_edge_labels(conn, src_id, direction)?
                    .into_iter()
                    .map(|(l, _)| l)
                    .collect()
            } else {
                Vec::new()
            };
            let labels: &[String] = if edge_types.is_empty() {
                &discovered
            } else {
                edge_types
            };
            for label in labels {
                for nbr in edge::get_neighbors(conn, src_id, label, direction)? {
                    if bound_dst.is_some_and(|b| b != nbr) {
                        continue;
                    }
                    for (s, d, seq, props) in edge::hop_edges(conn, src_id, nbr, label, direction)?
                    {
                        if capped(&results) {
                            return Ok(results);
                        }
                        emit(s, d, label, seq, &props, &mut results)?;
                    }
                }
            }
        }
    }

    Ok(results)
}

/// Whether `key` identifies the stored edge `(s, d, label, seq)`
/// (allocation-free [`EdgeKey`] comparison).
fn is_edge(key: &EdgeKey, s: NodeId, d: NodeId, label: &str, seq: u64) -> bool {
    key.0 == s.0 && key.1 == d.0 && key.3 == seq && key.2 == label
}

pub(in crate::cypher::executor) fn exec_cross_product(
    conn: &Connection,
    left: &LogicalOp,
    right: &LogicalOp,
    same_match: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    // Materialize only the left side. Re-execute the right side per left
    // record so peak memory is O(left + right + output) instead of
    // O(left * right).
    let left_records = exec(conn, left, ctx)?;
    let mut results = Vec::new();
    for l in &left_records {
        let right_records = exec(conn, right, ctx)?;
        let _transient = TransientRows::of(&right_records);
        for r in &right_records {
            let mut combined = l.clone();
            for (key, val) in &r.fields {
                combined.set(key.clone(), val.clone());
            }
            // Cross-pattern relationship uniqueness (only within the same
            // MATCH clause — separate MATCHes have independent scopes).
            if same_match && has_duplicate_relationships(&combined) {
                continue;
            }
            push_capped(&mut results, combined, ctx)?;
        }
    }
    Ok(results)
}

/// Directed identity of a stored relationship: `(src, dst, type, seq)`.
/// Relationship uniqueness compares these, so `a->b` and `b->a` are
/// different relationships, as are two parallel `a->b` edges.
pub(in crate::cypher::executor) type EdgeKey = (u64, u64, String, u64);

/// The [`EdgeKey`] of a relationship value.
pub(in crate::cypher::executor) fn edge_key(e: &Edge) -> EdgeKey {
    (e.src.0, e.dst.0, e.label.clone(), e.seq)
}

/// The [`EdgeKey`] of a relationship bound flat under `alias` in `rec`
/// (`alias.__src` / `__dst` / `__type` / `__edge_seq`), if any.
fn flat_edge_key(rec: &NamedRecord, alias: &str) -> Option<EdgeKey> {
    let (Some(Value::I64(s)), Some(Value::I64(d)), Some(Value::String(t))) = (
        rec.get(&format!("{alias}.__src")),
        rec.get(&format!("{alias}.__dst")),
        rec.get(&format!("{alias}.__type")),
    ) else {
        return None;
    };
    let seq = match rec.get(&format!("{alias}.__edge_seq")) {
        Some(Value::I64(s)) => *s as u64,
        _ => 0,
    };
    Some((*s as u64, *d as u64, t.clone(), seq))
}

/// Keys of the relationships bound by single-hop pattern elements in `rec`
/// (flat `alias.__src` bindings), in field order.
fn flat_edge_keys(rec: &NamedRecord, skip: Option<&str>) -> Vec<EdgeKey> {
    rec.fields
        .keys()
        .filter_map(|key| key.strip_suffix(".__src"))
        .filter(|alias| Some(*alias) != skip)
        .filter_map(|alias| flat_edge_key(rec, alias))
        .collect()
}

/// Every relationship bound in `rec` other than `skip`: single-hop flat
/// bindings plus the edges of var-length list bindings. Used to enforce
/// relationship uniqueness across the segments of one pattern.
pub(in crate::cypher::executor) fn bound_edge_keys(
    rec: &NamedRecord,
    skip: Option<&str>,
) -> Vec<EdgeKey> {
    let mut keys = flat_edge_keys(rec, skip);
    for (name, val) in &rec.fields {
        if Some(name.as_str()) == skip {
            continue;
        }
        if let Value::List(items) = val {
            keys.extend(items.iter().filter_map(|v| match v {
                Value::Edge(e) => Some(edge_key(e)),
                _ => None,
            }));
        }
    }
    keys
}

/// Check if a record contains two relationship bindings that refer to the
/// same underlying edge (same directed src/dst/type/seq).
pub(in crate::cypher::executor) fn has_duplicate_relationships(rec: &NamedRecord) -> bool {
    let keys = flat_edge_keys(rec, None);
    let mut seen = std::collections::HashSet::with_capacity(keys.len());
    keys.into_iter().any(|k| !seen.insert(k))
}

pub(in crate::cypher::executor) fn exec_filter(
    conn: &Connection,
    input: &LogicalOp,
    predicate: &Expr,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();
    for rec in records {
        if eval_predicate(predicate, &rec, crate::cypher::eval::EvalCx::new(conn))? {
            results.push(rec);
        }
    }
    Ok(results)
}

/// Reconstruct a compound `Value::Node` or `Value::Edge` from a variable's
/// flat bindings in a record. Returns `None` if the variable has no node/edge
/// metadata (i.e. it's an expression/aggregate result, not a pattern binding).
///
/// Records flow through the pipeline with a flattened shape (`n.name`,
/// `n.__id`, etc.); this helper materializes the compound at projection time
/// so query results look the way openCypher specifies.
pub(crate) fn build_compound_binding(rec: &dyn RecordView, var: &str) -> Option<Value> {
    use crate::types::{Edge, Node, Properties};

    // Var-length relationship variables stored directly as Value::List.
    if let Some(val @ Value::List(_)) = rec.get(var) {
        return Some(val.clone());
    }
    // Path values stored directly.
    if let Some(val @ Value::Path(_)) = rec.get(var) {
        return Some(val.clone());
    }

    let prefix = format!("{var}.");

    // Edge binding: has __src / __dst / __type metadata.
    let src_key = format!("{var}.__src");
    let dst_key = format!("{var}.__dst");
    let type_key = format!("{var}.__type");
    if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) =
        (rec.get(&src_key), rec.get(&dst_key), rec.get(&type_key))
    {
        let src = *src;
        let dst = *dst;
        let label = label.clone();
        let seq = match rec.get(&format!("{var}.__edge_seq")) {
            Some(Value::I64(s)) => *s as u64,
            _ => 0,
        };
        let mut properties = Properties::new();
        rec.for_each_field(&mut |key, val| {
            if let Some(prop) = key.strip_prefix(&prefix) {
                if !prop.starts_with("__") {
                    properties.insert(prop.to_string(), val.clone());
                }
            }
        });
        return Some(Value::Edge(Edge {
            src: NodeId(src as u64),
            dst: NodeId(dst as u64),
            label,
            seq,
            properties,
        }));
    }

    // Node binding: has __id / __label metadata.
    let id_key = format!("{var}.__id");
    let label_key = format!("{var}.__label");
    if let (Some(Value::I64(id)), Some(Value::String(label_str))) =
        (rec.get(&id_key), rec.get(&label_key))
    {
        let id = *id;
        let label_str = label_str.clone();
        let mut properties = Properties::new();
        rec.for_each_field(&mut |key, val| {
            if let Some(prop) = key.strip_prefix(&prefix) {
                if !prop.starts_with("__") {
                    properties.insert(prop.to_string(), val.clone());
                }
            }
        });
        // Reconstruct labels from the colon-joined __label string.
        let labels: Vec<String> = if label_str.is_empty() {
            Vec::new()
        } else {
            label_str.split(':').map(|s| s.to_string()).collect()
        };
        return Some(Value::Node(Node {
            id: NodeId(id as u64),
            labels,
            properties,
        }));
    }

    None
}

/// Collect the set of variable names in a record that are bound as compound
/// entities (nodes or edges). Used by `RETURN *` to know which prefixes to
/// fold into compound columns rather than emitting as flat properties.
pub(crate) fn compound_binding_vars(rec: &dyn RecordView) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut vars: BTreeSet<String> = BTreeSet::new();
    rec.for_each_field(&mut |key, _| {
        if let Some((var, prop)) = key.split_once('.') {
            if prop == "__id" || prop == "__src" {
                vars.insert(var.to_string());
            }
        }
    });
    vars.into_iter().collect()
}

pub(in crate::cypher::executor) fn exec_project(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::ReturnItem],
    emit_compound: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &records {
        let mut projected = NamedRecord::new();
        for item in items {
            match &item.expr.kind {
                ExprKind::Star => {
                    if emit_compound {
                        // Final RETURN * — emit one compound column per bound
                        // variable (plus any non-binding user-visible scalars).
                        let bound_vars = compound_binding_vars(rec);
                        for var in &bound_vars {
                            if let Some(compound) = build_compound_binding(rec, var) {
                                projected.set(var.clone(), compound);
                            }
                        }
                        for (key, val) in &rec.fields {
                            if let Some((owner, prop)) = key.split_once('.') {
                                // Dotted key: skip internal fields and fields
                                // belonging to compound-bound variables.
                                if prop.starts_with("__") {
                                    continue;
                                }
                                if bound_vars.iter().any(|v| v == owner) {
                                    continue;
                                }
                                projected.set(key.clone(), val.clone());
                            } else {
                                // Bare key: emit if it's a scalar value from
                                // WITH/UNWIND (no accompanying __id metadata)
                                // and not already emitted as a compound binding.
                                if !bound_vars.iter().any(|v| v == key) {
                                    let has_id = rec.get(&format!("{key}.__id")).is_some();
                                    if !has_id {
                                        projected.set(key.clone(), val.clone());
                                    }
                                }
                            }
                        }
                    } else {
                        // Intermediate WITH * — preserve flat shape so
                        // downstream pattern-matching / joins / ORDER BY keep
                        // working against `var.__id` / `var.prop` fields.
                        for (key, val) in &rec.fields {
                            projected.set(key.clone(), val.clone());
                        }
                    }
                }
                ExprKind::Variable(var) => {
                    let col_name = item.alias.clone().unwrap_or_else(|| var.clone());
                    if emit_compound {
                        if let Some(compound) = build_compound_binding(rec, var) {
                            projected.set(col_name, compound);
                        } else if let Some(existing) = rec.get(&col_name) {
                            projected.set(col_name, existing.clone());
                        } else {
                            let val =
                                eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                            projected.set(col_name, val);
                        }
                    } else {
                        // Intermediate: carry forward the flat binding shape so
                        // downstream operators can still access `var.prop` and
                        // `var.__id`. Rename prefixes when an alias was given.
                        let src_prefix = format!("{var}.");
                        let dst_prefix = format!("{col_name}.");
                        let mut propagated_any = false;
                        for (key, val) in &rec.fields {
                            if let Some(rest) = key.strip_prefix(&src_prefix) {
                                projected.set(format!("{dst_prefix}{rest}"), val.clone());
                                propagated_any = true;
                            }
                        }
                        // Also propagate the bare variable column if present
                        // (used by some operators as a compact id reference).
                        if let Some(existing) = rec.get(var) {
                            projected.set(col_name.clone(), existing.clone());
                            propagated_any = true;
                        }
                        if !propagated_any {
                            let val =
                                eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                            projected.set(col_name, val);
                        }
                    }
                }
                _ => {
                    let col_name = item
                        .alias
                        .clone()
                        .unwrap_or_else(|| expr_to_column_name(&item.expr));
                    let expr_col = expr_to_column_name(&item.expr);
                    // Check for deleted entity access before any cache lookup.
                    // Property access on a deleted entity must raise an error
                    // even if the value is still in the record.
                    if let ExprKind::Property(var, prop) = &item.expr.kind {
                        if rec.get(&format!("{var}.__deleted")) == Some(&Value::Bool(true)) {
                            return Err(GraphError::Query(
                                crate::types::QueryError::EntityNotFound {
                                    phase: crate::types::QueryPhase::Runtime,
                                    message: format!(
                                        "DeletedEntityAccess: cannot access property `{prop}` on deleted entity `{var}`"
                                    ),
                                    code: ErrorCode::Other,
                                    hint: None,
                                    span: None,
                                },
                            ));
                        }
                    }
                    // Check if the col_name collides with a MATCH variable binding
                    // (which stores raw node IDs). MATCH variables always have
                    // accompanying `var.__id` metadata; aggregate results don't.
                    let is_match_binding = rec.get(&format!("{col_name}.__id")).is_some();
                    let val = if !is_match_binding {
                        // First try the expression's natural column name — this
                        // is the key used by the Aggregate operator for group
                        // keys and the only safe name to match (it can never
                        // collide with an upstream variable).
                        if let Some(existing) = rec.get(&expr_col) {
                            existing.clone()
                        } else if col_name == expr_col {
                            // No alias rename — safe to check col_name too
                            // (already checked above, so this is a miss → eval).
                            eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?
                        } else if let Some(existing) = rec.get(&col_name) {
                            // Alias differs from expression name. The col_name
                            // value in the record may be a pre-computed aggregate
                            // result (stored under alias by agg_col_name) or a
                            // stale upstream variable. Aggregate results are
                            // stored under the alias, so check for that.
                            // Heuristic: if the expression is an aggregate
                            // function, trust the cached value; otherwise
                            // evaluate to avoid shadowing bugs.
                            let is_agg = matches!(
                                &item.expr.kind,
                                ExprKind::FunctionCall { name, .. }
                                    if matches!(name.to_ascii_lowercase().as_str(),
                                        "count" | "sum" | "avg" | "min" | "max" | "collect"
                                        | "percentiledisc" | "percentilecont" | "stdev" | "stdevp")
                            );
                            if is_agg {
                                existing.clone()
                            } else {
                                eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?
                            }
                        } else {
                            eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?
                        }
                    } else {
                        eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?
                    };
                    projected.set(col_name, val);
                }
            }
        }
        // Charge each projected row as it is built, so a projection that
        // creates large values (lists) fails at the row that crosses the
        // budget instead of after every row exists.
        charge_materialized(approx_named_bytes(&projected))?;
        results.push(projected);
    }

    Ok(results)
}

pub(in crate::cypher::executor) fn exec_unwind(
    conn: &Connection,
    input: &LogicalOp,
    expr: &Expr,
    alias: &str,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    unwind_records(conn, records, expr, alias, &NamedRecord::new(), ctx)
}

/// Apply `UNWIND expr AS alias` to `records`. `expr` is evaluated over the
/// correlated `outer` row merged with each record (`outer` is empty at top
/// level).
pub(in crate::cypher::executor) fn unwind_records(
    conn: &Connection,
    records: Vec<NamedRecord>,
    expr: &Expr,
    alias: &str,
    outer: &NamedRecord,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut results = Vec::new();
    // Stop building rows as soon as they pass the row cap (read-only
    // queries) or the materialization budget, not after the whole product
    // of input rows and list items exists.
    let row_cap = unwind_row_cap(ctx);

    for rec in &records {
        let cx = crate::cypher::eval::EvalCx::new(conn);
        let items = if outer.is_empty() {
            crate::cypher::eval::eval_unwind_items(expr, rec, cx)?
        } else {
            crate::cypher::eval::eval_unwind_items(expr, &merge_outer(outer, rec), cx)?
        };
        for item in items {
            let mut new_rec = rec.clone();
            // Expand nodes/edges into flat bindings for property access.
            match &item {
                Value::Node(n) => {
                    populate_node_bindings(&mut new_rec, n, alias);
                }
                Value::Edge(e) => {
                    new_rec.set(alias.to_string(), Value::Edge(e.clone()));
                    populate_edge_bindings(&mut new_rec, e, alias);
                }
                _ => {
                    new_rec.set(alias.to_string(), item);
                }
            }
            charge_materialized(approx_named_bytes(&new_rec))?;
            results.push(new_rec);
            row_count_within_cap(results.len(), row_cap)?;
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{Expr, ExprKind, LiteralValue};
    use crate::cypher::ir::FullTextOp;

    #[test]
    fn exec_fulltext_lookup_writes_fts_score_flat_key() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        let mut props = std::collections::HashMap::new();
        props.insert(
            "body".to_string(),
            Value::String("rust systems programming".to_string()),
        );
        crate::storage::node::create_node(&conn, &["Doc".to_string()], props).unwrap();
        crate::storage::fts::create_fulltext_index(&conn, "Doc", "body").unwrap();

        let term_expr =
            Expr::synthetic(ExprKind::Literal(LiteralValue::String("rust".to_string())));
        let outer = NamedRecord::default();
        let records = exec_fulltext_lookup(
            &conn,
            "Doc",
            "n",
            "body",
            FullTextOp::Contains,
            false,
            &term_expr,
            None,
            &outer,
            None,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let score = records[0]
            .get("n.__fts_score")
            .expect("n.__fts_score flat key must be set");
        match score {
            Value::F64(s) => assert!(
                s.is_finite() && *s > 0.0,
                "score should be positive, got {s}"
            ),
            other => panic!("n.__fts_score was {other:?}, expected F64"),
        }
    }

    #[test]
    fn exec_fulltext_fallback_writes_zero_fts_score() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        let mut props = std::collections::HashMap::new();
        props.insert("body".to_string(), Value::String("ab cd".to_string()));
        crate::storage::node::create_node(&conn, &["Doc".to_string()], props).unwrap();
        crate::storage::fts::create_fulltext_index(&conn, "Doc", "body").unwrap();

        // Term length 2 is below the trigram floor → fallback path.
        let records = exec_fulltext_fallback(
            &conn,
            "Doc",
            "n",
            "body",
            FullTextOp::Contains,
            false,
            "ab",
            None,
            &NamedRecord::new(),
            None,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let score = records[0]
            .get("n.__fts_score")
            .expect("fallback must set n.__fts_score");
        assert_eq!(*score, Value::F64(0.0));
    }
}
