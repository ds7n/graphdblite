//! Correlated execution: subqueries, EXISTS, multi-MATCH joins, OPTIONAL MATCH.
//!
//! Extracted from `executor::mod` to keep the dispatcher file manageable.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::cypher::ast::{Expr, ExprKind};
use crate::cypher::eval::{eval_predicate, expr_to_column_name};
use crate::cypher::ir::*;
use crate::cypher::record::NamedRecord;
use crate::edge;
use crate::node;
use crate::types::{Direction, NodeId, Result, Value};

use super::read::exec_fulltext_lookup;
use super::{
    build_compound_binding, check_row_limit, collect_flat_edge_ids, compound_binding_vars, exec,
    exec_aggregate_over_records, exec_id_lookup, exec_index_lookup, exec_scan, fetch_and_populate,
    has_duplicate_relationships, literal_to_value, node_to_record, ExecContext,
};

/// Correlated inner join: for each left record, execute the right side with
/// correlated bindings. Only emit combined rows; drop left rows with no match.
pub(super) fn exec_correlated_join(
    conn: &Connection,
    input: &LogicalOp,
    right: &LogicalOp,
    same_match: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
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
            // Cross-pattern relationship uniqueness (only within the same
            // MATCH clause — separate MATCHes have independent scopes).
            if same_match && has_duplicate_relationships(&combined) {
                continue;
            }
            results.push(combined);
        }
        // If right_records is empty, left row is dropped (inner join semantics).
    }

    Ok(results)
}

pub(super) fn exec_left_outer_join(
    conn: &Connection,
    input: &LogicalOp,
    right: &LogicalOp,
    optional_aliases: &[String],
    opt_filter: Option<&Expr>,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let left_records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for l_rec in &left_records {
        // Execute the right side with correlated bindings from this left record.
        let right_records = exec_correlated(conn, right, l_rec, ctx)?;

        if right_records.is_empty() {
            // No match — emit left record with NULLs for optional aliases
            // and their flattened metadata keys so downstream operators
            // (property access, labels(), type(), etc.) see Null properly.
            let mut rec = l_rec.clone();
            for alias in optional_aliases {
                rec.set(alias.clone(), Value::Null);
                for suffix in &["__id", "__labels", "__src", "__dst", "__type"] {
                    rec.set(format!("{alias}.{suffix}"), Value::Null);
                }
            }
            results.push(rec);
        } else {
            // Merge each right record into the left record.
            let mut any_passed = false;
            for r_rec in &right_records {
                let mut combined = l_rec.clone();
                for (key, val) in &r_rec.fields {
                    if !combined.fields.contains_key(key) {
                        combined.set(key.clone(), val.clone());
                    }
                }
                // If there's a WHERE clause on the OPTIONAL MATCH, check it.
                // Rows that fail the predicate are discarded; if ALL rows fail,
                // the left record is null-filled.
                if let Some(filter) = opt_filter {
                    if !eval_predicate(filter, &combined, crate::cypher::eval::EvalCx::new(conn))? {
                        continue;
                    }
                }
                any_passed = true;
                results.push(combined);
            }
            // If no right row passed the filter, null-fill.
            if !any_passed {
                let mut rec = l_rec.clone();
                for alias in optional_aliases {
                    rec.set(alias.clone(), Value::Null);
                    for suffix in &["__id", "__labels", "__src", "__dst", "__type"] {
                        rec.set(format!("{alias}.{suffix}"), Value::Null);
                    }
                }
                results.push(rec);
            }
        }
    }

    Ok(results)
}

/// Execute a plan with correlated bindings from an outer record.
///
/// Extract a node ID from a record value — handles both `Value::I64` (flat
/// binding from MATCH) and `Value::Node` (compound binding from WITH/RETURN).
pub(super) fn value_to_node_id(val: &Value) -> Option<NodeId> {
    match val {
        Value::I64(id) => Some(NodeId(*id as u64)),
        Value::Node(n) => Some(n.id),
        _ => None,
    }
}

/// When a `Scan` alias is already bound in the outer record, returns just
/// that single node instead of scanning all nodes with that label. This
/// turns O(N*M) uncorrelated joins into O(N) correlated lookups.
pub(super) fn exec_correlated(
    conn: &Connection,
    plan: &LogicalOp,
    outer: &NamedRecord,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    match plan {
        LogicalOp::Scan { label, alias } => {
            // If the alias is already bound in the outer record, return just that node.
            if let Some(node_id) = outer.get(alias).and_then(value_to_node_id) {
                let node = node::get_node(conn, node_id)?;
                // Verify label matches if the scan has a label filter.
                if !label.is_empty() && !node.labels.contains(label) {
                    return Ok(vec![]);
                }
                Ok(vec![node_to_record(&node, alias)])
            } else if outer.get(alias) == Some(&Value::Null) {
                // Variable is bound to null (e.g. from OPTIONAL MATCH) — no match.
                Ok(vec![])
            } else {
                exec_scan(conn, label, alias, ctx)
            }
        }

        LogicalOp::IdLookup { alias, value_expr } => {
            // If the alias is already bound in the outer record, that
            // takes precedence — match the existing Scan/IndexLookup
            // handling for re-bound variables.
            if let Some(node_id) = outer.get(alias).and_then(value_to_node_id) {
                let node = node::get_node(conn, node_id)?;
                return Ok(vec![node_to_record(&node, alias)]);
            }
            if outer.get(alias) == Some(&Value::Null) {
                return Ok(vec![]);
            }
            exec_id_lookup(conn, alias, value_expr, outer)
        }

        LogicalOp::FullTextLookup {
            label,
            alias,
            property,
            op,
            term,
            remaining_filters,
        } => {
            // Pass `outer` so the term expression can resolve variables from
            // the enclosing scope (e.g. an UNWIND'd value).
            exec_fulltext_lookup(
                conn,
                label,
                alias,
                property,
                *op,
                term,
                remaining_filters.as_ref(),
                outer,
            )
        }

        LogicalOp::IndexLookup {
            label,
            alias,
            index_properties,
            lookups,
            remaining_filters,
        } => {
            if let Some(node_id) = outer.get(alias).and_then(value_to_node_id) {
                let node = node::get_node(conn, node_id)?;
                if !label.is_empty() && !node.labels.contains(label) {
                    return Ok(vec![]);
                }
                let rec = node_to_record(&node, alias);
                // Verify every lookup property matches the pre-bound node.
                for (property, value) in lookups {
                    let expected = crate::cypher::executor::resolve_lookup_key(value)?;
                    let actual_key = format!("{alias}.{property}");
                    if rec.get(&actual_key) != Some(&expected) {
                        return Ok(vec![]);
                    }
                }
                if let Some(filter) = remaining_filters {
                    if !eval_predicate(filter, &rec, crate::cypher::eval::EvalCx::new(conn))? {
                        return Ok(vec![]);
                    }
                }
                Ok(vec![rec])
            } else {
                exec_index_lookup(
                    conn,
                    label,
                    alias,
                    index_properties,
                    lookups,
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
            var_length,
            var_length_prop_filters,
            result_cap: _,
        } => {
            let input_records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();

            for rec in &input_records {
                let src_id = match rec.get(src_alias).and_then(value_to_node_id) {
                    Some(id) => id,
                    _ => continue,
                };

                // If no types specified, discover all edge types for this node.
                // For var-length, pass empty labels so traverse_paths discovers
                // types at each hop (different nodes may have different edge types).
                let _owned_labels: Vec<String>;
                let labels: Vec<&str> = if edge_types.is_empty() {
                    let all = edge::get_all_edge_labels(conn, src_id, *direction)?;
                    _owned_labels = all.into_iter().map(|(l, _)| l).collect();
                    _owned_labels.iter().map(|s| s.as_str()).collect()
                } else {
                    edge_types.iter().map(|s| s.as_str()).collect()
                };
                let var_length_labels: Vec<&str> = if edge_types.is_empty() {
                    vec![] // signal traverse_paths to discover per-hop
                } else {
                    labels.clone()
                };

                // If the destination alias is already bound — either in the outer
                // record (from the left side of a CorrelatedJoin) or in the
                // current record (from an earlier expand in this chain) —
                // filter to only the matching ID.
                let bound_dst = outer
                    .get(dst_alias)
                    .and_then(value_to_node_id)
                    .or_else(|| rec.get(dst_alias).and_then(value_to_node_id));

                if *var_length {
                    // Check if the rel alias is already bound to a list of
                    // edges (e.g. `WITH [r1, r2] AS rs ... MATCH ()-[rs*]->()`).
                    // If so, use those specific edges as the path.
                    let bound_edge_list = rel_alias.as_ref().and_then(|ra| {
                        let val = outer.get(ra).or_else(|| rec.get(ra))?;
                        if let Value::List(items) = val {
                            let edges: Vec<&crate::types::Edge> = items
                                .iter()
                                .filter_map(|v| match v {
                                    Value::Edge(e) => Some(e),
                                    _ => None,
                                })
                                .collect();
                            if edges.len() == items.len() && !edges.is_empty() {
                                Some(edges)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    });

                    if let Some(bound_edges) = bound_edge_list {
                        // Walk the pre-bound edge chain and verify it forms a
                        // valid path starting from src_id.
                        let mut current = src_id;
                        let mut valid = true;
                        for edge in &bound_edges {
                            let next = match direction {
                                Direction::Outgoing => {
                                    if edge.src == current {
                                        Some(edge.dst)
                                    } else {
                                        None
                                    }
                                }
                                Direction::Incoming => {
                                    if edge.dst == current {
                                        Some(edge.src)
                                    } else {
                                        None
                                    }
                                }
                                Direction::Both => {
                                    if edge.src == current {
                                        Some(edge.dst)
                                    } else if edge.dst == current {
                                        Some(edge.src)
                                    } else {
                                        None
                                    }
                                }
                            };
                            match next {
                                Some(n) => current = n,
                                None => {
                                    valid = false;
                                    break;
                                }
                            }
                        }
                        if valid {
                            let dst_id = current;
                            if bound_dst.is_none() || bound_dst == Some(dst_id) {
                                let mut new_rec = rec.clone();
                                fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
                                if let Some(r_alias) = rel_alias {
                                    new_rec.set(
                                        r_alias.to_string(),
                                        Value::List(
                                            bound_edges
                                                .iter()
                                                .map(|e| Value::Edge((*e).clone()))
                                                .collect(),
                                        ),
                                    );
                                }
                                results.push(new_rec);
                            }
                        }
                    } else {
                        // Variable-length traversal — pass ALL labels at once.
                        let prop_filter_values: HashMap<String, Value> = var_length_prop_filters
                            .iter()
                            .filter_map(|(k, expr)| match &expr.kind {
                                ExprKind::Literal(lit) => Some((k.clone(), literal_to_value(lit))),
                                _ => None,
                            })
                            .collect();
                        let paths = edge::traverse_paths(
                            conn,
                            src_id,
                            &var_length_labels,
                            *direction,
                            *min_hops,
                            *max_hops,
                            &prop_filter_values,
                            None,
                            ctx.max_traversal_work,
                        )?;
                        // Collect edges already bound by prior Expand steps in
                        // this pattern chain so we can enforce cross-segment
                        // relationship uniqueness (e.g. the fixed-length edge
                        // in `()-[:R]->()<-[:R*3]->()` must not be reused by
                        // the var-length traversal). Compare on (src, dst, type)
                        // only — exec_correlated may not track edge_seq for
                        // fixed-length segments, and for cross-segment checks
                        // any edge between the same endpoints counts as the same.
                        let prior_edges: Vec<(i64, i64, String)> = collect_flat_edge_ids(rec)
                            .into_iter()
                            .map(|(s, d, t, _seq)| (s, d, t))
                            .collect();
                        for (dst_id, steps) in paths {
                            if let Some(expected) = bound_dst {
                                if dst_id != expected {
                                    continue;
                                }
                            }
                            // Skip paths that reuse an edge from a prior
                            // segment in the same pattern.
                            if !prior_edges.is_empty() {
                                let has_overlap = steps.iter().any(|step| {
                                    let s = step.edge_src.0 as i64;
                                    let d = step.edge_dst.0 as i64;
                                    let key = (s.min(d), s.max(d), step.edge_label.clone());
                                    prior_edges.contains(&key)
                                });
                                if has_overlap {
                                    continue;
                                }
                            }
                            let mut new_rec = rec.clone();
                            fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
                            if let Some(r_alias) = rel_alias {
                                let edge_list: Vec<Value> = steps
                                    .iter()
                                    .map(|step| {
                                        let props = edge::get_edge_properties_at(
                                            conn,
                                            step.edge_src,
                                            step.edge_dst,
                                            &step.edge_label,
                                            step.edge_seq,
                                        )
                                        .unwrap_or_default();
                                        Value::Edge(crate::types::Edge {
                                            src: step.edge_src,
                                            dst: step.edge_dst,
                                            label: step.edge_label.clone(),
                                            properties: props,
                                        })
                                    })
                                    .collect();
                                new_rec.set(r_alias.to_string(), Value::List(edge_list));
                            }
                            results.push(new_rec);
                        }
                    }
                } else {
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
                        let seq = outer
                            .get(&format!("{ra}.__edge_seq"))
                            .and_then(|v| match v {
                                Value::I64(s) => Some(*s as u64),
                                _ => None,
                            })
                            .unwrap_or(0);
                        Some((src, dst, rtype, seq))
                    });

                    // If the relationship is already bound, skip the scan and use
                    // the bound edge directly.
                    if let Some((rel_src, rel_dst, ref rel_type, rel_seq)) = bound_rel {
                        // Check that the bound relationship type matches the pattern constraint.
                        if !edge_types.is_empty() && !edge_types.iter().any(|t| t == rel_type) {
                            continue;
                        }
                        // Check that this source node is an endpoint of the bound edge.
                        let (expected_src, expected_dst) = match direction {
                            Direction::Incoming => (rel_dst, rel_src),
                            Direction::Outgoing => (rel_src, rel_dst),
                            Direction::Both => {
                                if src_id == rel_src {
                                    (rel_src, rel_dst)
                                } else if src_id == rel_dst {
                                    (rel_dst, rel_src)
                                } else {
                                    continue;
                                }
                            }
                        };
                        if src_id != expected_src {
                            continue;
                        }
                        let dst_id = expected_dst;
                        let mut new_rec = rec.clone();
                        fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
                        if let Some(r_alias) = rel_alias {
                            new_rec.set(r_alias.to_string(), Value::String(rel_type.clone()));
                            new_rec.set(format!("{r_alias}.__src"), Value::I64(rel_src.0 as i64));
                            new_rec.set(format!("{r_alias}.__dst"), Value::I64(rel_dst.0 as i64));
                            new_rec
                                .set(format!("{r_alias}.__type"), Value::String(rel_type.clone()));
                            new_rec
                                .set(format!("{r_alias}.__edge_seq"), Value::I64(rel_seq as i64));
                        }
                        results.push(new_rec);
                        continue;
                    }

                    for &label in &labels {
                        let dst_ids = if *min_hops == 1 && *max_hops == 1 {
                            edge::get_neighbors(conn, src_id, label, *direction)?
                        } else {
                            edge::traverse(
                                conn, src_id, label, *direction, *min_hops, *max_hops, None,
                            )?
                        };

                        for dst_id in dst_ids {
                            if let Some(expected) = bound_dst {
                                if dst_id != expected {
                                    continue;
                                }
                            }
                            let mut new_rec = rec.clone();
                            fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
                            if let Some(r_alias) = rel_alias {
                                let (edge_src, edge_dst) = match direction {
                                    Direction::Incoming => (dst_id, src_id),
                                    Direction::Outgoing => (src_id, dst_id),
                                    Direction::Both => {
                                        if edge::edge_exists(conn, src_id, dst_id, label)
                                            .unwrap_or(false)
                                        {
                                            (src_id, dst_id)
                                        } else {
                                            (dst_id, src_id)
                                        }
                                    }
                                };
                                // Relationship uniqueness: check against other
                                // relationship bindings in the record.
                                let (es, ed) = (edge_src.0 as i64, edge_dst.0 as i64);
                                let edge_key = (es.min(ed), es.max(ed), label, 0u64);
                                let mut duplicate = false;
                                for (key, _val) in &new_rec.fields {
                                    if key.ends_with(".__src") && key != &format!("{r_alias}.__src")
                                    {
                                        let other_alias = &key[..key.len() - 6];
                                        let other_seq = match new_rec
                                            .get(&format!("{other_alias}.__edge_seq"))
                                        {
                                            Some(Value::I64(s)) => *s as u64,
                                            _ => 0,
                                        };
                                        if let (
                                            Some(Value::I64(os)),
                                            Some(Value::I64(od)),
                                            Some(Value::String(ot)),
                                        ) = (
                                            new_rec.get(key),
                                            new_rec.get(&format!("{other_alias}.__dst")),
                                            new_rec.get(&format!("{other_alias}.__type")),
                                        ) {
                                            let other_key = (
                                                (*os).min(*od),
                                                (*os).max(*od),
                                                ot.as_str(),
                                                other_seq,
                                            );
                                            if other_key == edge_key {
                                                duplicate = true;
                                                break;
                                            }
                                        }
                                    }
                                }
                                if duplicate {
                                    continue;
                                }
                                new_rec.set(r_alias.to_string(), Value::String(label.to_string()));
                                new_rec
                                    .set(format!("{r_alias}.__src"), Value::I64(edge_src.0 as i64));
                                new_rec
                                    .set(format!("{r_alias}.__dst"), Value::I64(edge_dst.0 as i64));
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
            }

            check_row_limit(&results, ctx)?;
            Ok(results)
        }

        LogicalOp::Filter { input, predicate } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for rec in records {
                // Merge outer bindings into the record so nested EXISTS
                // predicates can see variables from enclosing scopes.
                let mut merged = rec.clone();
                for (k, v) in &outer.fields {
                    if !merged.fields.contains_key(k) {
                        merged.set(k.clone(), v.clone());
                    }
                }
                if eval_predicate(predicate, &merged, crate::cypher::eval::EvalCx::new(conn))? {
                    results.push(rec);
                }
            }
            Ok(results)
        }

        LogicalOp::CrossProduct {
            left,
            right,
            same_match,
        } => {
            let left_records = exec_correlated(conn, left, outer, ctx)?;
            let mut results = Vec::new();
            for l in &left_records {
                // Merge left record with outer so the right side sees both.
                let mut merged = outer.clone();
                for (key, val) in &l.fields {
                    merged.set(key.clone(), val.clone());
                }
                let right_records = exec_correlated(conn, right, &merged, ctx)?;
                for r in &right_records {
                    let mut combined = l.clone();
                    for (key, val) in &r.fields {
                        combined.set(key.clone(), val.clone());
                    }
                    if *same_match && has_duplicate_relationships(&combined) {
                        continue;
                    }
                    results.push(combined);
                }
            }
            Ok(results)
        }

        LogicalOp::CorrelatedJoin {
            input,
            right,
            same_match,
        } => {
            let left_records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for l in &left_records {
                // Merge outer into left so the right side sees both scopes.
                let mut merged = outer.clone();
                for (key, val) in &l.fields {
                    merged.set(key.clone(), val.clone());
                }
                let right_records = exec_correlated(conn, right, &merged, ctx)?;
                for r in &right_records {
                    let mut combined = l.clone();
                    for (key, val) in &r.fields {
                        if !combined.fields.contains_key(key) {
                            combined.set(key.clone(), val.clone());
                        }
                    }
                    if *same_match && has_duplicate_relationships(&combined) {
                        continue;
                    }
                    results.push(combined);
                }
            }
            Ok(results)
        }

        // Project: push correlation into the input, then apply projection
        // normally via the non-correlated exec path for the Project node alone.
        LogicalOp::Project {
            input,
            items,
            emit_compound,
        } => {
            let input_records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for rec in &input_records {
                let mut projected = NamedRecord::new();
                for item in items {
                    match &item.expr.kind {
                        ExprKind::Star => {
                            for (key, val) in &rec.fields {
                                projected.set(key.clone(), val.clone());
                            }
                        }
                        _ => {
                            let col = item
                                .alias
                                .clone()
                                .or_else(|| {
                                    if let ExprKind::Variable(v) = &item.expr.kind {
                                        Some(v.clone())
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_else(|| format!("{:?}", item.expr));
                            // Try alias-based lookup first (aggregate results
                            // are stored under alias by exec_aggregate).
                            let expr_col = expr_to_column_name(&item.expr);
                            let val = if let Some(existing) = rec.get(&expr_col) {
                                existing.clone()
                            } else if let Some(existing) = rec.get(&col) {
                                existing.clone()
                            } else {
                                crate::cypher::eval::eval_expr(
                                    &item.expr,
                                    rec,
                                    crate::cypher::eval::EvalCx::new(conn),
                                )?
                            };
                            projected.set(col, val);
                            // Carry forward internal metadata for bound variables.
                            if let ExprKind::Variable(v) = &item.expr.kind {
                                let alias = item.alias.as_deref().unwrap_or(v.as_str());
                                for (key, val) in &rec.fields {
                                    if key.starts_with(&format!("{v}.")) {
                                        let suffix = &key[v.len()..];
                                        projected.set(format!("{alias}{suffix}"), val.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                if *emit_compound {
                    // For final RETURN, also build compound node/relationship values.
                    let mut compound = NamedRecord::new();
                    let bound_vars = compound_binding_vars(&projected);
                    for var in &bound_vars {
                        if let Some(cv) = build_compound_binding(&projected, var) {
                            compound.set(var.clone(), cv);
                        }
                    }
                    // Also carry non-compound columns.
                    for (key, val) in &projected.fields {
                        if !key.contains('.') && !bound_vars.contains(key) {
                            compound.set(key.clone(), val.clone());
                        }
                    }
                    results.push(compound);
                } else {
                    results.push(projected);
                }
            }
            Ok(results)
        }

        // Aggregate: push correlation into the input, then aggregate.
        LogicalOp::Aggregate {
            input,
            group_keys,
            aggregates,
        } => {
            // Execute the input with correlation, then aggregate the results.
            let records = exec_correlated(conn, input, outer, ctx)?;
            exec_aggregate_over_records(conn, &records, group_keys, aggregates)
        }

        // MaterializePath: push correlation into the input, then materialize paths.
        LogicalOp::MaterializePath {
            input,
            path_alias,
            node_aliases,
            rel_aliases,
        } => {
            let records = exec_correlated(conn, input, outer, ctx)?;
            let mut results = Vec::new();
            for rec in records {
                let mut has_null = false;
                let mut nodes = Vec::new();
                for alias in node_aliases {
                    if rec.get(alias) == Some(&Value::Null) {
                        has_null = true;
                        break;
                    }
                    if let Some(Value::I64(id)) = rec.get(&format!("{alias}.__id")) {
                        match node::get_node(conn, NodeId(*id as u64)) {
                            Ok(n) => nodes.push(n),
                            Err(_) => break,
                        }
                    }
                }
                let mut edges = Vec::new();
                for alias in rel_aliases {
                    if rec.get(alias) == Some(&Value::Null) {
                        has_null = true;
                        break;
                    }
                    if let Some(Value::List(edge_list)) = rec.get(alias) {
                        for item in edge_list {
                            if let Value::Edge(e) = item {
                                edges.push(e.clone());
                            }
                        }
                    } else if let Some(Value::Edge(e)) = build_compound_binding(&rec, alias) {
                        edges.push(e);
                    }
                }

                // Relationship uniqueness within the path: skip records where
                // the same edge appears more than once across segments (e.g.
                // a var-length segment reusing the bound relationship's edge).
                if !has_null && edges.len() > 1 {
                    let mut edge_ids: Vec<(i64, i64, String, u64)> = Vec::new();
                    let mut dup = false;
                    for e in &edges {
                        let s = e.src.0 as i64;
                        let d = e.dst.0 as i64;
                        let key = (s.min(d), s.max(d), e.label.clone(), 0u64);
                        if edge_ids.contains(&key) {
                            dup = true;
                            break;
                        }
                        edge_ids.push(key);
                    }
                    if dup {
                        continue;
                    }
                }

                // Var-length path node reconstruction.
                let has_var_length_rel = rel_aliases
                    .iter()
                    .any(|a| matches!(rec.get(a), Some(Value::List(_))));
                if !has_null && has_var_length_rel {
                    if edges.is_empty() {
                        // Zero-length var-length path: collapse to single start node.
                        nodes.truncate(1);
                    } else if edges.len() + 1 > nodes.len() {
                        // Multi-hop: rebuild full node list from edge endpoints.
                        let mut full_nodes = vec![nodes[0].clone()];
                        for edge in &edges {
                            let next_id = edge.dst;
                            let n = node::get_node(conn, next_id)?;
                            full_nodes.push(n);
                        }
                        if nodes.len() >= 2 {
                            *full_nodes.last_mut().unwrap() = nodes.last().unwrap().clone();
                        }
                        nodes = full_nodes;
                    }
                }

                let mut new_rec = rec;
                if has_null {
                    new_rec.set(path_alias.to_string(), Value::Null);
                } else if !nodes.is_empty() {
                    new_rec.set(
                        path_alias.to_string(),
                        Value::Path(crate::types::PathValue { nodes, edges }),
                    );
                }
                results.push(new_rec);
            }
            Ok(results)
        }

        // Fallback: execute normally (no correlation pushdown).
        _ => exec(conn, plan, ctx),
    }
}
