//! MaterializePath, ShortestPath, build_path_value.

use rusqlite::Connection;

use crate::cypher::record::NamedRecord;
use crate::types::*;
use crate::{edge, node};

use super::read::*;
use super::*;

/// Bind `path_alias` to the path value of each record (the `MaterializePath`
/// operator over already executed input). Shared by the materialized and
/// correlated executors. A record whose path would use the same relationship
/// twice (e.g. a var-length segment reusing a fixed segment's edge) is
/// dropped: relationship uniqueness within a pattern.
pub(in crate::cypher::executor) fn materialize_paths(
    conn: &Connection,
    records: Vec<NamedRecord>,
    path_alias: &str,
    node_aliases: &[String],
    rel_aliases: &[String],
) -> Result<Vec<NamedRecord>> {
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

        if !has_null && edges.len() > 1 {
            let mut seen = std::collections::HashSet::with_capacity(edges.len());
            if !edges.iter().all(|e| seen.insert(edge_key(e))) {
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

/// Turn a BFS result into `PathValue`s, loading each node and the stored
/// edge(s) for each hop in storage orientation (so undirected hops keep
/// their real start/end nodes).
///
/// With `all_parallel`, every combination of parallel edges along the path
/// is a separate value (allShortestPaths); otherwise only the first edge per
/// hop is used, giving exactly one value (shortestPath).
pub(in crate::cypher::executor) fn build_path_values(
    conn: &Connection,
    path: &edge::LabeledPath,
    direction: Direction,
    all_parallel: bool,
) -> Result<Vec<crate::types::PathValue>> {
    use crate::types::{Edge, PathValue};

    let mut nodes = Vec::with_capacity(path.nodes.len());
    for id in &path.nodes {
        nodes.push(crate::node::get_node(conn, *id)?);
    }

    // Cartesian product of the parallel-edge choices for each hop.
    let mut edge_lists: Vec<Vec<Edge>> = vec![Vec::with_capacity(path.labels.len())];
    for (pair, label) in path.nodes.windows(2).zip(&path.labels) {
        let mut choices = edge::hop_edges(conn, pair[0], pair[1], label, direction)?;
        if !all_parallel {
            choices.truncate(1);
        }
        let mut next = Vec::with_capacity(edge_lists.len() * choices.len());
        for prefix in &edge_lists {
            for (src, dst, seq, properties) in &choices {
                let mut edges = prefix.clone();
                edges.push(Edge {
                    src: *src,
                    dst: *dst,
                    label: label.clone(),
                    seq: *seq,
                    properties: properties.clone(),
                });
                next.push(edges);
            }
        }
        edge_lists = next;
    }

    Ok(edge_lists
        .into_iter()
        .map(|edges| PathValue {
            nodes: nodes.clone(),
            edges,
        })
        .collect())
}

/// Run the materialized `ShortestPath` operator.
pub(in crate::cypher::executor) fn exec_shortest_path(
    conn: &Connection,
    plan: &LogicalOp,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let LogicalOp::ShortestPath { input, .. } = plan else {
        unreachable!("exec_shortest_path called with {}", plan.op_name());
    };
    let records = exec(conn, input, ctx)?;
    extend_with_shortest_paths(conn, plan, records, ctx)
}

/// Bind the shortest path(s) between each record's endpoints.
///
/// `plan` must be a `LogicalOp::ShortestPath`; `records` is its already
/// executed input. A record with no path within the hop bounds is dropped
/// (plain MATCH semantics); `OPTIONAL MATCH` turns that into a null through
/// its `LeftOuterJoin`. All searches in the query share one
/// `max_traversal_work` budget (`ctx.shortest_path_work`).
pub(in crate::cypher::executor) fn extend_with_shortest_paths(
    conn: &Connection,
    plan: &LogicalOp,
    records: Vec<NamedRecord>,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let LogicalOp::ShortestPath {
        src_alias,
        dst_alias,
        path_alias,
        edge_types,
        direction,
        min_hops,
        max_hops,
        all_paths,
        ..
    } = plan
    else {
        unreachable!("extend_with_shortest_paths called with {}", plan.op_name());
    };
    let labels: Vec<&str> = edge_types.iter().map(String::as_str).collect();
    let mut budget = edge::WorkBudget::resume(ctx.max_traversal_work, ctx.shortest_path_work.get());
    let mut results = Vec::new();

    for rec in records {
        let Some(src_id) = rec.get(src_alias).and_then(value_to_node_id) else {
            continue;
        };
        let Some(dst_id) = rec.get(dst_alias).and_then(value_to_node_id) else {
            continue;
        };

        let found = if *all_paths {
            edge::all_shortest_paths(
                conn,
                src_id,
                dst_id,
                &labels,
                *direction,
                *min_hops,
                *max_hops,
                &mut budget,
            )
        } else {
            edge::shortest_path(
                conn,
                src_id,
                dst_id,
                &labels,
                *direction,
                *min_hops,
                *max_hops,
                &mut budget,
            )
            .map(|p| p.into_iter().collect())
        };
        ctx.shortest_path_work.set(budget.used());
        for path in found? {
            for value in build_path_values(conn, &path, *direction, *all_paths)? {
                let mut new_rec = rec.clone();
                new_rec.set(path_alias.clone(), Value::Path(value));
                results.push(new_rec);
            }
        }
    }

    check_row_limit(&results, ctx)?;
    Ok(results)
}
