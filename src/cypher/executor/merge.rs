//! MERGE evaluation.
//!
//! openCypher semantics: for each input row, MERGE matches the *whole*
//! pattern against the graph, with the row's already-bound variables fixed.
//! Every match produces an output row (ON MATCH applied to it). When nothing
//! matches, the unbound parts of the pattern (nodes with their labels and
//! properties, and the relationship) are created and ON CREATE is applied.
//! ON CREATE / ON MATCH items are dispatched on the variable they name, so
//! they can update the merged node, the merged relationship, or any other
//! variable bound in the row.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::{cypher_equals, eval_expr};
use crate::cypher::record::NamedRecord;
use crate::types::*;
use crate::{edge, index, node};

use super::util::populate_node_bindings;
use super::write::{
    apply_set_item, bound_node_id, create_indexed_node, validate_write_labels,
    validate_write_properties,
};
use super::*;

/// Standalone MERGE (no preceding clause): merge against an empty row.
pub(in crate::cypher::executor) fn exec_merge(
    conn: &Connection,
    pattern: &Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut out = Vec::new();
    let merge = Merge {
        pattern,
        on_create,
        on_match,
    };
    merge.row(conn, &NamedRecord::new(), &mut out, ctx)?;
    Ok(out)
}

/// MERGE after other clauses: merge once per input row.
pub(in crate::cypher::executor) fn exec_match_merge(
    conn: &Connection,
    input: &LogicalOp,
    merge_pattern: &Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut out = Vec::with_capacity(records.len());
    let merge = Merge {
        pattern: merge_pattern,
        on_create,
        on_match,
    };
    for rec in &records {
        merge.row(conn, rec, &mut out, ctx)?;
    }
    Ok(out)
}

/// A MERGE clause: its pattern and ON CREATE / ON MATCH items.
struct Merge<'a> {
    pattern: &'a Pattern,
    on_create: &'a [SetItem],
    on_match: &'a [SetItem],
}

/// One endpoint of a relationship MERGE pattern, as seen from the input row.
enum End {
    /// The variable is bound in the row: only this node may match.
    Bound(NodeId),
    /// Unbound: any node with these labels and properties may match; one is
    /// created when the pattern does not match.
    Free {
        labels: Vec<String>,
        props: Properties,
    },
    /// The right endpoint repeats the (unbound) left variable: a self-loop.
    SameAsLeft,
}

impl Merge<'_> {
    /// Merge the pattern for input row `rec`, appending the output rows.
    fn row(
        &self,
        conn: &Connection,
        rec: &NamedRecord,
        out: &mut Vec<NamedRecord>,
        ctx: &ExecContext,
    ) -> Result<()> {
        match self.pattern.elements.as_slice() {
            [PatternElement::Node(n)] => self.node_row(conn, rec, n, out, ctx),
            [PatternElement::Node(l), PatternElement::Relationship(r), PatternElement::Node(rt)] => {
                self.rel_row(conn, rec, l, r, rt, out, ctx)
            }
            _ => Err(GraphError::semantic(
                "MERGE pattern must be a single node or (node)-[rel]->(node)",
            )),
        }
    }

    /// Single-node MERGE: bind every node matching the pattern, or create one.
    fn node_row(
        &self,
        conn: &Connection,
        rec: &NamedRecord,
        node_pat: &NodePattern,
        out: &mut Vec<NamedRecord>,
        ctx: &ExecContext,
    ) -> Result<()> {
        let props = eval_merge_properties(conn, rec, &node_pat.properties)?;
        let matches = find_merge_nodes(conn, &node_pat.labels, &props)?;
        if matches.is_empty() {
            let created = create_indexed_node(conn, &node_pat.labels, props)?;
            let row = self.node_output(conn, rec, node_pat, &created, self.on_create)?;
            return push_capped(out, row, ctx);
        }
        for n in &matches {
            let row = self.node_output(conn, rec, node_pat, n, self.on_match)?;
            push_capped(out, row, ctx)?;
        }
        Ok(())
    }

    /// The output row for a merged node: `rec` plus the node binding, with
    /// `items` (ON CREATE or ON MATCH) applied.
    fn node_output(
        &self,
        conn: &Connection,
        rec: &NamedRecord,
        node_pat: &NodePattern,
        merged: &Node,
        items: &[SetItem],
    ) -> Result<NamedRecord> {
        let mut row = rec.clone();
        if let Some(v) = &node_pat.variable {
            populate_node_bindings(&mut row, merged, v);
        }
        for item in items {
            apply_set_item(conn, &mut row, item)?;
        }
        if let Some(p) = &self.pattern.path_variable {
            let current = node::get_node(conn, merged.id)?;
            row.set(p.clone(), Value::Path(PathValue::single(current)));
        }
        Ok(row)
    }

    /// Relationship MERGE `(l)-[r:T {props}]->(rt)` (any direction): bind
    /// every occurrence of the whole pattern, or create its unbound parts.
    #[allow(clippy::too_many_arguments)]
    fn rel_row(
        &self,
        conn: &Connection,
        rec: &NamedRecord,
        left_pat: &NodePattern,
        rel: &RelPattern,
        right_pat: &NodePattern,
        out: &mut Vec<NamedRecord>,
        ctx: &ExecContext,
    ) -> Result<()> {
        let left = resolve_end(conn, rec, left_pat)?;
        let right = match (&left_pat.variable, &right_pat.variable, &left) {
            (Some(l), Some(r), End::Free { .. }) if l == r => End::SameAsLeft,
            _ => resolve_end(conn, rec, right_pat)?,
        };
        let edge_type = rel.rel_types.first().cloned().unwrap_or_default();
        let edge_props = eval_merge_properties(conn, rec, &rel.properties)?;
        // Direction of the relationship as seen from the left endpoint.
        let from_left = match rel.direction {
            RelDirection::Outgoing => Direction::Outgoing,
            RelDirection::Incoming => Direction::Incoming,
            RelDirection::Undirected => Direction::Both,
        };

        let matches = find_merge_rels(conn, &left, &right, &edge_type, from_left, &edge_props)?;
        if matches.is_empty() {
            let left_id = match &left {
                End::Bound(id) => *id,
                End::Free { labels, props } => create_indexed_node(conn, labels, props.clone())?.id,
                End::SameAsLeft => unreachable!("only the right endpoint can repeat the left"),
            };
            let right_id = match &right {
                End::Bound(id) => *id,
                End::Free { labels, props } => create_indexed_node(conn, labels, props.clone())?.id,
                End::SameAsLeft => left_id,
            };
            // An undirected MERGE creates the relationship left to right.
            let (src, dst) = match rel.direction {
                RelDirection::Incoming => (right_id, left_id),
                _ => (left_id, right_id),
            };
            validate_write_labels(std::slice::from_ref(&edge_type))?;
            validate_write_properties(&edge_props)?;
            let seq = edge::create_edge(conn, src, dst, &edge_type, edge_props.clone())?;
            let created = Edge {
                src,
                dst,
                label: edge_type,
                seq,
                properties: edge_props,
            };
            let ends = [(left_pat, left_id), (right_pat, right_id)];
            let row = self.rel_output(conn, rec, ends, rel, &created, self.on_create)?;
            return push_capped(out, row, ctx);
        }
        for (left_id, right_id, e) in &matches {
            let ends = [(left_pat, *left_id), (right_pat, *right_id)];
            let row = self.rel_output(conn, rec, ends, rel, e, self.on_match)?;
            push_capped(out, row, ctx)?;
        }
        Ok(())
    }

    /// The output row for a merged relationship: `rec` plus bindings for the
    /// pattern's newly bound nodes and the relationship, with `items` applied.
    fn rel_output(
        &self,
        conn: &Connection,
        rec: &NamedRecord,
        ends: [(&NodePattern, NodeId); 2],
        rel: &RelPattern,
        merged: &Edge,
        items: &[SetItem],
    ) -> Result<NamedRecord> {
        let mut row = rec.clone();
        for (pat, id) in ends {
            if let Some(v) = &pat.variable {
                if rec.get(v).is_none() {
                    populate_node_bindings(&mut row, &node::get_node(conn, id)?, v);
                }
            }
        }
        if let Some(r) = &rel.variable {
            row.set(r.clone(), Value::String(merged.label.clone()));
            populate_edge_bindings(&mut row, merged, r);
        }
        for item in items {
            apply_set_item(conn, &mut row, item)?;
        }
        if let Some(p) = &self.pattern.path_variable {
            let properties = edge::get_edge_properties_at(
                conn,
                merged.src,
                merged.dst,
                &merged.label,
                merged.seq,
            )?;
            row.set(
                p.clone(),
                Value::Path(PathValue {
                    nodes: vec![
                        node::get_node(conn, ends[0].1)?,
                        node::get_node(conn, ends[1].1)?,
                    ],
                    edges: vec![Edge {
                        properties,
                        ..merged.clone()
                    }],
                }),
            );
        }
        Ok(row)
    }
}

/// Classify one endpoint of a relationship pattern against the input row.
fn resolve_end(conn: &Connection, rec: &NamedRecord, pat: &NodePattern) -> Result<End> {
    if let Some(v) = &pat.variable {
        if rec.get(v).is_some() {
            return Ok(End::Bound(bound_node_id(conn, rec, v, "MERGE")?));
        }
    }
    Ok(End::Free {
        labels: pat.labels.clone(),
        props: eval_merge_properties(conn, rec, &pat.properties)?,
    })
}

/// Evaluate a MERGE pattern's property map against `rec`. A null value can
/// never match and is rejected (`MergeReadOwnWrites`).
fn eval_merge_properties(
    conn: &Connection,
    rec: &NamedRecord,
    properties: &HashMap<String, Expr>,
) -> Result<Properties> {
    let mut props = Properties::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
        if val == Value::Null {
            return Err(GraphError::semantic("MERGE with null property value")
                .with_code(ErrorCode::MergeReadOwnWrites));
        }
        props.insert(key.clone(), val);
    }
    Ok(props)
}

/// Whether `props` (pattern properties) all equal the stored `actual` ones
/// under Cypher equality (`1 = 1.0`).
fn props_match(actual: &Properties, props: &Properties) -> bool {
    props
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|stored| cypher_equals(stored, v)))
}

/// Whether node `n` carries every label in `labels` and matches `props`.
fn node_matches(n: &Node, labels: &[String], props: &Properties) -> bool {
    labels.iter().all(|l| n.labels.contains(l)) && props_match(&n.properties, props)
}

/// The other numeric representation of `v` that Cypher considers equal
/// (`1` for `1.0` and vice versa), for probing an index whose keys are
/// type-exact. Over-inclusion is harmless: candidates are re-checked.
fn numeric_twin(v: &Value) -> Option<Value> {
    match v {
        Value::I64(i) => Some(Value::F64(*i as f64)),
        Value::F64(f) if f.is_finite() && f.fract() == 0.0 => Some(Value::I64(*f as i64)),
        _ => None,
    }
}

/// Every node carrying all of `labels` and matching `props`. Candidates come
/// from a single-property index on the first label when one covers a
/// pattern property, else from a label scan (a full scan without labels).
fn find_merge_nodes(conn: &Connection, labels: &[String], props: &Properties) -> Result<Vec<Node>> {
    let primary = labels.first().map(String::as_str).unwrap_or("");
    let indexed: Vec<String> = index::list_indexes_for_label(conn, primary)?
        .into_iter()
        .filter(|info| info.properties.len() == 1)
        .map(|mut info| info.properties.remove(0))
        .collect();
    let probe = props
        .iter()
        .find(|(k, v)| indexed.contains(k) && !matches!(v, Value::List(_) | Value::Map(_)));
    let candidates = match probe {
        Some((key, value)) => {
            let mut ids = index::index_lookup(conn, primary, key, value)?;
            if let Some(twin) = numeric_twin(value) {
                ids.extend(index::index_lookup(conn, primary, key, &twin)?);
            }
            ids.sort_by_key(|id| id.0);
            ids.dedup();
            ids.into_iter()
                .map(|id| node::get_node(conn, id))
                .collect::<Result<Vec<_>>>()?
        }
        None => node::find_nodes_by_label(conn, primary)?,
    };
    Ok(candidates
        .into_iter()
        .filter(|n| node_matches(n, labels, props))
        .collect())
}

/// Every relationship of type `edge_type` incident to `id` in direction
/// `dir`, with the node at its other end. A self-loop is listed once.
fn incident_edges(
    conn: &Connection,
    id: NodeId,
    edge_type: &str,
    dir: Direction,
) -> Result<Vec<(NodeId, Edge)>> {
    let mut out = Vec::new();
    let mut add = |src: NodeId, dst: NodeId, other: NodeId| -> Result<()> {
        for (seq, properties) in edge::get_all_edge_props(conn, src, dst, edge_type)? {
            out.push((
                other,
                Edge {
                    src,
                    dst,
                    label: edge_type.to_string(),
                    seq,
                    properties,
                },
            ));
        }
        Ok(())
    };
    if matches!(dir, Direction::Outgoing | Direction::Both) {
        for nb in edge::get_neighbors(conn, id, edge_type, Direction::Outgoing)? {
            add(id, nb, nb)?;
        }
    }
    if matches!(dir, Direction::Incoming | Direction::Both) {
        for nb in edge::get_neighbors(conn, id, edge_type, Direction::Incoming)? {
            if dir == Direction::Both && nb == id {
                continue; // self-loop already listed as outgoing
            }
            add(nb, id, nb)?;
        }
    }
    Ok(out)
}

/// Every occurrence of `(left)-[:edge_type {edge_props}]-(right)` (direction
/// `from_left` as seen from the left endpoint), as `(left, right, edge)`.
/// The search starts from a bound endpoint when there is one, so it only
/// walks that node's relationships.
fn find_merge_rels(
    conn: &Connection,
    left: &End,
    right: &End,
    edge_type: &str,
    from_left: Direction,
    edge_props: &Properties,
) -> Result<Vec<(NodeId, NodeId, Edge)>> {
    // Anchor on the right endpoint only when it is bound and the left is not.
    let anchor_right = matches!(right, End::Bound(_)) && !matches!(left, End::Bound(_));
    let (anchor, other, dir) = if anchor_right {
        let flipped = match from_left {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
            Direction::Both => Direction::Both,
        };
        (right, left, flipped)
    } else {
        (left, right, from_left)
    };
    let anchors: Vec<NodeId> = match anchor {
        End::Bound(id) => vec![*id],
        End::Free { labels, props } => find_merge_nodes(conn, labels, props)?
            .into_iter()
            .map(|n| n.id)
            .collect(),
        End::SameAsLeft => unreachable!("the anchor is never a repeated variable"),
    };

    let mut found = Vec::new();
    for a in anchors {
        for (o, e) in incident_edges(conn, a, edge_type, dir)? {
            if !props_match(&e.properties, edge_props) {
                continue;
            }
            let other_ok = match other {
                End::Bound(id) => o == *id,
                End::SameAsLeft => o == a,
                End::Free { labels, props } => {
                    node_matches(&node::get_node(conn, o)?, labels, props)
                }
            };
            if other_ok {
                found.push(if anchor_right { (o, a, e) } else { (a, o, e) });
            }
        }
    }
    Ok(found)
}
