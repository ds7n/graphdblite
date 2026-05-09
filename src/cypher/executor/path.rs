//! MaterializePath, ShortestPath, build_path_value.

use rusqlite::Connection;

use crate::cypher::record::NamedRecord;
use crate::types::*;
use crate::{edge, node};

use super::read::*;
use super::*;

pub(in crate::cypher::executor) fn exec_materialize_path(
    conn: &Connection,
    input: &LogicalOp,
    path_alias: &str,
    node_aliases: &[String],
    rel_aliases: &[String],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in records {
        // If any node alias is Null (from unmatched OPTIONAL MATCH),
        // the entire path is Null.
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

        // Var-length path node reconstruction.
        let has_var_length_rel = rel_aliases
            .iter()
            .any(|a| matches!(rec.get(a), Some(Value::List(_))));
        if !has_null && has_var_length_rel {
            if edges.is_empty() {
                // Zero-length var-length path: collapse to single start node.
                nodes.truncate(1);
            } else {
                // Rebuild full node list from edge endpoints. This is necessary
                // because with multiple var-length segments and zero-length matches,
                // the static node_aliases may contain duplicate intermediate nodes.
                let mut full_nodes = vec![nodes[0].clone()];
                for edge in &edges {
                    // Determine next node: for undirected traversals, the
                    // edge may be stored in either direction. Pick the
                    // endpoint that differs from the previous node.
                    let prev_id = full_nodes.last().unwrap().id;
                    let next_id = if prev_id == edge.src {
                        edge.dst
                    } else {
                        edge.src
                    };
                    let n = node::get_node(conn, next_id)?;
                    full_nodes.push(n);
                }
                // Use the last node from node_aliases if available (has correct label filter).
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
                Value::Path(PathValue { nodes, edges }),
            );
        }
        results.push(new_rec);
    }

    Ok(results)
}

/// Aggregate pre-computed records (used by correlated execution).
/// Reuses the existing `compute_aggregate` function.
pub(in crate::cypher::executor) fn build_path_value(
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
pub(in crate::cypher::executor) fn exec_shortest_path(
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
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let label = edge_type.unwrap_or("");
    let mut results = Vec::new();

    for rec in &records {
        let src_id = match rec.get(src_alias).and_then(value_to_node_id) {
            Some(id) => id,
            _ => continue,
        };
        let dst_id = match rec.get(dst_alias).and_then(value_to_node_id) {
            Some(id) => id,
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
