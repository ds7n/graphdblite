//! MERGE evaluation and find-or-create helpers.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::eval_expr;
use crate::cypher::record::NamedRecord;
use crate::types::*;
use crate::{edge, fts, index, node};

use super::*;

pub(in crate::cypher::executor) fn apply_merge_set_item_node(
    conn: &Connection,
    item: &SetItem,
    node_id: NodeId,
    rec: &NamedRecord,
) -> Result<()> {
    match item {
        SetItem::Property(assignment) => {
            let old = node::get_node(conn, node_id)?;
            let mut a_rec = rec.clone();
            a_rec.set(assignment.variable.clone(), Value::I64(node_id.0 as i64));
            let val = eval_expr(
                &assignment.value,
                &a_rec,
                crate::cypher::eval::EvalCx::new(conn),
            )?;
            node::set_node_property(conn, node_id, &assignment.property, val.clone())?;
            let mut new_props = old.properties.clone();
            new_props.insert(assignment.property.clone(), val);
            index::update_indexes_for_node(
                conn,
                node_id,
                &old.labels,
                Some(&old.properties),
                &new_props,
            )?;
            fts::update_fts_for_node(
                conn,
                node_id,
                &old.labels,
                Some(&old.properties),
                &new_props,
            )?;
        }
        SetItem::Label {
            variable: _,
            labels,
        } => {
            // Adding labels can bring new per-label indexes into scope for this
            // node, so re-insert index/FTS entries across the full label set.
            for label in labels {
                node::add_node_label(conn, node_id, label)?;
            }
            let new = node::get_node(conn, node_id)?;
            index::update_indexes_for_node(conn, node_id, &new.labels, None, &new.properties)?;
            fts::update_fts_for_node(conn, node_id, &new.labels, None, &new.properties)?;
        }
        SetItem::MapMerge { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            let old = node::get_node(conn, node_id)?;
            let mut new_props = old.properties.clone();
            for (k, v) in &map {
                if *v == Value::Null {
                    new_props.remove(k);
                } else {
                    new_props.insert(k.clone(), v.clone());
                }
                node::set_node_property(conn, node_id, k, v.clone())?;
            }
            maintain_indexes_after_prop_change(conn, node_id, &old, &new_props)?;
        }
        SetItem::MapOverwrite { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            // Clear existing properties and set new ones.
            let old = node::get_node(conn, node_id)?;
            for key in old.properties.keys() {
                node::set_node_property(conn, node_id, key, Value::Null)?;
            }
            let mut new_props = Properties::new();
            for (k, v) in &map {
                if *v != Value::Null {
                    new_props.insert(k.clone(), v.clone());
                }
                node::set_node_property(conn, node_id, k, v.clone())?;
            }
            maintain_indexes_after_prop_change(conn, node_id, &old, &new_props)?;
        }
    }
    Ok(())
}

/// Bring secondary + FTS indexes in sync after a MERGE `SET`/`SET +=` map write
/// mutates a node's properties, mirroring the property-change maintenance in
/// `write.rs`. `old` is the node as read *before* the write; `new_props` is the
/// full post-write property set.
fn maintain_indexes_after_prop_change(
    conn: &Connection,
    node_id: NodeId,
    old: &Node,
    new_props: &Properties,
) -> Result<()> {
    index::update_indexes_for_node(conn, node_id, &old.labels, Some(&old.properties), new_props)?;
    fts::update_fts_for_node(conn, node_id, &old.labels, Some(&old.properties), new_props)?;
    Ok(())
}

/// Apply a single SetItem to an edge in a MERGE ON CREATE/ON MATCH context.
pub(in crate::cypher::executor) fn apply_merge_set_item_edge(
    conn: &Connection,
    item: &SetItem,
    src_id: NodeId,
    dst_id: NodeId,
    edge_type: &str,
    rec: &NamedRecord,
) -> Result<()> {
    match item {
        SetItem::Property(assignment) => {
            let val = eval_expr(
                &assignment.value,
                rec,
                crate::cypher::eval::EvalCx::new(conn),
            )?;
            edge::set_edge_property(conn, src_id, dst_id, edge_type, &assignment.property, val)?;
        }
        SetItem::Label { .. } => {
            // Labels on edges are not standard Cypher; ignore.
        }
        SetItem::MapMerge { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            for (k, v) in &map {
                edge::set_edge_property(conn, src_id, dst_id, edge_type, k, v.clone())?;
            }
        }
        SetItem::MapOverwrite { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            // For overwrite on edges, clear existing props then set new ones.
            let old_props = edge::get_edge_properties(conn, src_id, dst_id, edge_type)?;
            for key in old_props.keys() {
                edge::set_edge_property(conn, src_id, dst_id, edge_type, key, Value::Null)?;
            }
            for (k, v) in &map {
                edge::set_edge_property(conn, src_id, dst_id, edge_type, k, v.clone())?;
            }
        }
    }
    Ok(())
}

/// Apply a single SetItem to a specific parallel edge by sequence number.
pub(in crate::cypher::executor) fn apply_merge_set_item_edge_at(
    conn: &Connection,
    item: &SetItem,
    src_id: NodeId,
    dst_id: NodeId,
    edge_type: &str,
    seq: u64,
    rec: &NamedRecord,
) -> Result<()> {
    match item {
        SetItem::Property(assignment) => {
            let val = eval_expr(
                &assignment.value,
                rec,
                crate::cypher::eval::EvalCx::new(conn),
            )?;
            edge::set_edge_property_at(
                conn,
                src_id,
                dst_id,
                edge_type,
                seq,
                &assignment.property,
                val,
            )?;
        }
        SetItem::Label { .. } => {}
        SetItem::MapMerge { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            for (k, v) in &map {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, k, v.clone())?;
            }
        }
        SetItem::MapOverwrite { variable: _, value } => {
            let map = resolve_to_map(value, rec, conn)?;
            let old_props = edge::get_edge_properties_at(conn, src_id, dst_id, edge_type, seq)?;
            for key in old_props.keys() {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, key, Value::Null)?;
            }
            for (k, v) in &map {
                edge::set_edge_property_at(conn, src_id, dst_id, edge_type, seq, k, v.clone())?;
            }
        }
    }
    Ok(())
}

/// Resolve an expression to a property map. If the expression evaluates to a
/// node ID, load that node's properties. If it evaluates to an edge, use the
/// edge's properties. If it's already a map, use it directly.
pub(in crate::cypher::executor) fn resolve_to_map(
    expr: &Expr,
    rec: &NamedRecord,
    conn: &Connection,
) -> Result<Properties> {
    let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
    match val {
        Value::Map(map) => Ok(map.into_iter().collect()),
        Value::Node(n) => Ok(n.properties),
        Value::Edge(e) => Ok(e.properties),
        Value::I64(id) => {
            // Could be a node ID — try to load its properties.
            match node::get_node(conn, NodeId(id as u64)) {
                Ok(n) => Ok(n.properties),
                Err(_) => Ok(Properties::new()),
            }
        }
        Value::Null => Ok(Properties::new()),
        _ => Ok(Properties::new()),
    }
}

pub(in crate::cypher::executor) fn exec_merge(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    if pattern.elements.len() == 1 {
        return exec_merge_node(conn, pattern, on_create, on_match);
    }
    // 3-element relationship MERGE: (a:L {p})-[:TYPE]->(b:L {p})
    exec_merge_relationship(conn, pattern, on_create, on_match)
}

/// Single-node MERGE: find-or-create a node matching the pattern.
pub(in crate::cypher::executor) fn exec_merge_node(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
    let node_pat = match pattern.elements.first() {
        Some(PatternElement::Node(n)) => n,
        _ => unreachable!("MERGE pattern validated at plan time"),
    };

    let all_labels: Vec<&str> = node_pat.labels.iter().map(|s| s.as_str()).collect();
    let alias = node_pat.variable.as_deref().unwrap_or("_merge");

    // Pre-evaluate properties.
    let dummy_rec = NamedRecord::new();
    let mut props = Properties::new();
    for (key, expr) in &node_pat.properties {
        let val = eval_expr(expr, &dummy_rec, crate::cypher::eval::EvalCx::new(conn))?;
        // Null property in MERGE is MergeReadOwnWrites.
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        props.insert(key.clone(), val);
    }

    // Find a node that has ALL required labels and matching properties.
    let matched = find_merge_match_multi_label(conn, &all_labels, &props)?;

    let node_id = match matched {
        Some(n) => {
            let rec = NamedRecord::new();
            for item in on_match {
                apply_merge_set_item_node(conn, item, n.id, &rec)?;
            }
            n.id
        }
        None => {
            let labels: Vec<String> = all_labels.iter().map(|s| s.to_string()).collect();
            let id = node::create_node(conn, &labels, props.clone())?;
            index::update_indexes_for_node(conn, id, &labels, None, &props)?;
            fts::update_fts_for_node(conn, id, &labels, None, &props)?;

            let rec = NamedRecord::new();
            for item in on_create {
                apply_merge_set_item_node(conn, item, id, &rec)?;
            }
            id
        }
    };

    let mut rec = NamedRecord::new();
    rec.set(alias.to_string(), Value::I64(node_id.0 as i64));
    rec.set(format!("{alias}.__id"), Value::I64(node_id.0 as i64));
    // Populate property bindings from the resolved node so slot-path Property
    // reads land on populated slots. Skip `__label`/`__labels`: that pair
    // triggers `build_compound_binding`, shifting Variable resolution from
    // `Value::I64` to `Value::Node` and breaking downstream code paths that
    // expect the flat shape (see exec_create_node note).
    let resolved = node::get_node(conn, node_id)?;
    for (k, v) in &resolved.properties {
        rec.set(format!("{alias}.{k}"), v.clone());
    }

    // Bind path variable if present: MERGE p = (a {props})
    if let Some(ref path_var) = pattern.path_variable {
        rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
    }

    Ok(vec![rec])
}

/// Relationship MERGE: find-or-create nodes and the edge between them.
pub(in crate::cypher::executor) fn exec_merge_relationship(
    conn: &Connection,
    pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
) -> Result<Vec<NamedRecord>> {
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

    // Pre-evaluate edge properties and check for null.
    let mut edge_props = Properties::new();
    let dummy_rec = NamedRecord::new();
    for (key, expr) in &rel.properties {
        let val = eval_expr(expr, &dummy_rec, crate::cypher::eval::EvalCx::new(conn))?;
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        edge_props.insert(key.clone(), val);
    }

    // Find or create the edge (checking properties too).
    let edge_match = if edge::edge_exists(conn, src_id, dst_id, &edge_type)? {
        if edge_props.is_empty() {
            true
        } else {
            let existing_props = edge::get_edge_properties(conn, src_id, dst_id, &edge_type)?;
            edge_props
                .iter()
                .all(|(k, v)| existing_props.get(k) == Some(v))
        }
    } else {
        false
    };
    if !edge_match {
        edge::create_edge(conn, src_id, dst_id, &edge_type, edge_props)?;
        let mut rec = NamedRecord::new();
        if let Some(ref v) = src_pat.variable {
            rec.set(v.clone(), Value::I64(src_id.0 as i64));
        }
        if let Some(ref v) = dst_pat.variable {
            rec.set(v.clone(), Value::I64(dst_id.0 as i64));
        }
        for item in on_create {
            apply_merge_set_item_edge(conn, item, src_id, dst_id, &edge_type, &rec)?;
        }
    } else {
        let mut rec = NamedRecord::new();
        if let Some(ref v) = src_pat.variable {
            rec.set(v.clone(), Value::I64(src_id.0 as i64));
        }
        if let Some(ref v) = dst_pat.variable {
            rec.set(v.clone(), Value::I64(dst_id.0 as i64));
        }
        for item in on_match {
            apply_merge_set_item_edge(conn, item, src_id, dst_id, &edge_type, &rec)?;
        }
    }

    let mut rec = NamedRecord::new();
    if let Some(ref v) = src_pat.variable {
        rec.set(v.clone(), Value::I64(src_id.0 as i64));
    }
    if let Some(ref v) = dst_pat.variable {
        rec.set(v.clone(), Value::I64(dst_id.0 as i64));
    }

    // Bind path variable if present: MERGE p = (a)-[:R]->(b)
    if let Some(ref path_var) = pattern.path_variable {
        let src_node = node::get_node(conn, src_id)?;
        let dst_node = node::get_node(conn, dst_id)?;
        let edge_props = edge::get_edge_properties(conn, src_id, dst_id, &edge_type)?;
        let path = PathValue {
            nodes: vec![src_node, dst_node],
            edges: vec![crate::types::Edge {
                src: src_id,
                dst: dst_id,
                label: edge_type.clone(),
                properties: edge_props,
            }],
        };
        rec.set(path_var.clone(), Value::Path(path));
    }

    Ok(vec![rec])
}

/// Find a node matching the MERGE pattern properties, or create it if not found.
pub(in crate::cypher::executor) fn find_or_create_merge_node(
    conn: &Connection,
    label: &str,
    properties: &HashMap<String, Expr>,
) -> Result<NodeId> {
    let dummy_rec = NamedRecord::new();
    let mut props = Properties::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, &dummy_rec, crate::cypher::eval::EvalCx::new(conn))?;
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        props.insert(key.clone(), val);
    }
    let matched = find_merge_match_evaluated(conn, label, &props)?;
    match matched {
        Some(n) => Ok(n.id),
        None => {
            let labels: Vec<String> = if label.is_empty() {
                vec![]
            } else {
                vec![label.to_string()]
            };
            let id = node::create_node(conn, &labels, props.clone())?;
            index::update_indexes_for_node(conn, id, &labels, None, &props)?;
            fts::update_fts_for_node(conn, id, &labels, None, &props)?;
            Ok(id)
        }
    }
}

pub(in crate::cypher::executor) fn exec_match_merge(
    conn: &Connection,
    input: &LogicalOp,
    merge_pattern: &crate::cypher::ast::Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut result = Vec::with_capacity(records.len());

    // Extract the merge pattern structure: (src_node)-[:TYPE]->(dst_node) or single node.
    let elements = &merge_pattern.elements;

    if elements.len() == 1 {
        // Single node MERGE — match-or-create per input record.
        // MERGE produces one output row per matching node (all matches, not just first).
        for rec in &records {
            let node_pat = match &elements[0] {
                PatternElement::Node(n) => n,
                _ => unreachable!(),
            };
            let label = node_pat.labels.first().map(|s| s.as_str()).unwrap_or("");
            let mut props = Properties::new();
            for (key, expr) in &node_pat.properties {
                let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                // Null property in MERGE is MergeReadOwnWrites.
                if val == Value::Null {
                    return Err(GraphError::semantic("MERGE with null property value")
                        .with_code(ErrorCode::MergeReadOwnWrites));
                }
                props.insert(key.clone(), val);
            }
            let matches = find_merge_matches_evaluated(conn, label, &props)?;
            let alias = node_pat.variable.as_deref().unwrap_or("_merge");

            if matches.is_empty() {
                // No match — create a new node.
                let labels: Vec<String> = if label.is_empty() {
                    vec![]
                } else {
                    vec![label.to_string()]
                };
                let id = node::create_node(conn, &labels, props.clone())?;
                index::update_indexes_for_node(conn, id, &labels, None, &props)?;
                fts::update_fts_for_node(conn, id, &labels, None, &props)?;
                for item in on_create {
                    apply_merge_set_item_node(conn, item, id, rec)?;
                }
                let mut out_rec = rec.clone();
                out_rec.set(alias.to_string(), Value::I64(id.0 as i64));
                out_rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                // Populate prop keys (post-ON CREATE) so slot-path Property
                // reads land on populated slots. Skip __label/__labels —
                // see exec_create_node note re: compound-binding heuristic.
                let resolved = node::get_node(conn, id)?;
                for (k, v) in &resolved.properties {
                    out_rec.set(format!("{alias}.{k}"), v.clone());
                }
                // Bind path variable if present.
                if let Some(ref path_var) = merge_pattern.path_variable {
                    out_rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
                }
                result.push(out_rec);
            } else {
                // One or more matches — produce one row per matching node.
                for n in &matches {
                    for item in on_match {
                        apply_merge_set_item_node(conn, item, n.id, rec)?;
                    }
                    let mut out_rec = rec.clone();
                    out_rec.set(alias.to_string(), Value::I64(n.id.0 as i64));
                    out_rec.set(format!("{alias}.__id"), Value::I64(n.id.0 as i64));
                    // Re-fetch (ON MATCH may have written) and populate prop keys.
                    let resolved = node::get_node(conn, n.id)?;
                    for (k, v) in &resolved.properties {
                        out_rec.set(format!("{alias}.{k}"), v.clone());
                    }
                    // Bind path variable if present.
                    if let Some(ref path_var) = merge_pattern.path_variable {
                        out_rec.set(path_var.clone(), Value::Path(PathValue::single(resolved)));
                    }
                    result.push(out_rec);
                }
            }
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
    let undirected = matches!(rel.direction, crate::cypher::ast::RelDirection::Undirected);

    for rec in &records {
        let src_id = match rec.get(src_var) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };
        let dst_id = match rec.get(dst_var) {
            Some(Value::I64(id)) => NodeId(*id as u64),
            _ => continue,
        };

        let mut out_rec = rec.clone();

        // Pre-evaluate merge pattern edge properties.
        let mut merge_edge_props = Properties::new();
        for (key, expr) in &rel.properties {
            let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
            if val == Value::Null {
                return Err(GraphError::semantic("MERGE with null property value")
                    .with_code(ErrorCode::MergeReadOwnWrites));
            }
            merge_edge_props.insert(key.clone(), val);
        }

        // Find all matching edges (possibly multiple parallel edges).
        // Each matching edge produces one output row.
        let find_matching_edges = |s: NodeId, d: NodeId| -> Result<Vec<(u64, Properties)>> {
            let all = edge::get_all_edge_props(conn, s, d, &edge_type)?;
            if merge_edge_props.is_empty() {
                return Ok(all);
            }
            Ok(all
                .into_iter()
                .filter(|(_, props)| {
                    merge_edge_props
                        .iter()
                        .all(|(k, v)| props.get(k) == Some(v))
                })
                .collect())
        };

        // Collect matches from forward direction, and reverse for undirected.
        let mut matched_edges: Vec<(NodeId, NodeId, u64, Properties)> = Vec::new();
        for (seq, props) in find_matching_edges(src_id, dst_id)? {
            matched_edges.push((src_id, dst_id, seq, props));
        }
        if undirected {
            for (seq, props) in find_matching_edges(dst_id, src_id)? {
                matched_edges.push((dst_id, src_id, seq, props));
            }
        }

        if matched_edges.is_empty() {
            // No match — create new edge.
            edge::create_edge(conn, src_id, dst_id, &edge_type, merge_edge_props)?;
            for item in on_create {
                apply_merge_set_item_edge(conn, item, src_id, dst_id, &edge_type, rec)?;
            }
            // Bind the relationship variable for the newly created edge.
            if let Some(ref r_alias) = rel.variable {
                out_rec.set(r_alias.clone(), Value::String(edge_type.clone()));
                out_rec.set(format!("{r_alias}.__src"), Value::I64(src_id.0 as i64));
                out_rec.set(format!("{r_alias}.__dst"), Value::I64(dst_id.0 as i64));
                out_rec.set(
                    format!("{r_alias}.__type"),
                    Value::String(edge_type.clone()),
                );
                let edge_props = edge::get_edge_properties(conn, src_id, dst_id, &edge_type)?;
                for (key, val) in &edge_props {
                    out_rec.set(format!("{r_alias}.{key}"), val.clone());
                }
            }
            // Bind path variable if present: MERGE p = (a)-[:R]->(b)
            if let Some(ref path_var) = merge_pattern.path_variable {
                let src_node = node::get_node(conn, src_id)?;
                let dst_node = node::get_node(conn, dst_id)?;
                let ep = edge::get_edge_properties(conn, src_id, dst_id, &edge_type)?;
                out_rec.set(
                    path_var.clone(),
                    Value::Path(PathValue {
                        nodes: vec![src_node, dst_node],
                        edges: vec![crate::types::Edge {
                            src: src_id,
                            dst: dst_id,
                            label: edge_type.clone(),
                            properties: ep,
                        }],
                    }),
                );
            }
            result.push(out_rec);
        } else {
            // One or more matches — produce one row per matching edge.
            for (m_src, m_dst, m_seq, _) in &matched_edges {
                for item in on_match {
                    apply_merge_set_item_edge_at(
                        conn, item, *m_src, *m_dst, &edge_type, *m_seq, rec,
                    )?;
                }
            }
            for (m_src, m_dst, m_seq, _) in &matched_edges {
                let mut match_rec = rec.clone();
                if let Some(ref r_alias) = rel.variable {
                    match_rec.set(r_alias.clone(), Value::String(edge_type.clone()));
                    match_rec.set(format!("{r_alias}.__src"), Value::I64(m_src.0 as i64));
                    match_rec.set(format!("{r_alias}.__dst"), Value::I64(m_dst.0 as i64));
                    match_rec.set(
                        format!("{r_alias}.__type"),
                        Value::String(edge_type.clone()),
                    );
                    match_rec.set(format!("{r_alias}.__edge_seq"), Value::I64(*m_seq as i64));
                    let edge_props =
                        edge::get_edge_properties_at(conn, *m_src, *m_dst, &edge_type, *m_seq)?;
                    for (key, val) in &edge_props {
                        match_rec.set(format!("{r_alias}.{key}"), val.clone());
                    }
                }
                // Bind path variable if present.
                if let Some(ref path_var) = merge_pattern.path_variable {
                    let src_node = node::get_node(conn, *m_src)?;
                    let dst_node = node::get_node(conn, *m_dst)?;
                    let ep =
                        edge::get_edge_properties_at(conn, *m_src, *m_dst, &edge_type, *m_seq)?;
                    match_rec.set(
                        path_var.clone(),
                        Value::Path(PathValue {
                            nodes: vec![src_node, dst_node],
                            edges: vec![crate::types::Edge {
                                src: *m_src,
                                dst: *m_dst,
                                label: edge_type.clone(),
                                properties: ep,
                            }],
                        }),
                    );
                }
                result.push(match_rec);
            }
        }
    }

    Ok(result)
}

/// Execute a CALL procedure: evaluate arguments, filter matching rows, yield columns.
pub(in crate::cypher::executor) fn find_merge_match_multi_label(
    conn: &Connection,
    labels: &[&str],
    properties: &Properties,
) -> Result<Option<crate::types::Node>> {
    let primary = labels.first().copied().unwrap_or("");
    let candidates = find_merge_matches_by_label_and_props(conn, primary, properties)?;
    // Require the node to carry every label in the pattern.
    Ok(candidates
        .into_iter()
        .find(|n| labels.iter().all(|req| n.labels.iter().any(|l| l == req))))
}

/// Collect all nodes matching a single label + property values (no multi-label filter).
///
/// When `label` is empty, performs a full node scan (handles label-less `MERGE (a)`).
pub(in crate::cypher::executor) fn find_merge_matches_by_label_and_props(
    conn: &Connection,
    label: &str,
    properties: &Properties,
) -> Result<Vec<crate::types::Node>> {
    // Try indexed lookup on the primary label (skipped when label is empty).
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<String> = indexes
        .iter()
        .filter(|info| info.properties.len() == 1)
        .map(|info| info.properties[0].clone())
        .collect();
    let indexed_props: Vec<&str> = indexed_props.iter().map(|s| s.as_str()).collect();

    for (key, value) in properties {
        if indexed_props.contains(&key.as_str()) {
            let ids = index::index_lookup(conn, label, key, value)?;
            let mut matches = Vec::new();
            for id in ids {
                let n = node::get_node(conn, id)?;
                let all_match = properties
                    .iter()
                    .all(|(k, expected)| n.properties.get(k) == Some(expected));
                if all_match {
                    matches.push(n);
                }
            }
            return Ok(matches);
        }
    }

    // Fall back to label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing
        .into_iter()
        .filter(|n| {
            properties
                .iter()
                .all(|(key, expected)| n.properties.get(key) == Some(expected))
        })
        .collect())
}

/// Find a node matching label + pre-evaluated property values.
pub(in crate::cypher::executor) fn find_merge_match_evaluated(
    conn: &Connection,
    label: &str,
    properties: &Properties,
) -> Result<Option<crate::types::Node>> {
    // Try to find an indexed property.
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<String> = indexes
        .iter()
        .filter(|info| info.properties.len() == 1)
        .map(|info| info.properties[0].clone())
        .collect();
    let indexed_props: Vec<&str> = indexed_props.iter().map(|s| s.as_str()).collect();

    for (key, value) in properties {
        if indexed_props.contains(&key.as_str()) {
            let ids = index::index_lookup(conn, label, key, value)?;
            // Filter candidates by remaining properties.
            for id in ids {
                let n = node::get_node(conn, id)?;
                let all_match = properties
                    .iter()
                    .all(|(k, expected)| n.properties.get(k) == Some(expected));
                if all_match {
                    return Ok(Some(n));
                }
            }
            return Ok(None);
        }
    }

    // No index available — fall back to label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing.into_iter().find(|n| {
        properties
            .iter()
            .all(|(key, expected)| n.properties.get(key) == Some(expected))
    }))
}

/// Find ALL nodes matching label + pre-evaluated property values.
pub(in crate::cypher::executor) fn find_merge_matches_evaluated(
    conn: &Connection,
    label: &str,
    properties: &Properties,
) -> Result<Vec<crate::types::Node>> {
    // Try indexed lookup first.
    let indexes = index::list_indexes_for_label(conn, label)?;
    let indexed_props: Vec<String> = indexes
        .iter()
        .filter(|info| info.properties.len() == 1)
        .map(|info| info.properties[0].clone())
        .collect();
    let indexed_props: Vec<&str> = indexed_props.iter().map(|s| s.as_str()).collect();

    for (key, value) in properties {
        if indexed_props.contains(&key.as_str()) {
            let ids = index::index_lookup(conn, label, key, value)?;
            let mut matches = Vec::new();
            for id in ids {
                let n = node::get_node(conn, id)?;
                let all_match = properties
                    .iter()
                    .all(|(k, expected)| n.properties.get(k) == Some(expected));
                if all_match {
                    matches.push(n);
                }
            }
            return Ok(matches);
        }
    }

    // No index — label scan.
    let existing = node::find_nodes_by_label(conn, label)?;
    Ok(existing
        .into_iter()
        .filter(|n| {
            properties
                .iter()
                .all(|(key, expected)| n.properties.get(key) == Some(expected))
        })
        .collect())
}
