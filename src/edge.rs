use rusqlite::Connection;

use crate::storage::encoding::{
    decode_id_list, encode_id_list, insert_into_sorted, remove_from_sorted,
};
use crate::storage::kv;
use crate::types::{validate_name, Direction, GraphError, NodeId, Properties, Result};

/// Build the adjacency table key: [node_id: 8 bytes BE][label: UTF-8].
fn adj_key(node_id: NodeId, label: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(8 + label.len());
    key.extend_from_slice(&node_id.to_be_bytes());
    key.extend_from_slice(label.as_bytes());
    key
}

/// Build the edge properties key: [src: 8 BE][dst: 8 BE][label: UTF-8].
fn edge_props_key(src: NodeId, dst: NodeId, label: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(16 + label.len());
    key.extend_from_slice(&src.to_be_bytes());
    key.extend_from_slice(&dst.to_be_bytes());
    key.extend_from_slice(label.as_bytes());
    key
}

/// Create an edge from src to dst with the given label and properties.
pub fn create_edge(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
    properties: Properties,
) -> Result<()> {
    validate_name(label)?;
    for key in properties.keys() {
        validate_name(key)?;
    }
    // Update outgoing adjacency list for src.
    let out_key = adj_key(src, label);
    let mut out_ids = match kv::get(conn, kv::TABLE_ADJ_OUT, &out_key)? {
        Some(data) => decode_id_list(&data),
        None => Vec::new(),
    };
    insert_into_sorted(&mut out_ids, dst.0);
    kv::put(conn, kv::TABLE_ADJ_OUT, &out_key, &encode_id_list(&out_ids))?;

    // Update incoming adjacency list for dst.
    let in_key = adj_key(dst, label);
    let mut in_ids = match kv::get(conn, kv::TABLE_ADJ_IN, &in_key)? {
        Some(data) => decode_id_list(&data),
        None => Vec::new(),
    };
    insert_into_sorted(&mut in_ids, src.0);
    kv::put(conn, kv::TABLE_ADJ_IN, &in_key, &encode_id_list(&in_ids))?;

    // Store edge properties if non-empty.
    if !properties.is_empty() {
        let props_key = edge_props_key(src, dst, label);
        let data =
            rmp_serde::to_vec(&properties).map_err(|e| GraphError::Serialization(e.to_string()))?;
        kv::put(conn, kv::TABLE_EDGE_PROPS, &props_key, &data)?;
    }

    Ok(())
}

/// Delete an edge from src to dst with the given label.
pub fn delete_edge(conn: &Connection, src: NodeId, dst: NodeId, label: &str) -> Result<()> {
    // Update outgoing adjacency list.
    let out_key = adj_key(src, label);
    if let Some(data) = kv::get(conn, kv::TABLE_ADJ_OUT, &out_key)? {
        let mut ids = decode_id_list(&data);
        if remove_from_sorted(&mut ids, dst.0) {
            if ids.is_empty() {
                kv::delete(conn, kv::TABLE_ADJ_OUT, &out_key)?;
            } else {
                kv::put(conn, kv::TABLE_ADJ_OUT, &out_key, &encode_id_list(&ids))?;
            }
        }
    }

    // Update incoming adjacency list.
    let in_key = adj_key(dst, label);
    if let Some(data) = kv::get(conn, kv::TABLE_ADJ_IN, &in_key)? {
        let mut ids = decode_id_list(&data);
        if remove_from_sorted(&mut ids, src.0) {
            if ids.is_empty() {
                kv::delete(conn, kv::TABLE_ADJ_IN, &in_key)?;
            } else {
                kv::put(conn, kv::TABLE_ADJ_IN, &in_key, &encode_id_list(&ids))?;
            }
        }
    }

    // Delete edge properties.
    let props_key = edge_props_key(src, dst, label);
    kv::delete(conn, kv::TABLE_EDGE_PROPS, &props_key)?;

    Ok(())
}

/// Get neighbor node IDs for a given node, edge label, and direction.
pub fn get_neighbors(
    conn: &Connection,
    id: NodeId,
    label: &str,
    direction: Direction,
) -> Result<Vec<NodeId>> {
    let mut result = Vec::new();

    if matches!(direction, Direction::Outgoing | Direction::Both) {
        let key = adj_key(id, label);
        if let Some(data) = kv::get(conn, kv::TABLE_ADJ_OUT, &key)? {
            result.extend(decode_id_list(&data).into_iter().map(NodeId));
        }
    }

    if matches!(direction, Direction::Incoming | Direction::Both) {
        let key = adj_key(id, label);
        if let Some(data) = kv::get(conn, kv::TABLE_ADJ_IN, &key)? {
            for raw_id in decode_id_list(&data) {
                let nid = NodeId(raw_id);
                // Avoid duplicates in Both direction.
                if direction == Direction::Both && result.contains(&nid) {
                    continue;
                }
                result.push(nid);
            }
        }
    }

    Ok(result)
}

/// Get edge properties for a specific edge.
pub fn get_edge_properties(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
) -> Result<Properties> {
    let key = edge_props_key(src, dst, label);
    match kv::get(conn, kv::TABLE_EDGE_PROPS, &key)? {
        Some(data) => {
            let props: Properties = rmp_serde::from_slice(&data)
                .map_err(|e| GraphError::Serialization(e.to_string()))?;
            Ok(props)
        }
        None => Ok(Properties::new()),
    }
}

/// Variable-length path traversal using BFS.
///
/// Returns all distinct node IDs reachable from `start` by following edges with
/// the given `label` and `direction`, between `min_hops` and `max_hops` inclusive.
/// The start node is never included in the result (even for cycles at hop 0).
pub fn traverse(
    conn: &Connection,
    start: NodeId,
    label: &str,
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
) -> Result<Vec<NodeId>> {
    use std::collections::{HashSet, VecDeque};

    let mut visited: HashSet<u64> = HashSet::new();
    let mut result: Vec<NodeId> = Vec::new();

    // BFS queue: (node_id, current_depth)
    let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();
    queue.push_back((start, 0));
    visited.insert(start.0);

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= max_hops {
            continue;
        }

        let neighbors = get_neighbors(conn, current, label, direction)?;
        let next_depth = depth + 1;

        for neighbor in neighbors {
            if next_depth >= min_hops && neighbor != start {
                // Only add to result once.
                if !result.contains(&neighbor) {
                    result.push(neighbor);
                }
            }

            // Only traverse further if we haven't visited this node yet.
            if visited.insert(neighbor.0) {
                queue.push_back((neighbor, next_depth));
            }
        }
    }

    Ok(result)
}

/// Find the shortest path between two nodes using BFS.
///
/// Returns the path as an ordered list of node IDs (including start and end),
/// or `None` if no path exists within `max_hops`.
pub fn shortest_path(
    conn: &Connection,
    start: NodeId,
    end: NodeId,
    label: &str,
    direction: Direction,
    max_hops: u32,
) -> Result<Option<Vec<NodeId>>> {
    use std::collections::{HashMap, VecDeque};

    if start == end {
        return Ok(Some(vec![start]));
    }

    // BFS with parent tracking.
    let mut parent: HashMap<u64, u64> = HashMap::new();
    let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();
    queue.push_back((start, 0));
    parent.insert(start.0, start.0); // sentinel: start's parent is itself

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= max_hops {
            continue;
        }

        let neighbors = get_neighbors(conn, current, label, direction)?;
        for neighbor in neighbors {
            if parent.contains_key(&neighbor.0) {
                continue; // already visited
            }
            parent.insert(neighbor.0, current.0);

            if neighbor == end {
                // Reconstruct path.
                let mut path = vec![end];
                let mut cur = end.0;
                while cur != start.0 {
                    cur = parent[&cur];
                    path.push(NodeId(cur));
                }
                path.reverse();
                return Ok(Some(path));
            }

            queue.push_back((neighbor, depth + 1));
        }
    }

    Ok(None)
}

/// Find all shortest paths between two nodes using BFS.
///
/// Returns all paths of minimum length as ordered lists of node IDs.
/// Returns an empty vec if no path exists within `max_hops`.
pub fn all_shortest_paths(
    conn: &Connection,
    start: NodeId,
    end: NodeId,
    label: &str,
    direction: Direction,
    max_hops: u32,
) -> Result<Vec<Vec<NodeId>>> {
    use std::collections::{HashMap, VecDeque};

    if start == end {
        return Ok(vec![vec![start]]);
    }

    // BFS tracking ALL parents per node (not just the first).
    // parents[node] = set of nodes that reach it at the shortest distance.
    let mut parents: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut depth_of: HashMap<u64, u32> = HashMap::new();
    let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();

    queue.push_back((start, 0));
    depth_of.insert(start.0, 0);
    parents.insert(start.0, vec![]);

    let mut found_depth: Option<u32> = None;

    while let Some((current, depth)) = queue.pop_front() {
        // If we've already found the target at a shorter depth, stop.
        if let Some(fd) = found_depth {
            if depth >= fd {
                continue;
            }
        }
        if depth >= max_hops {
            continue;
        }

        let neighbors = get_neighbors(conn, current, label, direction)?;
        let next_depth = depth + 1;

        for neighbor in neighbors {
            if let Some(&existing_depth) = depth_of.get(&neighbor.0) {
                // Already visited at this or shorter depth — add parent if same depth.
                if existing_depth == next_depth {
                    parents.entry(neighbor.0).or_default().push(current.0);
                }
                continue;
            }

            // First visit.
            depth_of.insert(neighbor.0, next_depth);
            parents.insert(neighbor.0, vec![current.0]);

            if neighbor == end {
                found_depth = Some(next_depth);
                // Don't stop yet — finish this BFS level to find all equal-length paths.
            } else if found_depth.is_none() {
                queue.push_back((neighbor, next_depth));
            }
        }
    }

    if !depth_of.contains_key(&end.0) {
        return Ok(vec![]);
    }

    // Reconstruct all paths from end back to start using parent pointers.
    let mut all_paths = Vec::new();
    let mut stack: Vec<(u64, Vec<NodeId>)> = vec![(end.0, vec![end])];

    while let Some((node, path)) = stack.pop() {
        if node == start.0 {
            let mut complete = path;
            complete.reverse();
            all_paths.push(complete);
            continue;
        }
        if let Some(pars) = parents.get(&node) {
            for &p in pars {
                let mut extended = path.clone();
                extended.push(NodeId(p));
                stack.push((p, extended));
            }
        }
    }

    Ok(all_paths)
}

/// Get all edge labels and their neighbor IDs for a node in a given direction.
/// Used internally for cascading node deletion.
pub fn get_all_edge_labels(
    conn: &Connection,
    id: NodeId,
    direction: Direction,
) -> Result<Vec<(String, Vec<u64>)>> {
    let table = match direction {
        Direction::Outgoing => kv::TABLE_ADJ_OUT,
        Direction::Incoming => kv::TABLE_ADJ_IN,
        Direction::Both => {
            // Combine both directions.
            let mut result = get_all_edge_labels(conn, id, Direction::Outgoing)?;
            result.extend(get_all_edge_labels(conn, id, Direction::Incoming)?);
            return Ok(result);
        }
    };

    let prefix = id.to_be_bytes();
    let entries = kv::scan_prefix(conn, table, &prefix)?;

    let mut result = Vec::new();
    for (key, data) in entries {
        if key.len() > 8 {
            let label = String::from_utf8(key[8..].to_vec()).map_err(|e| {
                GraphError::Serialization(format!("invalid UTF-8 in edge label: {e}"))
            })?;
            let ids = decode_id_list(&data);
            result.push((label, ids));
        }
    }
    Ok(result)
}
