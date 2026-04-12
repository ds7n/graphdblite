use std::collections::HashMap;

use rusqlite::Connection;

use crate::cypher::ast::{Expr, LiteralValue, PatternElement};
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::ir::*;
use crate::cypher::record::Record;
use crate::edge;
use crate::node;
use crate::types::{Direction, GraphError, NodeId, Properties, Result, Value};

/// Execute a logical plan against the database, producing result records.
pub fn execute(conn: &Connection, plan: &LogicalOp) -> Result<Vec<Record>> {
    match plan {
        LogicalOp::EmptyRow => Ok(vec![Record::new()]),

        LogicalOp::Scan { label, alias } => exec_scan(conn, label, alias),

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
            *direction, *min_hops, *max_hops,
        ),

        LogicalOp::CrossProduct { left, right } => exec_cross_product(conn, left, right),

        LogicalOp::Filter { input, predicate } => exec_filter(conn, input, predicate),

        LogicalOp::Project { input, items } => exec_project(conn, input, items),

        LogicalOp::Aggregate {
            input,
            group_keys,
            aggregates,
        } => exec_aggregate(conn, input, group_keys, aggregates),

        LogicalOp::Sort { input, items } => exec_sort(conn, input, items),

        LogicalOp::Limit { input, count } => exec_limit(conn, input, *count),

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
            exec_match_create(conn, input, create_ops)
        }

        LogicalOp::Delete { input, variables } => exec_delete(conn, input, variables),

        LogicalOp::SetProperty { input, assignments } => {
            exec_set_property(conn, input, assignments)
        }

        LogicalOp::Merge {
            pattern,
            on_create,
            on_match,
        } => exec_merge(conn, pattern, on_create, on_match),

        LogicalOp::LeftOuterJoin {
            input,
            right,
            optional_aliases,
        } => exec_left_outer_join(conn, input, right, optional_aliases),
    }
}

fn exec_scan(conn: &Connection, label: &str, alias: &str) -> Result<Vec<Record>> {
    let nodes = node::find_nodes_by_label(conn, label)?;
    let mut records = Vec::new();
    for n in nodes {
        let mut rec = Record::new();
        rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
        // Flatten properties as "alias.prop" keys.
        for (key, val) in &n.properties {
            rec.set(format!("{alias}.{key}"), val.clone());
        }
        // Store label for potential use.
        rec.set(format!("{alias}.__label"), Value::String(n.label.clone()));
        rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
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
) -> Result<Vec<Record>> {
    let input_records = execute(conn, input)?;
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

    Ok(results)
}

fn exec_cross_product(
    conn: &Connection,
    left: &LogicalOp,
    right: &LogicalOp,
) -> Result<Vec<Record>> {
    let left_records = execute(conn, left)?;
    let right_records = execute(conn, right)?;
    let mut results = Vec::new();
    for l in &left_records {
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

fn exec_filter(
    conn: &Connection,
    input: &LogicalOp,
    predicate: &Expr,
) -> Result<Vec<Record>> {
    let records = execute(conn, input)?;
    let mut results = Vec::new();
    for rec in records {
        if eval_predicate(predicate, &rec)? {
            results.push(rec);
        }
    }
    Ok(results)
}

fn exec_project(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::ReturnItem],
) -> Result<Vec<Record>> {
    let records = execute(conn, input)?;
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
                            eval_expr(&item.expr, rec)?
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
                        eval_expr(&item.expr, rec)?
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
fn is_user_visible_field(key: &str) -> bool {
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
) -> Result<Vec<Record>> {
    let records = execute(conn, input)?;

    if group_keys.is_empty() {
        // No grouping — aggregate over all records.
        let mut rec = Record::new();
        for agg in aggregates {
            let col_name = agg
                .alias
                .clone()
                .unwrap_or_else(|| format!("{}(*)", agg_fn_name(agg.function)));
            let val = compute_aggregate(agg, &records)?;
            rec.set(col_name, val);
        }
        return Ok(vec![rec]);
    }

    // Group by keys.
    let mut groups: Vec<(Vec<Value>, Vec<Record>)> = Vec::new();

    for rec in &records {
        let key_vals: Vec<Value> = group_keys
            .iter()
            .map(|k| eval_expr(k, rec).unwrap_or(Value::Null))
            .collect();

        if let Some(group) = groups.iter_mut().find(|(k, _)| k == &key_vals) {
            group.1.push(rec.clone());
        } else {
            groups.push((key_vals, vec![rec.clone()]));
        }
    }

    let mut results = Vec::new();
    for (key_vals, group_records) in &groups {
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
            let val = compute_aggregate(agg, group_records)?;
            rec.set(col_name, val);
        }
        results.push(rec);
    }

    Ok(results)
}

fn compute_aggregate(agg: &AggregateExpr, records: &[Record]) -> Result<Value> {
    match agg.function {
        AggregateFunction::Count => {
            if matches!(agg.input, Expr::Star) {
                Ok(Value::I64(records.len() as i64))
            } else {
                let count = records
                    .iter()
                    .filter(|r| !matches!(eval_expr(&agg.input, r), Ok(Value::Null)))
                    .count();
                Ok(Value::I64(count as i64))
            }
        }
        AggregateFunction::Sum => {
            let mut sum = 0.0f64;
            for rec in records {
                match eval_expr(&agg.input, rec)? {
                    Value::I64(n) => sum += n as f64,
                    Value::F64(n) => sum += n,
                    _ => {}
                }
            }
            Ok(Value::F64(sum))
        }
        AggregateFunction::Avg => {
            let mut sum = 0.0f64;
            let mut count = 0;
            for rec in records {
                match eval_expr(&agg.input, rec)? {
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
                let val = eval_expr(&agg.input, rec)?;
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
                let val = eval_expr(&agg.input, rec)?;
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
                let val = eval_expr(&agg.input, rec)?;
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
) -> Result<Vec<Record>> {
    let mut records = execute(conn, input)?;
    records.sort_by(|a, b| {
        for item in items {
            let va = eval_expr(&item.expr, a).unwrap_or(Value::Null);
            let vb = eval_expr(&item.expr, b).unwrap_or(Value::Null);
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

fn exec_limit(conn: &Connection, input: &LogicalOp, count: u64) -> Result<Vec<Record>> {
    let records = execute(conn, input)?;
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
        let val = eval_expr(expr, &dummy_rec)?;
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
                    let val = eval_expr(expr, &dummy_rec)?;
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
                    let val = eval_expr(expr, &dummy_rec)?;
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
) -> Result<Vec<Record>> {
    let records = execute(conn, input)?;

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
                    let dummy_rec = Record::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, &dummy_rec)?;
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
                    let dummy_rec = Record::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, &dummy_rec)?;
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
) -> Result<Vec<Record>> {
    let records = execute(conn, input)?;
    for rec in &records {
        for var in variables {
            if let Some(Value::I64(id)) = rec.get(var) {
                node::delete_node(conn, NodeId(*id as u64))?;
            }
        }
    }
    Ok(vec![])
}

fn exec_set_property(
    conn: &Connection,
    input: &LogicalOp,
    assignments: &[crate::cypher::ast::Assignment],
) -> Result<Vec<Record>> {
    let records = execute(conn, input)?;
    for rec in &records {
        for assignment in assignments {
            if let Some(Value::I64(id)) = rec.get(&assignment.variable) {
                let val = eval_expr(&assignment.value, rec)?;
                node::set_node_property(
                    conn,
                    NodeId(*id as u64),
                    &assignment.property,
                    val,
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
    // MERGE only supports single node patterns in v0.1.
    let node_pat = match pattern.elements.first() {
        Some(PatternElement::Node(n)) => n,
        _ => {
            return Err(GraphError::Transaction(
                "MERGE only supports single node patterns in v0.1".to_string(),
            ))
        }
    };

    let label = node_pat.label.as_deref().unwrap_or("");
    let alias = node_pat.variable.as_deref().unwrap_or("_merge");

    // Try to find an existing node matching all inline properties.
    let existing = node::find_nodes_by_label(conn, label)?;
    let matched = existing.into_iter().find(|n| {
        node_pat.properties.iter().all(|(key, expr)| {
            let expected = match expr {
                Expr::Literal(lit) => literal_to_value(lit),
                _ => return false,
            };
            n.properties.get(key) == Some(&expected)
        })
    });

    match matched {
        Some(n) => {
            // ON MATCH SET
            for assignment in on_match {
                let mut rec = Record::new();
                rec.set(assignment.variable.clone(), Value::I64(n.id.0 as i64));
                let val = eval_expr(&assignment.value, &rec)?;
                node::set_node_property(conn, n.id, &assignment.property, val)?;
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
                let val = eval_expr(expr, &dummy_rec)?;
                props.insert(key.clone(), val);
            }
            let id = node::create_node(conn, label, props)?;

            // ON CREATE SET
            for assignment in on_create {
                let mut rec = Record::new();
                rec.set(assignment.variable.clone(), Value::I64(id.0 as i64));
                let val = eval_expr(&assignment.value, &rec)?;
                node::set_node_property(conn, id, &assignment.property, val)?;
            }

            let mut rec = Record::new();
            rec.set(alias.to_string(), Value::I64(id.0 as i64));
            Ok(vec![rec])
        }
    }
}

fn exec_left_outer_join(
    conn: &Connection,
    input: &LogicalOp,
    right: &LogicalOp,
    optional_aliases: &[String],
) -> Result<Vec<Record>> {
    let left_records = execute(conn, input)?;
    let right_records = execute(conn, right)?;

    // Find shared aliases: keys present in both left and right records (bare alias keys, no dots).
    // These are the join keys.
    let shared_aliases: Vec<String> = if let (Some(l), Some(r)) =
        (left_records.first(), right_records.first())
    {
        l.fields
            .keys()
            .filter(|k| !k.contains('.') && r.fields.contains_key(*k))
            .cloned()
            .collect()
    } else {
        vec![]
    };

    let mut results = Vec::new();

    for l_rec in &left_records {
        // Find right records that match on all shared aliases.
        let matching: Vec<&Record> = right_records
            .iter()
            .filter(|r| {
                shared_aliases.iter().all(|alias| {
                    l_rec.get(alias) == r.get(alias)
                })
            })
            .collect();

        if matching.is_empty() {
            // No match — emit left record with NULLs for optional aliases and their properties.
            let mut rec = l_rec.clone();
            for alias in optional_aliases {
                rec.set(alias.clone(), Value::Null);
            }
            results.push(rec);
        } else {
            // Matches found — merge each right record into the left record.
            for r_rec in matching {
                let mut combined = l_rec.clone();
                for (key, val) in &r_rec.fields {
                    // Only copy keys from the right that aren't already in the left
                    // (avoid overwriting shared alias bindings).
                    if !combined.fields.contains_key(key) || shared_aliases.iter().all(|a| a != key) {
                        combined.set(key.clone(), val.clone());
                    }
                }
                results.push(combined);
            }
        }
    }

    Ok(results)
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
