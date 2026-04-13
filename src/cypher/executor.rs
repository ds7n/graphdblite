use std::collections::HashMap;

use rusqlite::Connection;

use crate::cypher::ast::{Expr, LiteralValue, PatternElement};
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::ir::*;
use crate::cypher::record::Record;
use crate::edge;
use crate::index;
use crate::node;
use crate::types::{Direction, GraphError, NodeId, Properties, Result, Value};

/// Execution context carrying runtime limits.
#[derive(Default)]
pub struct ExecContext {
    /// Maximum rows any operator may produce. 0 = unlimited.
    pub max_result_rows: usize,
}

/// Check that a result set hasn't exceeded the row cap.
fn check_row_limit(results: &[Record], ctx: &ExecContext) -> Result<()> {
    if ctx.max_result_rows > 0 && results.len() > ctx.max_result_rows {
        return Err(GraphError::Transaction(format!(
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
        exec(conn, plan, &ExecContext::default())
    }
}

/// Execute with an explicit context carrying runtime limits.
pub fn execute_with_ctx(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    exec(conn, plan, ctx)
}

/// Check whether a plan tree contains only read-only operators.
fn is_read_only(plan: &LogicalOp) -> bool {
    match plan {
        LogicalOp::Scan { .. }
        | LogicalOp::IndexLookup { .. }
        | LogicalOp::EmptyRow => true,

        LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::Unwind { input, .. }
        | LogicalOp::Aggregate { input, .. }
        | LogicalOp::ShortestPath { input, .. } => is_read_only(input),

        LogicalOp::Expand { input, .. } => is_read_only(input),

        LogicalOp::CrossProduct { left, right }
        | LogicalOp::LeftOuterJoin { input: left, right, .. } => {
            is_read_only(left) && is_read_only(right)
        }

        // Write operations.
        LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::CreateSequence { .. }
        | LogicalOp::MatchCreate { .. }
        | LogicalOp::Delete { .. }
        | LogicalOp::SetProperty { .. }
        | LogicalOp::Merge { .. } => false,
    }
}

fn exec(conn: &Connection, plan: &LogicalOp, ctx: &ExecContext) -> Result<Vec<Record>> {
    match plan {
        LogicalOp::EmptyRow => Ok(vec![Record::new()]),

        LogicalOp::Scan { label, alias } => exec_scan(conn, label, alias, ctx),

        LogicalOp::IndexLookup {
            label,
            alias,
            property,
            value,
            remaining_filters,
        } => exec_index_lookup(conn, label, alias, property, value, remaining_filters.as_ref()),

        LogicalOp::Expand {
            input,
            src_alias,
            dst_alias,
            edge_type,
            direction,
            min_hops,
            max_hops,
        } => exec_expand(
            conn, input, src_alias, dst_alias, edge_type.as_deref(),
            *direction, *min_hops, *max_hops, ctx,
        ),

        LogicalOp::CrossProduct { left, right } => exec_cross_product(conn, left, right, ctx),

        LogicalOp::Filter { input, predicate } => exec_filter(conn, input, predicate, ctx),

        LogicalOp::Project { input, items } => exec_project(conn, input, items, ctx),

        LogicalOp::Aggregate {
            input,
            group_keys,
            aggregates,
        } => exec_aggregate(conn, input, group_keys, aggregates, ctx),

        LogicalOp::Sort { input, items } => exec_sort(conn, input, items, ctx),

        LogicalOp::Limit { input, count } => exec_limit(conn, input, *count, ctx),

        LogicalOp::CreateNode {
            label,
            alias,
            properties,
        } => exec_create_node(conn, label.as_deref(), alias.as_deref(), properties),

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

        LogicalOp::Delete { input, variables, detach } => exec_delete(conn, input, variables, *detach, ctx),

        LogicalOp::SetProperty { input, assignments } => {
            exec_set_property(conn, input, assignments, ctx)
        }

        LogicalOp::Merge {
            pattern,
            on_create,
            on_match,
        } => exec_merge(conn, pattern, on_create, on_match),

        LogicalOp::Unwind {
            input,
            expr,
            alias,
        } => exec_unwind(conn, input, expr, alias, ctx),

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
            conn, input, src_alias, dst_alias, path_alias,
            edge_type.as_deref(), *direction, *max_hops, *all_paths, ctx,
        ),
    }
}

fn exec_scan(conn: &Connection, label: &str, alias: &str, ctx: &ExecContext) -> Result<Vec<Record>> {
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

fn exec_expand(
    conn: &Connection,
    input: &LogicalOp,
    src_alias: &str,
    dst_alias: &str,
    edge_type: Option<&str>,
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

        let label = edge_type.unwrap_or("");

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
                    Value::String(dst_node.label.clone()),
                );
                new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                results.push(new_rec);
            }
        } else {
            // Variable-length traversal.
            let reachable =
                edge::traverse(conn, src_id, label, direction, min_hops, max_hops)?;
            for dst_id in reachable {
                let dst_node = node::get_node(conn, dst_id)?;
                let mut new_rec = rec.clone();
                new_rec.set(dst_alias.to_string(), Value::I64(dst_id.0 as i64));
                for (key, val) in &dst_node.properties {
                    new_rec.set(format!("{dst_alias}.{key}"), val.clone());
                }
                new_rec.set(
                    format!("{dst_alias}.__label"),
                    Value::String(dst_node.label.clone()),
                );
                new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                results.push(new_rec);
            }
        }
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
                return Err(GraphError::Transaction(format!(
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

fn exec_project(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::ReturnItem],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &records {
        let mut projected = Record::new();
        for item in items {
            match &item.expr {
                Expr::Star => {
                    // RETURN * — copy all user-visible fields (skip internal __ and bare aliases).
                    for (key, val) in &rec.fields {
                        if is_user_visible_field(key) {
                            projected.set(key.clone(), val.clone());
                        }
                    }
                }
                Expr::Variable(var) => {
                    // RETURN n — expand to all n.prop fields (skip __ props).
                    let prefix = format!("{var}.");
                    let mut found_props = false;
                    for (key, val) in &rec.fields {
                        if let Some(prop) = key.strip_prefix(&prefix) {
                            if !prop.starts_with("__") {
                                if let Some(alias) = &item.alias {
                                    projected.set(format!("{alias}.{prop}"), val.clone());
                                } else {
                                    projected.set(key.clone(), val.clone());
                                }
                                found_props = true;
                            }
                        }
                    }
                    if !found_props {
                        // Fall back to raw value (e.g., aggregate result already in record).
                        let col_name = item.alias.clone().unwrap_or_else(|| var.clone());
                        let val = if let Some(existing) = rec.get(&col_name) {
                            existing.clone()
                        } else {
                            eval_expr(&item.expr, rec, conn)?
                        };
                        projected.set(col_name, val);
                    }
                }
                _ => {
                    let col_name = item
                        .alias
                        .clone()
                        .unwrap_or_else(|| expr_to_column_name(&item.expr));
                    // Check if the aggregate result is already in the record (from Aggregate operator).
                    let val = if let Some(existing) = rec.get(&col_name) {
                        existing.clone()
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
            let col_name = agg
                .alias
                .clone()
                .unwrap_or_else(|| format!("{}(*)", agg_fn_name(agg.function)));
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
            rec.set(col_name, key_vals[i].clone());
        }
        for agg in aggregates {
            let col_name = agg
                .alias
                .clone()
                .unwrap_or_else(|| format!("{}(*)", agg_fn_name(agg.function)));
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
            if all_integer { Ok(Value::I64(i64_sum)) } else { Ok(Value::F64(f64_sum)) }
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

fn exec_limit(conn: &Connection, input: &LogicalOp, count: u64, ctx: &ExecContext) -> Result<Vec<Record>> {
    let records = exec(conn, input, ctx)?;
    Ok(records.into_iter().take(count as usize).collect())
}

fn exec_create_node(
    conn: &Connection,
    label: Option<&str>,
    alias: Option<&str>,
    properties: &HashMap<String, Expr>,
) -> Result<Vec<Record>> {
    let mut props = Properties::new();
    let dummy_rec = Record::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, &dummy_rec, conn)?;
        props.insert(key.clone(), val);
    }

    let id = node::create_node(conn, label.unwrap_or(""), props)?;

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
                label,
                alias,
                properties,
            } => {
                let mut props = Properties::new();
                let dummy_rec = Record::new();
                for (key, expr) in properties {
                    let val = eval_expr(expr, &dummy_rec, conn)?;
                    props.insert(key.clone(), val);
                }
                let id = node::create_node(conn, label.as_deref().unwrap_or(""), props)?;
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
                    GraphError::Transaction(format!("unbound variable: {src_alias}"))
                })?;
                let dst = bindings.get(dst_alias).ok_or_else(|| {
                    GraphError::Transaction(format!("unbound variable: {dst_alias}"))
                })?;
                let mut props = Properties::new();
                let dummy_rec = Record::new();
                for (key, expr) in properties {
                    let val = eval_expr(expr, &dummy_rec, conn)?;
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

        // Execute each CREATE op using the bindings.
        for op in create_ops {
            match op {
                LogicalOp::CreateNode {
                    label,
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
                        props.insert(key.clone(), val);
                    }
                    let id =
                        node::create_node(conn, label.as_deref().unwrap_or(""), props)?;
                    if let Some(alias) = alias {
                        bindings.insert(alias.clone(), id);
                    }
                }
                LogicalOp::CreateEdge {
                    src_alias,
                    dst_alias,
                    edge_type,
                    properties,
                } => {
                    let src = bindings.get(src_alias).ok_or_else(|| {
                        GraphError::Transaction(format!("unbound variable: {src_alias}"))
                    })?;
                    let dst = bindings.get(dst_alias).ok_or_else(|| {
                        GraphError::Transaction(format!("unbound variable: {dst_alias}"))
                    })?;
                    let mut props = Properties::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, rec, conn)?;
                        props.insert(key.clone(), val);
                    }
                    edge::create_edge(conn, *src, *dst, edge_type, props)?;
                }
                _ => {}
            }
        }
    }

    Ok(vec![])
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
            if let Some(Value::I64(id)) = rec.get(var) {
                let node_id = NodeId(*id as u64);
                if !detach && node::node_has_edges(conn, node_id)? {
                    return Err(GraphError::HasEdges(node_id));
                }
                node::delete_node(conn, node_id)?;
            }
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
            if let Some(Value::I64(id)) = rec.get(&assignment.variable) {
                let node_id = NodeId(*id as u64);
                let old = node::get_node(conn, node_id)?;
                let val = eval_expr(&assignment.value, rec, conn)?;
                node::set_node_property(conn, node_id, &assignment.property, val.clone())?;
                let mut new_props = old.properties.clone();
                new_props.insert(assignment.property.clone(), val);
                index::update_indexes_for_node(
                    conn, node_id, &old.label, Some(&old.properties), &new_props,
                )?;
            }
        }
    }
    Ok(vec![])
}

fn exec_merge(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[crate::cypher::ast::Assignment],
    on_match: &[crate::cypher::ast::Assignment],
) -> Result<Vec<Record>> {
    // Safety: MERGE pattern is validated at plan time to be a single node.
    let node_pat = match pattern.elements.first() {
        Some(PatternElement::Node(n)) => n,
        _ => unreachable!("MERGE pattern validated at plan time"),
    };

    let label = node_pat.label.as_deref().unwrap_or("");
    let alias = node_pat.variable.as_deref().unwrap_or("_merge");

    // Try to find an existing node — use index lookup if one exists for a
    // literal property in the MERGE pattern, otherwise fall back to label scan.
    let matched = find_merge_match(conn, label, &node_pat.properties)?;

    match matched {
        Some(n) => {
            // ON MATCH SET — update indexes alongside properties.
            for assignment in on_match {
                let old = node::get_node(conn, n.id)?;
                let mut rec = Record::new();
                rec.set(assignment.variable.clone(), Value::I64(n.id.0 as i64));
                let val = eval_expr(&assignment.value, &rec, conn)?;
                node::set_node_property(conn, n.id, &assignment.property, val.clone())?;
                let mut new_props = old.properties.clone();
                new_props.insert(assignment.property.clone(), val);
                index::update_indexes_for_node(
                    conn, n.id, &old.label, Some(&old.properties), &new_props,
                )?;
            }
            let mut rec = Record::new();
            rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
            Ok(vec![rec])
        }
        None => {
            // Create the node.
            let mut props = Properties::new();
            let dummy_rec = Record::new();
            for (key, expr) in &node_pat.properties {
                let val = eval_expr(expr, &dummy_rec, conn)?;
                props.insert(key.clone(), val);
            }
            let id = node::create_node(conn, label, props.clone())?;
            // Backfill indexes for the newly created node.
            index::update_indexes_for_node(conn, id, label, None, &props)?;

            // ON CREATE SET — update indexes alongside properties.
            for assignment in on_create {
                let old = node::get_node(conn, id)?;
                let mut rec = Record::new();
                rec.set(assignment.variable.clone(), Value::I64(id.0 as i64));
                let val = eval_expr(&assignment.value, &rec, conn)?;
                node::set_node_property(conn, id, &assignment.property, val.clone())?;
                let mut new_props = old.properties.clone();
                new_props.insert(assignment.property.clone(), val);
                index::update_indexes_for_node(
                    conn, id, &old.label, Some(&old.properties), &new_props,
                )?;
            }

            let mut rec = Record::new();
            rec.set(alias.to_string(), Value::I64(id.0 as i64));
            Ok(vec![rec])
        }
    }
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
                if !label.is_empty() && node.label != *label {
                    return Ok(vec![]);
                }
                Ok(vec![node_to_record(&node, alias)])
            } else {
                exec_scan(conn, label, alias, ctx)
            }
        }

        LogicalOp::IndexLookup { label, alias, property, value, remaining_filters } => {
            if let Some(Value::I64(id)) = outer.get(alias) {
                let node = node::get_node(conn, NodeId(*id as u64))?;
                if !label.is_empty() && node.label != *label {
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
                exec_index_lookup(conn, label, alias, property, value, remaining_filters.as_ref())
            }
        }

        LogicalOp::Expand {
            input, src_alias, dst_alias, edge_type, direction, min_hops, max_hops,
        } => {
            let input_records = exec_correlated(conn, input, outer, ctx)?;
            let label = edge_type.as_deref().unwrap_or("");
            let mut results = Vec::new();

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
                        Value::String(dst_node.label.clone()),
                    );
                    new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                    results.push(new_rec);
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

        // Fallback: execute normally (no correlation pushdown).
        _ => exec(conn, plan, ctx),
    }
}

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
                    let mut new_rec = rec.clone();
                    new_rec.set(path_alias.to_string(), Value::Path(path));
                    results.push(new_rec);
                }
            }
        } else {
            let path = edge::shortest_path(conn, src_id, dst_id, label, direction, max_hops)?;
            let mut new_rec = rec.clone();
            match path {
                Some(p) => new_rec.set(path_alias.to_string(), Value::Path(p)),
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

fn compare_values_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::I64(a), Value::I64(b)) => a.cmp(b),
        (Value::F64(a), Value::F64(b)) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        (Value::I64(a), Value::F64(b)) => (*a as f64)
            .partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::F64(a), Value::I64(b)) => a
            .partial_cmp(&(*b as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater, // nulls last
        (_, Value::Null) => std::cmp::Ordering::Less,
        _ => std::cmp::Ordering::Equal,
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
            edge_type,
            direction,
            min_hops,
            max_hops,
        } => {
            let input_records = exec(conn, input, &ExecContext::default())?;
            let label = edge_type.as_deref().unwrap_or("");
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
                        Value::String(dst_node.label.clone()),
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
pub(crate) fn node_to_record(n: &crate::types::Node, alias: &str) -> Record {
    let mut rec = Record::new();
    rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
    for (key, val) in &n.properties {
        rec.set(format!("{alias}.{key}"), val.clone());
    }
    rec.set(format!("{alias}.__label"), Value::String(n.label.clone()));
    rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
    rec
}

/// Check whether a record satisfies correlated bindings from an outer scope.
///
/// A binding matches if the record either doesn't contain the key (no
/// constraint) or contains it with an equal value.
fn record_matches_bindings(rec: &Record, bindings: &[(String, Value)]) -> bool {
    bindings.iter().all(|(key, outer_val)| {
        match rec.get(key) {
            Some(inner_val) => inner_val == outer_val,
            None => true,
        }
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
