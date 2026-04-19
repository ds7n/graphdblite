use std::collections::HashMap;

use rusqlite::Connection;

use crate::cypher::ast::{Expr, LiteralValue, PatternElement};
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::ir::*;
use crate::cypher::record::Record;
use crate::edge;
use crate::index;
use crate::node;
use crate::types::{Direction, GraphError, NodeId, PathValue, Properties, Result, Value};

/// Execution context carrying runtime limits.
#[derive(Default)]
pub struct ExecContext {
    /// Maximum rows any operator may produce. 0 = unlimited.
    pub max_result_rows: usize,
}

/// Check that a result set hasn't exceeded the row cap.
fn check_row_limit(results: &[Record], ctx: &ExecContext) -> Result<()> {
    if ctx.max_result_rows > 0 && results.len() > ctx.max_result_rows {
        return Err(GraphError::constraint(format!(
            "result set exceeded maximum of {} rows",
            ctx.max_result_rows
        )));
    }
    Ok(())
}

/// Execute a logical plan against the database, producing result records.
///
/// For read-only plans, uses the pull-based iterator model so that pipeline
/// operators (Filter, Limit, Project) stream without full materialization.
pub fn execute(conn: &Connection, plan: &LogicalOp) -> Result<Vec<Record>> {
    if is_read_only(plan) {
        let mut iter = crate::cypher::iter::build_iter(conn, plan)?;
        crate::cypher::iter::collect_all(&mut *iter)
    } else {
        let result = exec(conn, plan, &ExecContext::default())?;
        // Write-only queries (no RETURN clause) should return empty results.
        // When a RETURN is present, the planner wraps the write op in a Project,
        // so the top-level op will be Project/Sort/Skip/Limit/etc., not a bare write.
        if is_bare_write(plan) {
            Ok(vec![])
        } else {
            Ok(result)
        }
    }
}

/// Check if the top-level plan is a bare write op (no RETURN projection).
fn is_bare_write(plan: &LogicalOp) -> bool {
    matches!(
        plan,
        LogicalOp::CreateNode { .. }
            | LogicalOp::CreateEdge { .. }
            | LogicalOp::CreateSequence { .. }
            | LogicalOp::MatchCreate { .. }
            | LogicalOp::Delete { .. }
            | LogicalOp::SetProperty { .. }
            | LogicalOp::Remove { .. }
            | LogicalOp::Merge { .. }
            | LogicalOp::MatchMerge { .. }
    )
}

/// Execute with an explicit context carrying runtime limits.
pub fn execute_with_ctx(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let result = exec(conn, plan, ctx)?;
    if is_bare_write(plan) {
        Ok(vec![])
    } else {
        Ok(result)
    }
}

/// Check whether a plan tree contains only read-only operators.
fn is_read_only(plan: &LogicalOp) -> bool {
    match plan {
        LogicalOp::Scan { .. }
        | LogicalOp::IndexLookup { .. }
        | LogicalOp::EmptyRow
        | LogicalOp::SingleRow => true,

        LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Distinct { input }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Skip { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::Unwind { input, .. }
        | LogicalOp::Aggregate { input, .. }
        | LogicalOp::ShortestPath { input, .. }
        | LogicalOp::MaterializePath { input, .. } => is_read_only(input),

        LogicalOp::Expand { input, .. } => is_read_only(input),

        LogicalOp::CrossProduct { left, right }
        | LogicalOp::CorrelatedJoin {
            input: left, right, ..
        }
        | LogicalOp::LeftOuterJoin {
            input: left, right, ..
        } => is_read_only(left) && is_read_only(right),

        LogicalOp::Union { inputs, .. } => inputs.iter().all(is_read_only),

        // Write operations.
        LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::CreateSequence { .. }
        | LogicalOp::MatchCreate { .. }
        | LogicalOp::Delete { .. }
        | LogicalOp::SetProperty { .. }
        | LogicalOp::Remove { .. }
        | LogicalOp::Merge { .. }
        | LogicalOp::MatchMerge { .. } => false,
    }
}

fn exec(conn: &Connection, plan: &LogicalOp, ctx: &ExecContext) -> Result<Vec<Record>> {
    match plan {
        LogicalOp::SingleRow => Ok(vec![Record::new()]),

        LogicalOp::EmptyRow => Ok(vec![Record::new()]),

        LogicalOp::Scan { label, alias } => exec_scan(conn, label, alias, ctx),

        LogicalOp::IndexLookup {
            label,
            alias,
            property,
            value,
            remaining_filters,
        } => exec_index_lookup(
            conn,
            label,
            alias,
            property,
            value,
            remaining_filters.as_ref(),
        ),

        LogicalOp::Expand {
            input,
            src_alias,
            dst_alias,
            rel_alias,
            edge_types,
            direction,
            min_hops,
            max_hops,
        } => exec_expand(
            conn,
            input,
            src_alias,
            dst_alias,
            rel_alias.as_deref(),
            edge_types,
            *direction,
            *min_hops,
            *max_hops,
            ctx,
        ),

        LogicalOp::CrossProduct { left, right } => exec_cross_product(conn, left, right, ctx),

        LogicalOp::Filter { input, predicate } => exec_filter(conn, input, predicate, ctx),

        LogicalOp::Project {
            input,
            items,
            emit_compound,
        } => exec_project(conn, input, items, *emit_compound, ctx),

        LogicalOp::Aggregate {
            input,
            group_keys,
            aggregates,
        } => exec_aggregate(conn, input, group_keys, aggregates, ctx),

        LogicalOp::Distinct { input } => exec_distinct(conn, input, ctx),

        LogicalOp::Sort { input, items } => exec_sort(conn, input, items, ctx),

        LogicalOp::Skip { input, count } => exec_skip(conn, input, *count, ctx),

        LogicalOp::Limit { input, count } => exec_limit(conn, input, *count, ctx),

        LogicalOp::CreateNode {
            labels,
            alias,
            properties,
        } => exec_create_node(conn, labels, alias.as_deref(), properties),

        LogicalOp::CreateEdge {
            src_alias,
            dst_alias,
            edge_type,
            properties,
        } => exec_create_edge(conn, src_alias, dst_alias, edge_type, properties),

        LogicalOp::CreateSequence { ops } => exec_create_sequence(conn, ops),

        LogicalOp::MatchCreate { input, create_ops } => {
            exec_match_create(conn, input, create_ops, ctx)
        }

        LogicalOp::Delete {
            input,
            variables,
            detach,
        } => exec_delete(conn, input, variables, *detach, ctx),

        LogicalOp::SetProperty { input, assignments } => {
            exec_set_property(conn, input, assignments, ctx)
        }

        LogicalOp::Remove { input, items } => exec_remove(conn, input, items, ctx),

        LogicalOp::Merge {
            pattern,
            on_create,
            on_match,
        } => exec_merge(conn, pattern, on_create, on_match),

        LogicalOp::MatchMerge {
            input,
            merge_pattern,
            on_create,
            on_match,
        } => exec_match_merge(conn, input, merge_pattern, on_create, on_match, ctx),

        LogicalOp::Unwind { input, expr, alias } => exec_unwind(conn, input, expr, alias, ctx),

        LogicalOp::MaterializePath {
            input,
            path_alias,
            node_aliases,
            rel_aliases,
        } => exec_materialize_path(conn, input, path_alias, node_aliases, rel_aliases, ctx),

        LogicalOp::CorrelatedJoin { input, right } => exec_correlated_join(conn, input, right, ctx),

        LogicalOp::LeftOuterJoin {
            input,
            right,
            optional_aliases,
        } => exec_left_outer_join(conn, input, right, optional_aliases, ctx),

        LogicalOp::ShortestPath {
            input,
            src_alias,
            dst_alias,
            path_alias,
            edge_type,
            direction,
            max_hops,
            all_paths,
        } => exec_shortest_path(
            conn,
            input,
            src_alias,
            dst_alias,
            path_alias,
            edge_type.as_deref(),
            *direction,
            *max_hops,
            *all_paths,
            ctx,
        ),

        LogicalOp::Union { inputs, all } => {
            let mut results = Vec::new();
            for input in inputs {
                results.extend(exec(conn, input, ctx)?);
            }
            if !all {
                // Deduplicate for plain UNION.
                let mut seen = Vec::new();
                results.retain(|rec| {
                    if seen.iter().any(|s: &Record| s.fields == rec.fields) {
                        false
                    } else {
                        seen.push(rec.clone());
                        true
                    }
                });
            }
            Ok(results)
        }
    }
}

fn exec_scan(
    conn: &Connection,
    label: &str,
    alias: &str,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let nodes = node::find_nodes_by_label(conn, label)?;
    let mut records = Vec::with_capacity(nodes.len());
    for n in nodes {
        records.push(node_to_record(&n, alias));
    }
    check_row_limit(&records, ctx)?;
    Ok(records)
}

fn exec_index_lookup(
    conn: &Connection,
    label: &str,
    alias: &str,
    property: &str,
    value: &LiteralValue,
    remaining_filters: Option<&Expr>,
) -> Result<Vec<Record>> {
    let lookup_value = literal_to_value(value);
    let node_ids = index::index_lookup(conn, label, property, &lookup_value)?;
    let mut records = Vec::new();

    for id in node_ids {
        let n = node::get_node(conn, id)?;
        let rec = node_to_record(&n, alias);

        if let Some(filter) = remaining_filters {
            if !eval_predicate(filter, &rec, conn)? {
                continue;
            }
        }

        records.push(rec);
    }

    Ok(records)
}

#[allow(clippy::too_many_arguments)]
fn exec_expand(
    conn: &Connection,
    input: &LogicalOp,
    src_alias: &str,
    dst_alias: &str,
    rel_alias: Option<&str>,
    edge_types: &[String],
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let input_records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &input_records {
        let src_id = match rec.get(src_alias) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };

        // If no types specified, discover all edge types for this node.
        let _owned_labels: Vec<String>;
        let labels: Vec<&str> = if edge_types.is_empty() {
            let all = edge::get_all_edge_labels(conn, src_id, direction)?;
            _owned_labels = all.into_iter().map(|(l, _)| l).collect();
            _owned_labels.iter().map(|s| s.as_str()).collect()
        } else {
            edge_types.iter().map(|s| s.as_str()).collect()
        };

        for &label in &labels {
            if min_hops == 1 && max_hops == 1 {
                // Single hop — direct neighbor lookup.
                let neighbors = edge::get_neighbors(conn, src_id, label, direction)?;
                for dst_id in neighbors {
                    let dst_node = node::get_node(conn, dst_id)?;
                    let mut new_rec = rec.clone();
                    new_rec.set(dst_alias.to_string(), Value::I64(dst_id.0 as i64));
                    for (key, val) in &dst_node.properties {
                        new_rec.set(format!("{dst_alias}.{key}"), val.clone());
                    }
                    new_rec.set(
                        format!("{dst_alias}.__label"),
                        Value::String(dst_node.labels.join(":")),
                    );
                    new_rec.set(
                        format!("{dst_alias}.__labels"),
                        Value::List(
                            dst_node
                                .labels
                                .iter()
                                .map(|l| Value::String(l.clone()))
                                .collect(),
                        ),
                    );
                    new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                    // Bind relationship properties and identity when a rel variable is present.
                    if let Some(r_alias) = rel_alias {
                        let (edge_src, edge_dst) = match direction {
                            Direction::Incoming => (dst_id, src_id),
                            _ => (src_id, dst_id),
                        };
                        new_rec.set(format!("{r_alias}.__src"), Value::I64(edge_src.0 as i64));
                        new_rec.set(format!("{r_alias}.__dst"), Value::I64(edge_dst.0 as i64));
                        new_rec.set(
                            format!("{r_alias}.__type"),
                            Value::String(label.to_string()),
                        );
                        if let Ok(props) =
                            edge::get_edge_properties(conn, edge_src, edge_dst, label)
                        {
                            for (key, val) in &props {
                                new_rec.set(format!("{r_alias}.{key}"), val.clone());
                            }
                        }
                    }
                    results.push(new_rec);
                }
            } else {
                // Variable-length traversal.
                let reachable = edge::traverse(conn, src_id, label, direction, min_hops, max_hops)?;
                for dst_id in reachable {
                    let dst_node = node::get_node(conn, dst_id)?;
                    let mut new_rec = rec.clone();
                    new_rec.set(dst_alias.to_string(), Value::I64(dst_id.0 as i64));
                    for (key, val) in &dst_node.properties {
                        new_rec.set(format!("{dst_alias}.{key}"), val.clone());
                    }
                    new_rec.set(
                        format!("{dst_alias}.__label"),
                        Value::String(dst_node.labels.join(":")),
                    );
                    new_rec.set(
                        format!("{dst_alias}.__labels"),
                        Value::List(
                            dst_node
                                .labels
                                .iter()
                                .map(|l| Value::String(l.clone()))
                                .collect(),
                        ),
                    );
                    new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                    results.push(new_rec);
                }
            }
        } // end for &label in &labels
    }

    check_row_limit(&results, ctx)?;
    Ok(results)
}

fn exec_cross_product(
    conn: &Connection,
    left: &LogicalOp,
    right: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    // Materialize only the left side. Re-execute the right side per left
    // record so peak memory is O(left + right + output) instead of
    // O(left * right).
    let left_records = exec(conn, left, ctx)?;
    let mut results = Vec::new();
    for l in &left_records {
        let right_records = exec(conn, right, ctx)?;
        for r in &right_records {
            let mut combined = l.clone();
            for (key, val) in &r.fields {
                combined.set(key.clone(), val.clone());
            }
            results.push(combined);
            if ctx.max_result_rows > 0 && results.len() > ctx.max_result_rows {
                return Err(GraphError::constraint(format!(
                    "cross product exceeded maximum of {} rows",
                    ctx.max_result_rows
                )));
            }
        }
    }
    Ok(results)
}

fn exec_filter(
    conn: &Connection,
    input: &LogicalOp,
    predicate: &Expr,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();
    for rec in records {
        if eval_predicate(predicate, &rec, conn)? {
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
pub(crate) fn build_compound_binding(rec: &Record, var: &str) -> Option<Value> {
    use crate::types::{Edge, Node, Properties};

    let prefix = format!("{var}.");

    // Edge binding: has __src / __dst / __type metadata.
    let src_key = format!("{var}.__src");
    let dst_key = format!("{var}.__dst");
    let type_key = format!("{var}.__type");
    if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) =
        (rec.get(&src_key), rec.get(&dst_key), rec.get(&type_key))
    {
        let mut properties = Properties::new();
        for (key, val) in &rec.fields {
            if let Some(prop) = key.strip_prefix(&prefix) {
                if !prop.starts_with("__") {
                    properties.insert(prop.to_string(), val.clone());
                }
            }
        }
        return Some(Value::Edge(Edge {
            src: NodeId(*src as u64),
            dst: NodeId(*dst as u64),
            label: label.clone(),
            properties,
        }));
    }

    // Node binding: has __id / __label metadata.
    let id_key = format!("{var}.__id");
    let label_key = format!("{var}.__label");
    if let (Some(Value::I64(id)), Some(Value::String(label_str))) =
        (rec.get(&id_key), rec.get(&label_key))
    {
        let mut properties = Properties::new();
        for (key, val) in &rec.fields {
            if let Some(prop) = key.strip_prefix(&prefix) {
                if !prop.starts_with("__") {
                    properties.insert(prop.to_string(), val.clone());
                }
            }
        }
        // Reconstruct labels from the colon-joined __label string.
        let labels: Vec<String> = if label_str.is_empty() {
            Vec::new()
        } else {
            label_str.split(':').map(|s| s.to_string()).collect()
        };
        return Some(Value::Node(Node {
            id: NodeId(*id as u64),
            labels,
            properties,
        }));
    }

    None
}

/// Collect the set of variable names in a record that are bound as compound
/// entities (nodes or edges). Used by `RETURN *` to know which prefixes to
/// fold into compound columns rather than emitting as flat properties.
pub(crate) fn compound_binding_vars(rec: &Record) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut vars: BTreeSet<String> = BTreeSet::new();
    for key in rec.fields.keys() {
        if let Some((var, prop)) = key.split_once('.') {
            if prop == "__id" || prop == "__src" {
                vars.insert(var.to_string());
            }
        }
    }
    vars.into_iter().collect()
}

fn exec_project(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::ReturnItem],
    emit_compound: bool,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &records {
        let mut projected = Record::new();
        for item in items {
            match &item.expr {
                Expr::Star => {
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
                            if !is_user_visible_field(key) {
                                continue;
                            }
                            let owner = key.split_once('.').map(|(v, _)| v);
                            if let Some(owner) = owner {
                                if bound_vars.iter().any(|v| v == owner) {
                                    continue;
                                }
                            }
                            projected.set(key.clone(), val.clone());
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
                Expr::Variable(var) => {
                    let col_name = item.alias.clone().unwrap_or_else(|| var.clone());
                    if emit_compound {
                        if let Some(compound) = build_compound_binding(rec, var) {
                            projected.set(col_name, compound);
                        } else if let Some(existing) = rec.get(&col_name) {
                            projected.set(col_name, existing.clone());
                        } else {
                            let val = eval_expr(&item.expr, rec, conn)?;
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
                            let val = eval_expr(&item.expr, rec, conn)?;
                            projected.set(col_name, val);
                        }
                    }
                }
                _ => {
                    let col_name = item
                        .alias
                        .clone()
                        .unwrap_or_else(|| expr_to_column_name(&item.expr));
                    // Check if the col_name collides with a MATCH variable binding
                    // (which stores raw node IDs). MATCH variables always have
                    // accompanying `var.__id` metadata; aggregate results don't.
                    let is_match_binding = rec.get(&format!("{col_name}.__id")).is_some();
                    let val = if !is_match_binding {
                        if let Some(existing) = rec.get(&col_name) {
                            existing.clone()
                        } else {
                            eval_expr(&item.expr, rec, conn)?
                        }
                    } else {
                        eval_expr(&item.expr, rec, conn)?
                    };
                    projected.set(col_name, val);
                }
            }
        }
        results.push(projected);
    }

    Ok(results)
}

/// Returns true if a record field should be visible to the user.
/// Filters out bare alias keys (no dot — raw node IDs) and internal `__` properties.
pub(crate) fn is_user_visible_field(key: &str) -> bool {
    match key.split_once('.') {
        Some((_, prop)) => !prop.starts_with("__"),
        None => false, // bare alias like "n" is the raw node ID — hide it
    }
}

fn exec_aggregate(
    conn: &Connection,
    input: &LogicalOp,
    group_keys: &[Expr],
    aggregates: &[AggregateExpr],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;

    if group_keys.is_empty() {
        // No grouping — aggregate over all records.
        let mut rec = Record::new();
        for agg in aggregates {
            let col_name = agg_col_name(agg);
            let val = compute_aggregate(agg, &records, conn)?;
            rec.set(col_name, val);
        }
        return Ok(vec![rec]);
    }

    // Group by keys using a HashMap for O(1) group lookup.
    // IndexMap would preserve insertion order, but we use a separate Vec
    // to track key order so we don't need an extra dependency.
    let mut group_map: HashMap<Vec<Value>, Vec<Record>> = HashMap::new();
    let mut key_order: Vec<Vec<Value>> = Vec::new();

    for rec in &records {
        let key_vals: Vec<Value> = group_keys
            .iter()
            .map(|k| eval_expr(k, rec, conn).unwrap_or(Value::Null))
            .collect();

        if let Some(group) = group_map.get_mut(&key_vals) {
            group.push(rec.clone());
        } else {
            key_order.push(key_vals.clone());
            group_map.insert(key_vals, vec![rec.clone()]);
        }
    }

    let mut results = Vec::new();
    for key_vals in &key_order {
        let group_records = &group_map[key_vals];
        let mut rec = Record::new();
        for (i, key_expr) in group_keys.iter().enumerate() {
            let col_name = expr_to_column_name(key_expr);
            rec.set(col_name.clone(), key_vals[i].clone());
            // If the group key is a bare variable referring to a node/relationship,
            // propagate its flattened property keys (e.g. `n.name`, `n.__id`) from
            // a representative record so that downstream clauses like
            // `RETURN n.name` continue to work after WITH/aggregation.
            if let Expr::Variable(var) = key_expr {
                let prefix = format!("{var}.");
                if let Some(first) = group_records.first() {
                    for (key, val) in &first.fields {
                        if key.starts_with(&prefix) {
                            rec.set(key.clone(), val.clone());
                        }
                    }
                }
            }
        }
        for agg in aggregates {
            let col_name = agg_col_name(agg);
            let val = compute_aggregate(agg, group_records, conn)?;
            rec.set(col_name, val);
        }
        results.push(rec);
    }

    Ok(results)
}

fn compute_aggregate(agg: &AggregateExpr, records: &[Record], conn: &Connection) -> Result<Value> {
    match agg.function {
        AggregateFunction::Count => {
            if matches!(agg.input, Expr::Star) {
                Ok(Value::I64(records.len() as i64))
            } else {
                let count = records
                    .iter()
                    .filter(|r| !matches!(eval_expr(&agg.input, r, conn), Ok(Value::Null)))
                    .count();
                Ok(Value::I64(count as i64))
            }
        }
        AggregateFunction::Sum => {
            let mut i64_sum: i64 = 0;
            let mut f64_sum: f64 = 0.0;
            let mut all_integer = true;
            for rec in records {
                match eval_expr(&agg.input, rec, conn)? {
                    Value::I64(n) => {
                        i64_sum = i64_sum.wrapping_add(n);
                        f64_sum += n as f64;
                    }
                    Value::F64(n) => {
                        all_integer = false;
                        f64_sum += n;
                    }
                    _ => {}
                }
            }
            if all_integer {
                Ok(Value::I64(i64_sum))
            } else {
                Ok(Value::F64(f64_sum))
            }
        }
        AggregateFunction::Avg => {
            let mut sum = 0.0f64;
            let mut count = 0;
            for rec in records {
                match eval_expr(&agg.input, rec, conn)? {
                    Value::I64(n) => {
                        sum += n as f64;
                        count += 1;
                    }
                    Value::F64(n) => {
                        sum += n;
                        count += 1;
                    }
                    _ => {}
                }
            }
            if count > 0 {
                Ok(Value::F64(sum / count as f64))
            } else {
                Ok(Value::Null)
            }
        }
        AggregateFunction::Min => {
            let mut min: Option<Value> = None;
            for rec in records {
                let val = eval_expr(&agg.input, rec, conn)?;
                if !matches!(val, Value::Null) {
                    min = Some(match min {
                        None => val,
                        Some(ref current) => {
                            if value_less_than(&val, current) {
                                val
                            } else {
                                current.clone()
                            }
                        }
                    });
                }
            }
            Ok(min.unwrap_or(Value::Null))
        }
        AggregateFunction::Max => {
            let mut max: Option<Value> = None;
            for rec in records {
                let val = eval_expr(&agg.input, rec, conn)?;
                if !matches!(val, Value::Null) {
                    max = Some(match max {
                        None => val,
                        Some(ref current) => {
                            if value_less_than(current, &val) {
                                val
                            } else {
                                current.clone()
                            }
                        }
                    });
                }
            }
            Ok(max.unwrap_or(Value::Null))
        }
        AggregateFunction::Collect => {
            let mut items = Vec::new();
            for rec in records {
                let val = eval_expr(&agg.input, rec, conn)?;
                if !matches!(val, Value::Null) {
                    items.push(val);
                }
            }
            Ok(Value::List(items))
        }
    }
}

fn exec_sort(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::SortItem],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let mut records = exec(conn, input, ctx)?;
    records.sort_by(|a, b| {
        for item in items {
            let va = eval_expr(&item.expr, a, conn).unwrap_or(Value::Null);
            let vb = eval_expr(&item.expr, b, conn).unwrap_or(Value::Null);
            let ord = compare_values_for_sort(&va, &vb);
            let ord = if item.descending { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(records)
}

fn exec_distinct(conn: &Connection, input: &LogicalOp, ctx: &ExecContext) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let mut seen = Vec::new();
    let mut results = Vec::new();
    for rec in records {
        if !seen.iter().any(|s: &Record| s.fields == rec.fields) {
            seen.push(rec.clone());
            results.push(rec);
        }
    }
    Ok(results)
}

fn exec_skip(
    conn: &Connection,
    input: &LogicalOp,
    count: u64,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    Ok(records.into_iter().skip(count as usize).collect())
}

fn exec_limit(
    conn: &Connection,
    input: &LogicalOp,
    count: u64,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    Ok(records.into_iter().take(count as usize).collect())
}

fn exec_create_node(
    conn: &Connection,
    labels: &[String],
    alias: Option<&str>,
    properties: &HashMap<String, Expr>,
) -> Result<Vec<Record>> {
    let mut props = Properties::new();
    let dummy_rec = Record::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, &dummy_rec, conn)?;
        props.insert(key.clone(), val);
    }

    let id = node::create_node(conn, labels, props.clone())?;
    let primary_label = labels.first().map(|s| s.as_str()).unwrap_or("");
    index::update_indexes_for_node(conn, id, primary_label, None, &props)?;

    let mut rec = Record::new();
    if let Some(alias) = alias {
        rec.set(alias.to_string(), Value::I64(id.0 as i64));
        rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
    }
    Ok(vec![rec])
}

fn exec_create_edge(
    _conn: &Connection,
    _src_alias: &str,
    _dst_alias: &str,
    _edge_type: &str,
    _properties: &HashMap<String, Expr>,
) -> Result<Vec<Record>> {
    // Standalone edge creation is handled by exec_create_sequence.
    // This path is only reached for isolated CreateEdge ops (shouldn't happen in practice).
    Ok(vec![])
}

fn exec_create_sequence(conn: &Connection, ops: &[LogicalOp]) -> Result<Vec<Record>> {
    // Track variable → NodeId bindings for edge creation.
    let mut bindings: HashMap<String, NodeId> = HashMap::new();
    let mut last_record = Record::new();

    for op in ops {
        match op {
            LogicalOp::CreateNode {
                labels,
                alias,
                properties,
            } => {
                let mut props = Properties::new();
                let dummy_rec = Record::new();
                for (key, expr) in properties {
                    let val = eval_expr(expr, &dummy_rec, conn)?;
                    if val == Value::Null {
                        continue;
                    }
                    props.insert(key.clone(), val);
                }
                let id = node::create_node(conn, labels, props.clone())?;
                let primary_label = labels.first().map(|s| s.as_str()).unwrap_or("");
                index::update_indexes_for_node(conn, id, primary_label, None, &props)?;
                if let Some(alias) = alias {
                    bindings.insert(alias.clone(), id);
                    last_record.set(alias.clone(), Value::I64(id.0 as i64));
                    last_record.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                }
            }
            LogicalOp::CreateEdge {
                src_alias,
                dst_alias,
                edge_type,
                properties,
            } => {
                let src = bindings.get(src_alias).ok_or_else(|| {
                    GraphError::semantic(format!("unbound variable: {src_alias}"))
                })?;
                let dst = bindings.get(dst_alias).ok_or_else(|| {
                    GraphError::semantic(format!("unbound variable: {dst_alias}"))
                })?;
                let mut props = Properties::new();
                let dummy_rec = Record::new();
                for (key, expr) in properties {
                    let val = eval_expr(expr, &dummy_rec, conn)?;
                    if val == Value::Null {
                        continue;
                    }
                    props.insert(key.clone(), val);
                }
                edge::create_edge(conn, *src, *dst, edge_type, props)?;
            }
            _ => {
                // Shouldn't happen in a CreateSequence.
            }
        }
    }

    Ok(vec![last_record])
}

fn exec_match_create(
    conn: &Connection,
    input: &LogicalOp,
    create_ops: &[LogicalOp],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let mut result = Vec::with_capacity(records.len());

    for rec in &records {
        // Seed bindings from MATCH-bound variables (var → NodeId).
        let mut bindings: HashMap<String, NodeId> = HashMap::new();
        for (key, val) in &rec.fields {
            if !key.contains('.') {
                if let Value::I64(id) = val {
                    bindings.insert(key.clone(), NodeId(*id as u64));
                }
            }
        }

        // Start with a copy of the input record so MATCH-bound vars are available.
        let mut out_rec = rec.clone();

        // Execute each CREATE op using the bindings.
        for op in create_ops {
            match op {
                LogicalOp::CreateNode {
                    labels,
                    alias,
                    properties,
                } => {
                    // Skip if this alias is already bound from the MATCH pipeline.
                    if let Some(a) = alias {
                        if bindings.contains_key(a) {
                            continue;
                        }
                    }
                    let mut props = Properties::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, rec, conn)?;
                        if val == Value::Null {
                            continue;
                        }
                        props.insert(key.clone(), val);
                    }
                    let id = node::create_node(conn, labels, props.clone())?;
                    let primary_label = labels.first().map(|s| s.as_str()).unwrap_or("");
                    index::update_indexes_for_node(conn, id, primary_label, None, &props)?;
                    if let Some(alias) = alias {
                        bindings.insert(alias.clone(), id);
                        out_rec.set(alias.clone(), Value::I64(id.0 as i64));
                        out_rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                    }
                }
                LogicalOp::CreateEdge {
                    src_alias,
                    dst_alias,
                    edge_type,
                    properties,
                } => {
                    let src = bindings.get(src_alias).ok_or_else(|| {
                        GraphError::semantic(format!("unbound variable: {src_alias}"))
                    })?;
                    let dst = bindings.get(dst_alias).ok_or_else(|| {
                        GraphError::semantic(format!("unbound variable: {dst_alias}"))
                    })?;
                    let mut props = Properties::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, rec, conn)?;
                        if val == Value::Null {
                            continue;
                        }
                        props.insert(key.clone(), val);
                    }
                    edge::create_edge(conn, *src, *dst, edge_type, props)?;
                }
                _ => {}
            }
        }

        result.push(out_rec);
    }

    Ok(result)
}

fn exec_delete(
    conn: &Connection,
    input: &LogicalOp,
    variables: &[String],
    detach: bool,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    for rec in &records {
        for var in variables {
            // Check if this is a relationship variable (has edge identity metadata).
            let edge_src_key = format!("{var}.__src");
            let edge_dst_key = format!("{var}.__dst");
            let edge_type_key = format!("{var}.__type");
            if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
                rec.get(&edge_src_key),
                rec.get(&edge_dst_key),
                rec.get(&edge_type_key),
            ) {
                edge::delete_edge(conn, NodeId(*src as u64), NodeId(*dst as u64), label)?;
            } else if let Some(Value::I64(id)) = rec.get(var) {
                let node_id = NodeId(*id as u64);
                if !detach && node::node_has_edges(conn, node_id)? {
                    return Err(GraphError::HasEdges(node_id));
                }
                node::delete_node(conn, node_id)?;
            }
            // Skip if value is Null (from OPTIONAL MATCH with no match).
        }
    }
    Ok(vec![])
}

fn exec_set_property(
    conn: &Connection,
    input: &LogicalOp,
    assignments: &[crate::cypher::ast::Assignment],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    for rec in &records {
        for assignment in assignments {
            let var = &assignment.variable;
            // Check if this is a relationship variable (has edge identity metadata).
            let edge_src_key = format!("{var}.__src");
            let edge_dst_key = format!("{var}.__dst");
            let edge_type_key = format!("{var}.__type");
            if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
                rec.get(&edge_src_key),
                rec.get(&edge_dst_key),
                rec.get(&edge_type_key),
            ) {
                let val = eval_expr(&assignment.value, rec, conn)?;
                edge::set_edge_property(
                    conn,
                    NodeId(*src as u64),
                    NodeId(*dst as u64),
                    label,
                    &assignment.property,
                    val,
                )?;
            } else if let Some(Value::I64(id)) = rec.get(var) {
                let node_id = NodeId(*id as u64);
                let old = node::get_node(conn, node_id)?;
                let val = eval_expr(&assignment.value, rec, conn)?;
                node::set_node_property(conn, node_id, &assignment.property, val.clone())?;
                let mut new_props = old.properties.clone();
                new_props.insert(assignment.property.clone(), val);
                index::update_indexes_for_node(
                    conn,
                    node_id,
                    old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                    Some(&old.properties),
                    &new_props,
                )?;
            }
        }
    }
    Ok(vec![])
}

fn exec_remove(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::RemoveItem],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        for item in items {
            match item {
                crate::cypher::ast::RemoveItem::Property { variable, property } => {
                    // Check if this is an edge variable.
                    let edge_src_key = format!("{variable}.__src");
                    let edge_dst_key = format!("{variable}.__dst");
                    let edge_type_key = format!("{variable}.__type");
                    if let (
                        Some(Value::I64(src)),
                        Some(Value::I64(dst)),
                        Some(Value::String(label)),
                    ) = (
                        rec.get(&edge_src_key),
                        rec.get(&edge_dst_key),
                        rec.get(&edge_type_key),
                    ) {
                        let src = *src;
                        let dst = *dst;
                        let label = label.clone();
                        // Remove edge property by setting to Null.
                        edge::set_edge_property(
                            conn,
                            NodeId(src as u64),
                            NodeId(dst as u64),
                            &label,
                            property,
                            Value::Null,
                        )?;
                        // Update record to reflect removal.
                        let prop_key = format!("{variable}.{property}");
                        rec.set(prop_key, Value::Null);
                    } else if let Some(Value::I64(id)) = rec.get(variable) {
                        let id = *id;
                        let node_id = NodeId(id as u64);
                        let old = node::get_node(conn, node_id)?;
                        node::remove_node_property(conn, node_id, property)?;
                        let mut new_props = old.properties.clone();
                        new_props.remove(property);
                        index::update_indexes_for_node(
                            conn,
                            node_id,
                            old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                            Some(&old.properties),
                            &new_props,
                        )?;
                        // Update record to reflect removal.
                        let prop_key = format!("{variable}.{property}");
                        rec.set(prop_key, Value::Null);
                    }
                }
                crate::cypher::ast::RemoveItem::Label { variable, labels } => {
                    if let Some(Value::I64(id)) = rec.get(variable) {
                        let id = *id;
                        let node_id = NodeId(id as u64);
                        for label in labels {
                            node::remove_node_label(conn, node_id, label)?;
                        }
                        // Update the labels in the record.
                        let label_key = format!("{variable}.__labels");
                        if let Some(Value::List(current_labels)) = rec.get(&label_key) {
                            let updated: Vec<Value> = current_labels
                                .iter()
                                .filter(|l| {
                                    if let Value::String(s) = l {
                                        !labels.contains(s)
                                    } else {
                                        true
                                    }
                                })
                                .cloned()
                                .collect();
                            rec.set(label_key, Value::List(updated));
                        }
                    }
                }
            }
        }
    }
    Ok(records)
}

fn exec_merge(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[crate::cypher::ast::Assignment],
    on_match: &[crate::cypher::ast::Assignment],
) -> Result<Vec<Record>> {
    if pattern.elements.len() == 1 {
        return exec_merge_node(conn, pattern, on_create, on_match);
    }
    // 3-element relationship MERGE: (a:L {p})-[:TYPE]->(b:L {p})
    exec_merge_relationship(conn, pattern, on_create, on_match)
}

/// Single-node MERGE: find-or-create a node matching the pattern.
fn exec_merge_node(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[crate::cypher::ast::Assignment],
    on_match: &[crate::cypher::ast::Assignment],
) -> Result<Vec<Record>> {
    let node_pat = match pattern.elements.first() {
        Some(PatternElement::Node(n)) => n,
        _ => unreachable!("MERGE pattern validated at plan time"),
    };

    let label = node_pat.labels.first().map(|s| s.as_str()).unwrap_or("");
    let alias = node_pat.variable.as_deref().unwrap_or("_merge");

    let matched = find_merge_match(conn, label, &node_pat.properties)?;

    match matched {
        Some(n) => {
            for assignment in on_match {
                let old = node::get_node(conn, n.id)?;
                let mut rec = Record::new();
                rec.set(assignment.variable.clone(), Value::I64(n.id.0 as i64));
                let val = eval_expr(&assignment.value, &rec, conn)?;
                node::set_node_property(conn, n.id, &assignment.property, val.clone())?;
                let mut new_props = old.properties.clone();
                new_props.insert(assignment.property.clone(), val);
                index::update_indexes_for_node(
                    conn,
                    n.id,
                    old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                    Some(&old.properties),
                    &new_props,
                )?;
            }
            let mut rec = Record::new();
            rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
            rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
            Ok(vec![rec])
        }
        None => {
            let mut props = Properties::new();
            let dummy_rec = Record::new();
            for (key, expr) in &node_pat.properties {
                let val = eval_expr(expr, &dummy_rec, conn)?;
                props.insert(key.clone(), val);
            }
            let labels: Vec<String> = if label.is_empty() {
                vec![]
            } else {
                vec![label.to_string()]
            };
            let id = node::create_node(conn, &labels, props.clone())?;
            index::update_indexes_for_node(conn, id, label, None, &props)?;

            for assignment in on_create {
                let old = node::get_node(conn, id)?;
                let mut rec = Record::new();
                rec.set(assignment.variable.clone(), Value::I64(id.0 as i64));
                let val = eval_expr(&assignment.value, &rec, conn)?;
                node::set_node_property(conn, id, &assignment.property, val.clone())?;
                let mut new_props = old.properties.clone();
                new_props.insert(assignment.property.clone(), val);
                index::update_indexes_for_node(
                    conn,
                    id,
                    old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                    Some(&old.properties),
                    &new_props,
                )?;
            }

            let mut rec = Record::new();
            rec.set(alias.to_string(), Value::I64(id.0 as i64));
            rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
            Ok(vec![rec])
        }
    }
}

/// Relationship MERGE: find-or-create nodes and the edge between them.
fn exec_merge_relationship(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[crate::cypher::ast::Assignment],
    on_match: &[crate::cypher::ast::Assignment],
) -> Result<Vec<Record>> {
    let src_pat = match &pattern.elements[0] {
        PatternElement::Node(n) => n,
        _ => unreachable!(),
    };
    let rel = match &pattern.elements[1] {
        PatternElement::Relationship(r) => r,
        _ => unreachable!(),
    };
    let dst_pat = match &pattern.elements[2] {
        PatternElement::Node(n) => n,
        _ => unreachable!(),
    };

    let src_label = src_pat.labels.first().map(|s| s.as_str()).unwrap_or("");
    let dst_label = dst_pat.labels.first().map(|s| s.as_str()).unwrap_or("");
    let edge_type = rel.rel_types.first().cloned().unwrap_or_default();

    // Find or create source and destination nodes.
    let src_id = find_or_create_merge_node(conn, src_label, &src_pat.properties)?;
    let dst_id = find_or_create_merge_node(conn, dst_label, &dst_pat.properties)?;

    // Find or create the edge.
    if !edge::edge_exists(conn, src_id, dst_id, &edge_type)? {
        let mut props = Properties::new();
        let dummy_rec = Record::new();
        for (key, expr) in &rel.properties {
            let val = eval_expr(expr, &dummy_rec, conn)?;
            props.insert(key.clone(), val);
        }
        edge::create_edge(conn, src_id, dst_id, &edge_type, props)?;
        for assignment in on_create {
            let mut rec = Record::new();
            if let Some(ref v) = src_pat.variable {
                rec.set(v.clone(), Value::I64(src_id.0 as i64));
            }
            if let Some(ref v) = dst_pat.variable {
                rec.set(v.clone(), Value::I64(dst_id.0 as i64));
            }
            let val = eval_expr(&assignment.value, &rec, conn)?;
            edge::set_edge_property(conn, src_id, dst_id, &edge_type, &assignment.property, val)?;
        }
    } else {
        for assignment in on_match {
            let mut rec = Record::new();
            if let Some(ref v) = src_pat.variable {
                rec.set(v.clone(), Value::I64(src_id.0 as i64));
            }
            if let Some(ref v) = dst_pat.variable {
                rec.set(v.clone(), Value::I64(dst_id.0 as i64));
            }
            let val = eval_expr(&assignment.value, &rec, conn)?;
            edge::set_edge_property(conn, src_id, dst_id, &edge_type, &assignment.property, val)?;
        }
    }

    let mut rec = Record::new();
    if let Some(ref v) = src_pat.variable {
        rec.set(v.clone(), Value::I64(src_id.0 as i64));
    }
    if let Some(ref v) = dst_pat.variable {
        rec.set(v.clone(), Value::I64(dst_id.0 as i64));
    }
    Ok(vec![rec])
}

/// Find a node matching the MERGE pattern properties, or create it if not found.
fn find_or_create_merge_node(
    conn: &Connection,
    label: &str,
    properties: &HashMap<String, Expr>,
) -> Result<NodeId> {
    let matched = find_merge_match(conn, label, properties)?;
    match matched {
        Some(n) => Ok(n.id),
        None => {
            let mut props = Properties::new();
            let dummy_rec = Record::new();
            for (key, expr) in properties {
                let val = eval_expr(expr, &dummy_rec, conn)?;
                props.insert(key.clone(), val);
            }
            let labels: Vec<String> = if label.is_empty() {
                vec![]
            } else {
                vec![label.to_string()]
            };
            let id = node::create_node(conn, &labels, props.clone())?;
            index::update_indexes_for_node(conn, id, label, None, &props)?;
            Ok(id)
        }
    }
}

fn exec_match_merge(
    conn: &Connection,
    input: &LogicalOp,
    merge_pattern: &crate::cypher::ast::Pattern,
    on_create: &[crate::cypher::ast::Assignment],
    on_match: &[crate::cypher::ast::Assignment],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let mut result = Vec::with_capacity(records.len());

    // Extract the merge pattern structure: (src_node)-[:TYPE]->(dst_node) or single node.
    let elements = &merge_pattern.elements;

    if elements.len() == 1 {
        // Single node MERGE — delegate to existing logic per record.
        for rec in &records {
            let node_pat = match &elements[0] {
                PatternElement::Node(n) => n,
                _ => unreachable!(),
            };
            let label = node_pat.labels.first().map(|s| s.as_str()).unwrap_or("");
            let mut props = Properties::new();
            for (key, expr) in &node_pat.properties {
                let val = eval_expr(expr, rec, conn)?;
                props.insert(key.clone(), val);
            }
            let matched = find_merge_match(conn, label, &node_pat.properties)?;
            let alias = node_pat.variable.as_deref().unwrap_or("_merge");
            let mut out_rec = rec.clone();

            if let Some(n) = matched {
                for assignment in on_match {
                    let mut a_rec = Record::new();
                    a_rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
                    let val = eval_expr(&assignment.value, &a_rec, conn)?;
                    node::set_node_property(conn, n.id, &assignment.property, val)?;
                }
                out_rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
                out_rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
            } else {
                let labels: Vec<String> = if label.is_empty() {
                    vec![]
                } else {
                    vec![label.to_string()]
                };
                let id = node::create_node(conn, &labels, props.clone())?;
                index::update_indexes_for_node(conn, id, label, None, &props)?;
                for assignment in on_create {
                    let mut a_rec = Record::new();
                    a_rec.set(alias.to_string(), Value::I64(id.0 as i64));
                    let val = eval_expr(&assignment.value, &a_rec, conn)?;
                    node::set_node_property(conn, id, &assignment.property, val)?;
                }
                out_rec.set(alias.to_string(), Value::I64(id.0 as i64));
                out_rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
            }

            result.push(out_rec);
        }
        return Ok(result);
    }

    // Relationship merge: (src)-[:TYPE {props}]->(dst)
    if elements.len() != 3 {
        return Err(GraphError::semantic(
            "MERGE pattern must be a single node or (node)-[rel]->(node)",
        ));
    }

    let src_node = match &elements[0] {
        PatternElement::Node(n) => n,
        _ => return Err(GraphError::semantic("expected node pattern")),
    };
    let rel = match &elements[1] {
        PatternElement::Relationship(r) => r,
        _ => return Err(GraphError::semantic("expected relationship pattern")),
    };
    let dst_node = match &elements[2] {
        PatternElement::Node(n) => n,
        _ => return Err(GraphError::semantic("expected node pattern")),
    };

    let src_var = src_node
        .variable
        .as_deref()
        .ok_or_else(|| GraphError::semantic("MERGE relationship source must have a variable"))?;
    let dst_var = dst_node
        .variable
        .as_deref()
        .ok_or_else(|| GraphError::semantic("MERGE relationship target must have a variable"))?;
    let edge_type = rel.rel_types.first().cloned().unwrap_or_default();

    for rec in &records {
        let src_id = match rec.get(src_var) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };
        let dst_id = match rec.get(dst_var) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };

        let out_rec = rec.clone();

        if !edge::edge_exists(conn, src_id, dst_id, &edge_type)? {
            let mut props = Properties::new();
            for (key, expr) in &rel.properties {
                let val = eval_expr(expr, rec, conn)?;
                props.insert(key.clone(), val);
            }
            edge::create_edge(conn, src_id, dst_id, &edge_type, props)?;
            for assignment in on_create {
                let val = eval_expr(&assignment.value, rec, conn)?;
                edge::set_edge_property(
                    conn,
                    src_id,
                    dst_id,
                    &edge_type,
                    &assignment.property,
                    val,
                )?;
            }
        } else {
            for assignment in on_match {
                let val = eval_expr(&assignment.value, rec, conn)?;
                edge::set_edge_property(
                    conn,
                    src_id,
                    dst_id,
                    &edge_type,
                    &assignment.property,
                    val,
                )?;
            }
        }

        result.push(out_rec);
    }

    Ok(result)
}

fn exec_unwind(
    conn: &Connection,
    input: &LogicalOp,
    expr: &Expr,
    alias: &str,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &records {
        let val = eval_expr(expr, rec, conn)?;
        match val {
            Value::List(items) => {
                for item in items {
                    let mut new_rec = rec.clone();
                    new_rec.set(alias.to_string(), item);
                    results.push(new_rec);
                }
            }
            Value::Null => {
                // UNWIND null produces no rows (like UNWIND []).
            }
            _ => {
                return Err(GraphError::Serialization(format!(
                    "UNWIND requires a list, got: {val}"
                )));
            }
        }
    }

    Ok(results)
}

/// Build a Path value from node and edge bindings in each record.
fn exec_materialize_path(
    conn: &Connection,
    input: &LogicalOp,
    path_alias: &str,
    node_aliases: &[String],
    rel_aliases: &[String],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in records {
        let mut nodes = Vec::new();
        for alias in node_aliases {
            if let Some(Value::I64(id)) = rec.get(&format!("{alias}.__id")) {
                match node::get_node(conn, NodeId(*id as u64)) {
                    Ok(n) => nodes.push(n),
                    Err(_) => break,
                }
            }
        }
        let edges = Vec::new(); // TODO: populate from rel_aliases for multi-hop paths
        let _ = rel_aliases; // suppress warning

        let mut new_rec = rec;
        if !nodes.is_empty() {
            new_rec.set(
                path_alias.to_string(),
                Value::Path(PathValue { nodes, edges }),
            );
        }
        results.push(new_rec);
    }

    Ok(results)
}

/// Correlated inner join: for each left record, execute the right side with
/// correlated bindings. Only emit combined rows; drop left rows with no match.
fn exec_correlated_join(
    conn: &Connection,
    input: &LogicalOp,
    right: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let left_records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for l_rec in &left_records {
        let right_records = exec_correlated(conn, right, l_rec, ctx)?;
        // Inner join: only emit if right produced results.
        for r_rec in &right_records {
            let mut combined = l_rec.clone();
            for (key, val) in &r_rec.fields {
                if !combined.fields.contains_key(key) {
                    combined.set(key.clone(), val.clone());
                }
            }
            results.push(combined);
        }
        // If right_records is empty, left row is dropped (inner join semantics).
    }

    Ok(results)
}

fn exec_left_outer_join(
    conn: &Connection,
    input: &LogicalOp,
    right: &LogicalOp,
    optional_aliases: &[String],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let left_records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for l_rec in &left_records {
        // Execute the right side with correlated bindings from this left record.
        let right_records = exec_correlated(conn, right, l_rec, ctx)?;

        if right_records.is_empty() {
            // No match — emit left record with NULLs for optional aliases.
            let mut rec = l_rec.clone();
            for alias in optional_aliases {
                rec.set(alias.clone(), Value::Null);
            }
            results.push(rec);
        } else {
            // Merge each right record into the left record.
            for r_rec in &right_records {
                let mut combined = l_rec.clone();
                for (key, val) in &r_rec.fields {
                    if !combined.fields.contains_key(key) {
                        combined.set(key.clone(), val.clone());
                    }
                }
                results.push(combined);
            }
        }
    }

    Ok(results)
}

/// Execute a plan with correlated bindings from an outer record.
///
/// When a `Scan` alias is already bound in the outer record, returns just
/// that single node instead of scanning all nodes with that label. This
/// turns O(N*M) uncorrelated joins into O(N) correlated lookups.
fn exec_correlated(
    conn: &Connection,
    plan: &LogicalOp,
    outer: &Record,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    match plan {
        LogicalOp::Scan { label, alias } => {
            // If the alias is already bound in the outer record, return just that node.
            if let Some(Value::I64(id)) = outer.get(alias) {
                let node = node::get_node(conn, NodeId(*id as u64))?;
                // Verify label matches if the scan has a label filter.
                if !label.is_empty() && !node.labels.contains(label) {
                    return Ok(vec![]);
                }
                Ok(vec![node_to_record(&node, alias)])
            } else {
                exec_scan(conn, label, alias, ctx)
            }
        }

        LogicalOp::IndexLookup {
            label,
            alias,
            property,
            value,
            remaining_filters,
        } => {
            if let Some(Value::I64(id)) = outer.get(alias) {
                let node = node::get_node(conn, NodeId(*id as u64))?;
                if !label.is_empty() && !node.labels.contains(label) {
                    return Ok(vec![]);
                }
                let rec = node_to_record(&node, alias);
                // Check the index property matches.
                let expected = literal_to_value(value);
                let actual_key = format!("{alias}.{property}");
                if rec.get(&actual_key) != Some(&expected) {
                    return Ok(vec![]);
                }
                if let Some(filter) = remaining_filters {
                    if !eval_predicate(filter, &rec, conn)? {
                        return Ok(vec![]);
                    }
                }
                Ok(vec![rec])
            } else {
                exec_index_lookup(
                    conn,
                    label,
                    alias,
                    property,
                    value,
                    remaining_filters.as_ref(),
                )
            }
        }

        LogicalOp::Expand {
            input,
            src_alias,
            dst_alias,
            rel_alias,
            edge_types,
            direction,
            min_hops,
            max_hops,
        } => {
            let input_records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();

            for rec in &input_records {
                let src_id = match rec.get(src_alias) {
                    Some(Value::I64(id)) => NodeId(*id as u64),
                    _ => continue,
                };

                // If no types specified, discover all edge types for this node.
                let _owned_labels: Vec<String>;
                let labels: Vec<&str> = if edge_types.is_empty() {
                    let all = edge::get_all_edge_labels(conn, src_id, *direction)?;
                    _owned_labels = all.into_iter().map(|(l, _)| l).collect();
                    _owned_labels.iter().map(|s| s.as_str()).collect()
                } else {
                    edge_types.iter().map(|s| s.as_str()).collect()
                };

                // If the destination alias is already bound in the outer record
                // (e.g. OPTIONAL MATCH (caller)-[:CALLS]->(fn) where fn comes
                // from the required MATCH), filter to only the matching ID.
                let bound_dst = outer.get(dst_alias).and_then(|v| match v {
                    Value::I64(id) => Some(NodeId(*id as u64)),
                    _ => None,
                });

                // If the relationship alias is already bound (forwarded through WITH),
                // constrain the expansion to only that specific edge.
                let bound_rel = rel_alias.as_ref().and_then(|ra| {
                    let src = outer.get(&format!("{ra}.__src")).and_then(|v| match v {
                        Value::I64(id) => Some(NodeId(*id as u64)),
                        _ => None,
                    })?;
                    let dst = outer.get(&format!("{ra}.__dst")).and_then(|v| match v {
                        Value::I64(id) => Some(NodeId(*id as u64)),
                        _ => None,
                    })?;
                    let rtype = outer.get(&format!("{ra}.__type")).and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })?;
                    Some((src, dst, rtype))
                });

                // If the relationship is already bound, skip the scan and use
                // the bound edge directly.
                if let Some((rel_src, rel_dst, ref rel_type)) = bound_rel {
                    // Check that this source node matches the edge's source.
                    let (expected_src, expected_dst) = match direction {
                        Direction::Incoming => (rel_dst, rel_src),
                        _ => (rel_src, rel_dst),
                    };
                    if src_id != expected_src {
                        continue;
                    }
                    let dst_id = expected_dst;
                    let dst_node = node::get_node(conn, dst_id)?;
                    let mut new_rec = rec.clone();
                    new_rec.set(dst_alias.to_string(), Value::I64(dst_id.0 as i64));
                    for (key, val) in &dst_node.properties {
                        new_rec.set(format!("{dst_alias}.{key}"), val.clone());
                    }
                    new_rec.set(
                        format!("{dst_alias}.__label"),
                        Value::String(dst_node.labels.join(":")),
                    );
                    new_rec.set(
                        format!("{dst_alias}.__labels"),
                        Value::List(
                            dst_node
                                .labels
                                .iter()
                                .map(|l| Value::String(l.clone()))
                                .collect(),
                        ),
                    );
                    new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                    if let Some(r_alias) = rel_alias {
                        new_rec.set(format!("{r_alias}.__src"), Value::I64(rel_src.0 as i64));
                        new_rec.set(format!("{r_alias}.__dst"), Value::I64(rel_dst.0 as i64));
                        new_rec.set(format!("{r_alias}.__type"), Value::String(rel_type.clone()));
                    }
                    results.push(new_rec);
                    continue;
                }

                for &label in &labels {
                    let dst_ids = if *min_hops == 1 && *max_hops == 1 {
                        edge::get_neighbors(conn, src_id, label, *direction)?
                    } else {
                        edge::traverse(conn, src_id, label, *direction, *min_hops, *max_hops)?
                    };

                    for dst_id in dst_ids {
                        if let Some(expected) = bound_dst {
                            if dst_id != expected {
                                continue;
                            }
                        }
                        let dst_node = node::get_node(conn, dst_id)?;
                        let mut new_rec = rec.clone();
                        new_rec.set(dst_alias.to_string(), Value::I64(dst_id.0 as i64));
                        for (key, val) in &dst_node.properties {
                            new_rec.set(format!("{dst_alias}.{key}"), val.clone());
                        }
                        new_rec.set(
                            format!("{dst_alias}.__label"),
                            Value::String(dst_node.labels.join(":")),
                        );
                        new_rec.set(
                            format!("{dst_alias}.__labels"),
                            Value::List(
                                dst_node
                                    .labels
                                    .iter()
                                    .map(|l| Value::String(l.clone()))
                                    .collect(),
                            ),
                        );
                        new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                        if let Some(r_alias) = rel_alias {
                            let (edge_src, edge_dst) = match direction {
                                Direction::Incoming => (dst_id, src_id),
                                _ => (src_id, dst_id),
                            };
                            new_rec.set(format!("{r_alias}.__src"), Value::I64(edge_src.0 as i64));
                            new_rec.set(format!("{r_alias}.__dst"), Value::I64(edge_dst.0 as i64));
                            new_rec.set(
                                format!("{r_alias}.__type"),
                                Value::String(label.to_string()),
                            );
                            if let Ok(props) =
                                edge::get_edge_properties(conn, edge_src, edge_dst, label)
                            {
                                for (key, val) in &props {
                                    new_rec.set(format!("{r_alias}.{key}"), val.clone());
                                }
                            }
                        }
                        results.push(new_rec);
                    }
                }
            }

            check_row_limit(&results, ctx)?;
            Ok(results)
        }

        LogicalOp::Filter { input, predicate } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for rec in records {
                if eval_predicate(predicate, &rec, conn)? {
                    results.push(rec);
                }
            }
            Ok(results)
        }

        LogicalOp::CrossProduct { left, right } => {
            let left_records = exec_correlated(conn, left, outer, ctx)?;
            let mut results = Vec::new();
            for l in &left_records {
                let right_records = exec_correlated(conn, right, outer, ctx)?;
                for r in &right_records {
                    let mut combined = l.clone();
                    for (key, val) in &r.fields {
                        combined.set(key.clone(), val.clone());
                    }
                    results.push(combined);
                }
            }
            Ok(results)
        }

        LogicalOp::CorrelatedJoin { input, right } => {
            let left_records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for l in &left_records {
                let right_records = exec_correlated(conn, right, l, ctx)?;
                for r in &right_records {
                    let mut combined = l.clone();
                    for (key, val) in &r.fields {
                        if !combined.fields.contains_key(key) {
                            combined.set(key.clone(), val.clone());
                        }
                    }
                    results.push(combined);
                }
            }
            Ok(results)
        }

        // Fallback: execute normally (no correlation pushdown).
        _ => exec(conn, plan, ctx),
    }
}

/// Materialize a `PathValue` by resolving each node and each hop's edge.
///
/// `label` is the edge type the path was traversed on. Edge direction for each
/// hop is resolved by checking both orientations — required for
/// `Direction::Both` traversals where any given hop may run forward or backward.
fn build_path_value(
    conn: &Connection,
    node_ids: Vec<NodeId>,
    label: &str,
) -> Result<crate::types::PathValue> {
    use crate::types::{Edge, PathValue};

    if node_ids.is_empty() {
        return Ok(PathValue {
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }

    let mut nodes = Vec::with_capacity(node_ids.len());
    for id in &node_ids {
        nodes.push(crate::node::get_node(conn, *id)?);
    }

    let mut edges = Vec::with_capacity(node_ids.len().saturating_sub(1));
    for pair in node_ids.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        // Try forward first; fall back to reverse for Direction::Both traversals.
        let (src, dst) = if edge::edge_exists(conn, a, b, label)? {
            (a, b)
        } else {
            (b, a)
        };
        let properties = edge::get_edge_properties(conn, src, dst, label)?;
        edges.push(Edge {
            src,
            dst,
            label: label.to_string(),
            properties,
        });
    }

    Ok(PathValue { nodes, edges })
}

#[allow(clippy::too_many_arguments)]
fn exec_shortest_path(
    conn: &Connection,
    input: &LogicalOp,
    src_alias: &str,
    dst_alias: &str,
    path_alias: &str,
    edge_type: Option<&str>,
    direction: Direction,
    max_hops: u32,
    all_paths: bool,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let label = edge_type.unwrap_or("");
    let mut results = Vec::new();

    for rec in &records {
        let src_id = match rec.get(src_alias) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };
        let dst_id = match rec.get(dst_alias) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };

        if all_paths {
            let paths = edge::all_shortest_paths(conn, src_id, dst_id, label, direction, max_hops)?;
            if paths.is_empty() {
                // No path found — emit record with null path.
                let mut new_rec = rec.clone();
                new_rec.set(path_alias.to_string(), Value::Null);
                results.push(new_rec);
            } else {
                for path in paths {
                    let path_value = build_path_value(conn, path, label)?;
                    let mut new_rec = rec.clone();
                    new_rec.set(path_alias.to_string(), Value::Path(path_value));
                    results.push(new_rec);
                }
            }
        } else {
            let path = edge::shortest_path(conn, src_id, dst_id, label, direction, max_hops)?;
            let mut new_rec = rec.clone();
            match path {
                Some(p) => {
                    let path_value = build_path_value(conn, p, label)?;
                    new_rec.set(path_alias.to_string(), Value::Path(path_value));
                }
                None => new_rec.set(path_alias.to_string(), Value::Null),
            }
            results.push(new_rec);
        }
    }

    Ok(results)
}

/// Find a node matching a MERGE pattern, using index lookup when available.
fn find_merge_match(
    conn: &Connection,
    label: &str,
    properties: &HashMap<String, Expr>,
) -> Result<Option<crate::types::Node>> {
    // Try to find an indexed property with a literal value.
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<&str> = indexes.iter().map(|(_, p)| p.as_str()).collect();

    for (key, expr) in properties {
        if let Expr::Literal(lit) = expr {
            if indexed_props.contains(&key.as_str()) {
                let value = literal_to_value(lit);
                let ids = index::index_lookup(conn, label, key, &value)?;
                // Filter candidates by remaining properties.
                for id in ids {
                    let n = node::get_node(conn, id)?;
                    let all_match = properties.iter().all(|(k, e)| {
                        let expected = match e {
                            Expr::Literal(l) => literal_to_value(l),
                            _ => return false,
                        };
                        n.properties.get(k) == Some(&expected)
                    });
                    if all_match {
                        return Ok(Some(n));
                    }
                }
                return Ok(None);
            }
        }
    }

    // No index available — fall back to label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing.into_iter().find(|n| {
        properties.iter().all(|(key, expr)| {
            let expected = match expr {
                Expr::Literal(lit) => literal_to_value(lit),
                _ => return false,
            };
            n.properties.get(key) == Some(&expected)
        })
    }))
}

pub(crate) fn literal_to_value(lit: &LiteralValue) -> Value {
    match lit {
        LiteralValue::Null => Value::Null,
        LiteralValue::Bool(b) => Value::Bool(*b),
        LiteralValue::I64(n) => Value::I64(*n),
        LiteralValue::F64(n) => Value::F64(*n),
        LiteralValue::String(s) => Value::String(s.clone()),
    }
}

fn value_less_than(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::I64(a), Value::I64(b)) => a < b,
        (Value::F64(a), Value::F64(b)) => a < b,
        (Value::I64(a), Value::F64(b)) => (*a as f64) < *b,
        (Value::F64(a), Value::I64(b)) => *a < (*b as f64),
        (Value::String(a), Value::String(b)) => a < b,
        (Value::List(_), Value::List(_)) => false, // lists are not orderable
        _ => false,
    }
}

/// Cypher type ordering rank for cross-type comparisons.
/// Order: Map < Node < Relationship < Path < List < String < Bool < Number < Null
fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Map(_) => 0,
        Value::Node(_) => 1,
        Value::Edge(_) => 2,
        Value::Path(_) => 3,
        Value::List(_) => 4,
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

fn compare_values_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::I64(a), Value::I64(b)) => a.cmp(b),
        (Value::F64(a), Value::F64(b)) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
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

/// Check whether any record produced by `plan` matches the given correlated
/// bindings.  Short-circuits on the first hit instead of materializing the
/// entire result set, which is the key optimisation for EXISTS subqueries.
///
/// For leaf/pipeline operators we iterate one record at a time. For operators
/// we don't special-case we fall back to `execute()` + linear scan.
pub fn execute_first_match(
    conn: &Connection,
    plan: &LogicalOp,
    correlated_bindings: &[(String, Value)],
) -> Result<bool> {
    match plan {
        LogicalOp::Scan { label, alias } => {
            let nodes = node::find_nodes_by_label(conn, label)?;
            for n in nodes {
                let rec = node_to_record(&n, alias);
                if record_matches_bindings(&rec, correlated_bindings) {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        LogicalOp::IndexLookup {
            label,
            alias,
            property,
            value,
            remaining_filters,
        } => {
            let lookup_value = literal_to_value(value);
            let node_ids = index::index_lookup(conn, label, property, &lookup_value)?;
            for id in node_ids {
                let n = node::get_node(conn, id)?;
                let rec = node_to_record(&n, alias);
                if let Some(filter) = remaining_filters {
                    if !eval_predicate(filter, &rec, conn)? {
                        continue;
                    }
                }
                if record_matches_bindings(&rec, correlated_bindings) {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        LogicalOp::Filter { input, predicate } => {
            // For filter over a scannable input, iterate the input one record
            // at a time, applying predicate + bindings check.
            let records = exec(conn, input, &ExecContext::default())?;
            for rec in records {
                if eval_predicate(predicate, &rec, conn)?
                    && record_matches_bindings(&rec, correlated_bindings)
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        LogicalOp::Expand {
            input,
            src_alias,
            dst_alias,
            edge_types,
            direction,
            min_hops,
            max_hops,
            ..
        } => {
            let input_records = exec(conn, input, &ExecContext::default())?;
            let label = edge_types.first().map(|s| s.as_str()).unwrap_or("");
            for rec in &input_records {
                let src_id = match rec.get(src_alias) {
                    Some(Value::I64(id)) => NodeId(*id as u64),
                    _ => continue,
                };
                let dst_ids = if *min_hops == 1 && *max_hops == 1 {
                    edge::get_neighbors(conn, src_id, label, *direction)?
                } else {
                    edge::traverse(conn, src_id, label, *direction, *min_hops, *max_hops)?
                };
                for dst_id in dst_ids {
                    let dst_node = node::get_node(conn, dst_id)?;
                    let mut new_rec = rec.clone();
                    new_rec.set(dst_alias.to_string(), Value::I64(dst_id.0 as i64));
                    for (key, val) in &dst_node.properties {
                        new_rec.set(format!("{dst_alias}.{key}"), val.clone());
                    }
                    new_rec.set(
                        format!("{dst_alias}.__label"),
                        Value::String(dst_node.labels.join(":")),
                    );
                    new_rec.set(
                        format!("{dst_alias}.__labels"),
                        Value::List(
                            dst_node
                                .labels
                                .iter()
                                .map(|l| Value::String(l.clone()))
                                .collect(),
                        ),
                    );
                    new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                    if record_matches_bindings(&new_rec, correlated_bindings) {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        }

        // Fallback: materialize and scan.
        _ => {
            let results = execute(conn, plan)?;
            for rec in &results {
                if record_matches_bindings(rec, correlated_bindings) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// Build a `Record` from a `Node`, keyed under the given alias.
///
/// Note: NodeId (u64) is transmitted as i64. This wraps for IDs above
/// i64::MAX (~9.2e18), which is practically unreachable — sequential IDs
/// would take thousands of years at millions of inserts per second.
pub(crate) fn node_to_record(n: &crate::types::Node, alias: &str) -> Record {
    debug_assert!(n.id.0 <= i64::MAX as u64, "NodeId exceeds i64::MAX");
    let mut rec = Record::new();
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
    rec
}

/// Check whether a record satisfies correlated bindings from an outer scope.
///
/// A binding matches if the record either doesn't contain the key (no
/// constraint) or contains it with an equal value.
fn record_matches_bindings(rec: &Record, bindings: &[(String, Value)]) -> bool {
    bindings.iter().all(|(key, outer_val)| match rec.get(key) {
        Some(inner_val) => inner_val == outer_val,
        None => true,
    })
}

fn agg_fn_name(f: AggregateFunction) -> &'static str {
    match f {
        AggregateFunction::Count => "count",
        AggregateFunction::Sum => "sum",
        AggregateFunction::Avg => "avg",
        AggregateFunction::Min => "min",
        AggregateFunction::Max => "max",
        AggregateFunction::Collect => "collect",
    }
}

/// Compute the column name for an aggregate expression, matching what
/// `expr_to_column_name` produces for the original `FunctionCall` expression.
fn agg_col_name(agg: &AggregateExpr) -> String {
    agg.alias.clone().unwrap_or_else(|| {
        let expr = crate::cypher::ast::Expr::FunctionCall {
            name: agg_fn_name(agg.function).to_string(),
            args: vec![agg.input.clone()],
        };
        expr_to_column_name(&expr)
    })
}
