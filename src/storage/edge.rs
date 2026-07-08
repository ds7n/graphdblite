use std::collections::HashMap;

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

/// Build the edge properties key: [src: 8 BE][dst: 8 BE][label: UTF-8][0x00][seq: 8 BE].
///
/// The NUL separator distinguishes the label from the sequence number.
/// Sequence numbers allow multiple parallel edges between the same (src, dst, label).
fn edge_props_key(src: NodeId, dst: NodeId, label: &str, seq: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(16 + label.len() + 1 + 8);
    key.extend_from_slice(&src.to_be_bytes());
    key.extend_from_slice(&dst.to_be_bytes());
    key.extend_from_slice(label.as_bytes());
    key.push(0x00);
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

/// Build the edge properties prefix for scanning all parallel edges:
/// [src: 8 BE][dst: 8 BE][label: UTF-8][0x00].
pub(crate) fn edge_props_prefix(src: NodeId, dst: NodeId, label: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(16 + label.len() + 1);
    key.extend_from_slice(&src.to_be_bytes());
    key.extend_from_slice(&dst.to_be_bytes());
    key.extend_from_slice(label.as_bytes());
    key.push(0x00);
    key
}

/// Extract the sequence number from an edge_props key.
pub(crate) fn edge_seq_from_key(key: &[u8], prefix_len: usize) -> u64 {
    if key.len() >= prefix_len + 8 {
        let bytes: [u8; 8] = key[prefix_len..prefix_len + 8].try_into().unwrap_or([0; 8]);
        u64::from_be_bytes(bytes)
    } else {
        0
    }
}

/// Extract the label/relationship type from an `edge_props` key.
///
/// Layout: `[src:8][dst:8][label_bytes][0x00][seq:8]`. Returns `None` if
/// the key is too short (< 16 bytes for the node-id prefix) or has no
/// `0x00` separator after the label.
pub(crate) fn label_from_edge_props_key(key: &[u8]) -> Option<&str> {
    if key.len() < 16 {
        return None;
    }
    let rest = &key[16..];
    let nul = rest.iter().position(|&b| b == 0x00)?;
    std::str::from_utf8(&rest[..nul]).ok()
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

    // Use global monotonic edge sequence so keys are never recycled after deletion.
    let next_seq = crate::id::next_edge_seq(conn)?;

    // Always store an edge_props row so relationship counting works correctly.
    let props_key = edge_props_key(src, dst, label, next_seq);
    let data = rmp_serde::to_vec(&properties).map_err(|e| GraphError::Serialization {
        context: String::new(),
        source: e.to_string(),
        hint: None,
    })?;
    kv::put(conn, kv::TABLE_EDGE_PROPS, &props_key, &data)?;
    crate::stats::increment_edge_type_count(conn, label)?;

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

    // Delete all parallel edge properties for this (src, dst, label).
    let prefix = edge_props_prefix(src, dst, label);
    let entries = kv::scan_prefix(conn, kv::TABLE_EDGE_PROPS, &prefix)?;
    let removed = entries.len() as u64;
    for (key, _) in entries {
        kv::delete(conn, kv::TABLE_EDGE_PROPS, &key)?;
    }
    if removed > 0 {
        crate::stats::decrement_edge_type_count_by(conn, label, removed)?;
    }

    Ok(())
}

/// Delete a single edge identified by (src, dst, label, seq).
///
/// If this was the last parallel edge, also removes the adjacency list entries.
pub fn delete_single_edge(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
    seq: u64,
) -> Result<()> {
    let props_key = edge_props_key(src, dst, label, seq);
    // Only decrement the edge-type counter when a row was actually removed;
    // a redundant delete of an already-gone edge (e.g. the same parallel edge
    // bound in two output rows) must not drift the counter below the live count.
    let removed = kv::delete(conn, kv::TABLE_EDGE_PROPS, &props_key)?;
    if removed {
        crate::stats::decrement_edge_type_count_by(conn, label, 1)?;
    }

    // Check if there are remaining parallel edges.
    let prefix = edge_props_prefix(src, dst, label);
    let remaining = kv::scan_prefix(conn, kv::TABLE_EDGE_PROPS, &prefix)?;
    if remaining.is_empty() {
        // Last edge — remove from adjacency lists.
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
    }
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

/// Get edge properties for the first edge between (src, dst, label).
///
/// For backward compatibility, returns the first parallel edge's properties.
/// Use `get_edge_properties_at` for a specific edge by sequence number, or
/// `get_all_edge_props` to enumerate all parallel edges.
pub fn get_edge_properties(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
) -> Result<Properties> {
    let prefix = edge_props_prefix(src, dst, label);
    let entries = kv::scan_prefix(conn, kv::TABLE_EDGE_PROPS, &prefix)?;
    match entries.into_iter().next() {
        Some((_, data)) => {
            let props: Properties =
                rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization {
                    context: String::new(),
                    source: e.to_string(),
                    hint: None,
                })?;
            Ok(props)
        }
        None => Ok(Properties::new()),
    }
}

/// Get edge properties for a specific parallel edge identified by sequence number.
pub fn get_edge_properties_at(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
    seq: u64,
) -> Result<Properties> {
    let key = edge_props_key(src, dst, label, seq);
    match kv::get(conn, kv::TABLE_EDGE_PROPS, &key)? {
        Some(data) => {
            let props: Properties =
                rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization {
                    context: String::new(),
                    source: e.to_string(),
                    hint: None,
                })?;
            Ok(props)
        }
        None => Ok(Properties::new()),
    }
}

/// Get all parallel edges between (src, dst, label), returning (seq, Properties) pairs.
pub fn get_all_edge_props(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
) -> Result<Vec<(u64, Properties)>> {
    let prefix = edge_props_prefix(src, dst, label);
    let entries = kv::scan_prefix(conn, kv::TABLE_EDGE_PROPS, &prefix)?;
    let mut result = Vec::with_capacity(entries.len());
    for (key, data) in entries {
        let seq = edge_seq_from_key(&key, prefix.len());
        let props: Properties =
            rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization {
                context: String::new(),
                source: e.to_string(),
                hint: None,
            })?;
        result.push((seq, props));
    }
    Ok(result)
}

/// Set a single property on the first edge between (src, dst, label).
///
/// Use `set_edge_property_at` to target a specific parallel edge by sequence.
pub fn set_edge_property(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
    key: &str,
    value: crate::types::Value,
) -> Result<()> {
    // Find the first edge's sequence number.
    let prefix = edge_props_prefix(src, dst, label);
    let entries = kv::scan_prefix(conn, kv::TABLE_EDGE_PROPS, &prefix)?;
    let seq = entries
        .first()
        .map(|(k, _)| edge_seq_from_key(k, prefix.len()))
        .unwrap_or(0);
    set_edge_property_at(conn, src, dst, label, seq, key, value)
}

/// Set a single property on a specific parallel edge identified by sequence number.
pub fn set_edge_property_at(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
    seq: u64,
    key: &str,
    value: crate::types::Value,
) -> Result<()> {
    let mut props = get_edge_properties_at(conn, src, dst, label, seq)?;
    if value == crate::types::Value::Null {
        props.remove(key);
    } else {
        props.insert(key.to_string(), value);
    }
    let props_key = edge_props_key(src, dst, label, seq);
    let data = rmp_serde::to_vec(&props).map_err(|e| GraphError::Serialization {
        context: String::new(),
        source: e.to_string(),
        hint: None,
    })?;
    kv::put(conn, kv::TABLE_EDGE_PROPS, &props_key, &data)?;
    Ok(())
}

/// Replace all properties on a specific parallel edge with the given map.
pub fn set_all_edge_properties_at(
    conn: &Connection,
    src: NodeId,
    dst: NodeId,
    label: &str,
    seq: u64,
    properties: crate::types::Properties,
) -> Result<()> {
    let props_key = edge_props_key(src, dst, label, seq);
    let data = rmp_serde::to_vec(&properties).map_err(|e| GraphError::Serialization {
        context: String::new(),
        source: e.to_string(),
        hint: None,
    })?;
    kv::put(conn, kv::TABLE_EDGE_PROPS, &props_key, &data)?;
    Ok(())
}

/// Check if a specific edge exists.
pub fn edge_exists(conn: &Connection, src: NodeId, dst: NodeId, label: &str) -> Result<bool> {
    let out_key = adj_key(src, label);
    match kv::get(conn, kv::TABLE_ADJ_OUT, &out_key)? {
        Some(data) => {
            let ids = decode_id_list(&data);
            Ok(ids.binary_search(&dst.0).is_ok())
        }
        None => Ok(false),
    }
}

/// Variable-length path traversal using BFS.
///
/// Returns all distinct node IDs reachable from `start` by following edges with
/// the given `label` and `direction`, between `min_hops` and `max_hops` inclusive.
/// The start node is never included in the result (even for cycles at hop 0).
///
/// When `max_results` is set, traversal stops once that many distinct nodes have
/// been collected. Useful for limit pushdown.
pub fn traverse(
    conn: &Connection,
    start: NodeId,
    label: &str,
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    max_results: Option<usize>,
) -> Result<Vec<NodeId>> {
    use std::collections::{HashSet, VecDeque};

    let mut visited: HashSet<u64> = HashSet::new();
    let mut result: Vec<NodeId> = Vec::new();
    let mut in_result: HashSet<u64> = HashSet::new();

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
            if next_depth >= min_hops && neighbor != start && in_result.insert(neighbor.0) {
                result.push(neighbor);
                if let Some(cap) = max_results {
                    if result.len() >= cap {
                        return Ok(result);
                    }
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

/// Variable-length path traversal using BFS, with depth tracking.
///
/// Returns `(node_id, depth)` pairs for all distinct nodes reachable from `start`
/// by following edges with the given `label` and `direction`, between `min_hops`
/// and `max_hops` inclusive. Results are ordered by depth (closest first).
/// The start node is never included in the result.
pub fn traverse_with_depth(
    conn: &Connection,
    start: NodeId,
    label: &str,
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    max_results: Option<usize>,
) -> Result<Vec<(NodeId, u32)>> {
    use std::collections::{HashSet, VecDeque};

    let mut visited: HashSet<u64> = HashSet::new();
    let mut result: Vec<(NodeId, u32)> = Vec::new();
    let mut in_result: HashSet<u64> = HashSet::new();

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
            if next_depth >= min_hops && neighbor != start && in_result.insert(neighbor.0) {
                result.push((neighbor, next_depth));
                if let Some(cap) = max_results {
                    if result.len() >= cap {
                        return Ok(result);
                    }
                }
            }

            if visited.insert(neighbor.0) {
                queue.push_back((neighbor, next_depth));
            }
        }
    }

    Ok(result)
}

/// A single step in a variable-length path: the edge traversed and the
/// destination node reached.
#[derive(Debug, Clone)]
pub struct PathStep {
    /// The actual edge source in the database.
    pub edge_src: NodeId,
    /// The actual edge destination in the database.
    pub edge_dst: NodeId,
    /// The edge type/label.
    pub edge_label: String,
    /// The parallel edge sequence number.
    pub edge_seq: u64,
    /// The node reached by this step.
    #[allow(dead_code)]
    pub dst: NodeId,
}

/// Variable-length path traversal returning full paths.
///
/// Returns all paths from `start` following edges with the given `label`(s) and
/// `direction`, with length between `min_hops` and `max_hops` inclusive.
///
/// # Why a custom DFS instead of `petgraph`
/// `petgraph` operates on an in-memory `Graph` value: every node and edge must
/// be loaded into a `Vec`/`HashMap` before any algorithm runs. graphdblite's
/// graph lives in SQLite — adjacency is lazy-loaded per-node via `kv::get` on
/// the `adj_out`/`adj_in` tables, and a single hop only touches the rows it
/// needs. Using `petgraph` would mean materializing the full subgraph
/// reachable from `start` (or the entire database) before traversal, which
/// defeats both the point of an embedded engine and the hop-bounded /
/// fuel-capped pruning that `traverse_paths` performs inline. Cypher
/// var-length patterns also need per-hop property predicates and parallel-edge
/// (`(src,dst,label,seq)`) enumeration; `petgraph` has neither concept and
/// would still leave us with a custom walker on top.
///
/// # Algorithm
/// Uses iterative DFS (explicit stack) with **relationship uniqueness** — each
/// edge may appear at most once per path, but different paths may share edges.
/// This matches Cypher's `uniqueness = RELATIONSHIP_PATH` semantics.
///
/// Each stack frame carries: current node, path so far, and a set of visited
/// edge hashes. Parallel edges (same src/dst/label, different seq) are treated
/// as distinct edges. The visited set uses u64 hashes of `(src, dst, label, seq)`
/// to avoid string allocations in the inner loop.
///
/// # Edge discovery
/// When `labels` is empty, all edge types are followed at each hop (discovered
/// dynamically per node via `get_all_edge_labels`, not just from the start node).
///
/// # Property filters
/// `prop_filters` are applied at each hop inside the DFS — edges that don't
/// match are pruned immediately rather than filtering results after traversal.
///
/// Zero-length paths (min_hops=0) include the start node with empty step list.
///
/// # Fuel cap
/// DFS work is bounded by `max_work` edge visits (0 = unlimited). Without
/// this, dense graphs combined with high hop counts (e.g. K15 with `*1..14`)
/// produce factorial path enumeration that pegs CPU and memory long before
/// `max_results` would stop it. Exceeding the cap returns
/// `GraphError::SizeLimit { what: "variable-length traversal work", .. }`.
#[allow(clippy::too_many_arguments)]
pub fn traverse_paths(
    conn: &Connection,
    start: NodeId,
    labels: &[&str],
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    prop_filters: &HashMap<String, crate::types::Value>,
    max_results: Option<usize>,
    max_work: u64,
) -> Result<Vec<(NodeId, Vec<PathStep>)>> {
    let mut results: Vec<(NodeId, Vec<PathStep>)> = Vec::new();

    // Zero-length match: the start node itself.
    if min_hops == 0 {
        results.push((start, Vec::new()));
        if let Some(cap) = max_results {
            if results.len() >= cap {
                return Ok(results);
            }
        }
    }

    if max_hops == 0 {
        return Ok(results);
    }

    // DFS stack: (current_node, path_so_far, visited_edges)
    // Edge key is the tuple (src, dst, label, seq) — collision-free, unlike a
    // u64 hash digest. The HashSet is small (bounded by hop depth per frame).
    type EdgeKey = (NodeId, NodeId, String, u64);
    type DfsFrame = (NodeId, Vec<PathStep>, std::collections::HashSet<EdgeKey>);
    let mut stack: Vec<DfsFrame> = Vec::new();
    stack.push((start, Vec::new(), std::collections::HashSet::new()));

    // Bound total DFS work. Each iteration of the inner edge-visit loop
    // decrements `fuel`; exhaustion returns SizeLimit so a malicious or
    // pathological pattern can't burn the host. The cap is supplied by the
    // caller (from `Config::max_traversal_work`); 0 disables it.
    let unlimited_work = max_work == 0;
    let mut fuel: u64 = max_work;

    while let Some((current, path, visited_edges)) = stack.pop() {
        let depth = path.len() as u32;
        if depth >= max_hops {
            continue;
        }

        // When no specific labels are given, discover all edge types for the
        // current node so we follow every type at every hop (not just types
        // present on the start node).
        let discovered: Vec<String>;
        let effective_labels: Vec<&str> = if labels.is_empty() {
            let all = get_all_edge_labels(conn, current, direction)?;
            discovered = all.into_iter().map(|(l, _)| l).collect();
            discovered.iter().map(|s| s.as_str()).collect()
        } else {
            discovered = Vec::new();
            let _ = &discovered; // suppress unused warning
            labels.to_vec()
        };

        for label in &effective_labels {
            let neighbors = get_neighbors(conn, current, label, direction)?;
            for neighbor in neighbors {
                // Enumerate all parallel edges between current and neighbor,
                // considering storage direction.
                let mut directed_edges: Vec<(NodeId, NodeId, u64, Properties)> = Vec::new();
                match direction {
                    Direction::Outgoing => {
                        let all = get_all_edge_props(conn, current, neighbor, label)?;
                        for (seq, props) in all {
                            directed_edges.push((current, neighbor, seq, props));
                        }
                    }
                    Direction::Incoming => {
                        let all = get_all_edge_props(conn, neighbor, current, label)?;
                        for (seq, props) in all {
                            directed_edges.push((neighbor, current, seq, props));
                        }
                    }
                    Direction::Both => {
                        let fwd = get_all_edge_props(conn, current, neighbor, label)?;
                        for (seq, props) in fwd {
                            directed_edges.push((current, neighbor, seq, props));
                        }
                        if current != neighbor {
                            let rev = get_all_edge_props(conn, neighbor, current, label)?;
                            for (seq, props) in rev {
                                directed_edges.push((neighbor, current, seq, props));
                            }
                        }
                    }
                }
                // Fallback: edge exists in adjacency but has no props row.
                if directed_edges.is_empty() {
                    let (es, ed) = match direction {
                        Direction::Incoming => (neighbor, current),
                        Direction::Outgoing => (current, neighbor),
                        Direction::Both => {
                            if edge_exists(conn, current, neighbor, label)? {
                                (current, neighbor)
                            } else {
                                (neighbor, current)
                            }
                        }
                    };
                    directed_edges.push((es, ed, 0, Properties::new()));
                }

                for (edge_src, edge_dst, seq, props) in directed_edges {
                    if !unlimited_work {
                        if fuel == 0 {
                            return Err(crate::types::GraphError::SizeLimit {
                                what: "variable-length traversal work".to_string(),
                                limit: max_work as usize,
                                actual: max_work as usize,
                                hint: Some(
                                    "narrow the hop range, add property predicates, LIMIT the result, or raise Config::max_traversal_work"
                                        .to_string(),
                                ),
                            });
                        }
                        fuel -= 1;
                    }
                    let edge_key: EdgeKey = (edge_src, edge_dst, label.to_string(), seq);
                    if visited_edges.contains(&edge_key) {
                        continue; // Relationship uniqueness.
                    }

                    // Apply inline property filters using already-fetched props.
                    if !prop_filters.is_empty() {
                        let mut matches = true;
                        for (key, expected) in prop_filters {
                            if props.get(key) != Some(expected) {
                                matches = false;
                                break;
                            }
                        }
                        if !matches {
                            continue;
                        }
                    }

                    let step = PathStep {
                        edge_src,
                        edge_dst,
                        edge_label: label.to_string(),
                        edge_seq: seq,
                        dst: neighbor,
                    };

                    let new_depth = path.len() as u32 + 1;
                    let want_result = new_depth >= min_hops;
                    let want_continue = new_depth < max_hops;

                    if want_result && want_continue {
                        // Need path for both result and stack — clone once for
                        // result, build extended path for stack.
                        let mut new_path = path.clone();
                        new_path.push(step);
                        results.push((neighbor, new_path.clone()));
                        if let Some(cap) = max_results {
                            if results.len() >= cap {
                                return Ok(results);
                            }
                        }
                        let mut new_visited = visited_edges.clone();
                        new_visited.insert(edge_key);
                        stack.push((neighbor, new_path, new_visited));
                    } else if want_result {
                        // Terminal depth — no need for stack copy.
                        let mut new_path = path.clone();
                        new_path.push(step);
                        results.push((neighbor, new_path));
                        if let Some(cap) = max_results {
                            if results.len() >= cap {
                                return Ok(results);
                            }
                        }
                    } else if want_continue {
                        // Below min_hops — only push to stack.
                        let mut new_path = path.clone();
                        new_path.push(step);
                        let mut new_visited = visited_edges.clone();
                        new_visited.insert(edge_key);
                        stack.push((neighbor, new_path, new_visited));
                    }
                }
            }
        }
    }

    Ok(results)
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
            // Combine both directions, deduplicating labels.
            let mut result = get_all_edge_labels(conn, id, Direction::Outgoing)?;
            let existing: std::collections::HashSet<String> =
                result.iter().map(|(l, _)| l.clone()).collect();
            for (label, ids) in get_all_edge_labels(conn, id, Direction::Incoming)? {
                if !existing.contains(&label) {
                    result.push((label, ids));
                }
            }
            return Ok(result);
        }
    };

    let prefix = id.to_be_bytes();
    let entries = kv::scan_prefix(conn, table, &prefix)?;

    let mut result = Vec::new();
    for (key, data) in entries {
        if key.len() > 8 {
            let label =
                String::from_utf8(key[8..].to_vec()).map_err(|e| GraphError::Serialization {
                    context: String::new(),
                    source: format!("invalid UTF-8 in edge label: {e}"),
                    hint: None,
                })?;
            let ids = decode_id_list(&data);
            result.push((label, ids));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod traverse_paths_tests {
    //! Unit tests for `traverse_paths` exercised against the storage layer
    //! directly — no Cypher parser/planner/executor in the loop. The
    //! executor calls `traverse_paths` from three sites
    //! (`exec_correlated`, `exec_expand`, `ExpandIter`); these tests pin
    //! the algorithm's behaviour independent of any of them.
    use super::*;
    use crate::storage::node;
    use crate::types::Value;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        crate::schema::init_schema(&conn).expect("init schema");
        conn
    }

    fn mk_node(conn: &Connection, label: &str) -> NodeId {
        node::create_node(conn, &[label.to_string()], Properties::new()).expect("create node")
    }

    fn mk_edge(conn: &Connection, src: NodeId, dst: NodeId, label: &str) {
        create_edge(conn, src, dst, label, Properties::new()).expect("create edge");
    }

    #[test]
    fn linear_chain_returns_paths_at_each_depth() {
        // a -KNOWS-> b -KNOWS-> c -KNOWS-> d
        let conn = fresh_conn();
        let a = mk_node(&conn, "P");
        let b = mk_node(&conn, "P");
        let c = mk_node(&conn, "P");
        let d = mk_node(&conn, "P");
        for (s, t) in [(a, b), (b, c), (c, d)] {
            mk_edge(&conn, s, t, "KNOWS");
        }

        let paths = traverse_paths(
            &conn,
            a,
            &["KNOWS"],
            Direction::Outgoing,
            1,
            3,
            &HashMap::new(),
            None,
            0,
        )
        .expect("traverse");

        // Expect b at depth 1, c at depth 2, d at depth 3.
        let mut by_dst: HashMap<NodeId, usize> = paths.iter().map(|(n, p)| (*n, p.len())).collect();
        assert_eq!(by_dst.remove(&b), Some(1));
        assert_eq!(by_dst.remove(&c), Some(2));
        assert_eq!(by_dst.remove(&d), Some(3));
        assert!(by_dst.is_empty(), "unexpected extra paths: {by_dst:?}");
    }

    #[test]
    fn relationship_uniqueness_breaks_two_node_cycle() {
        // a -R-> b and b -R-> a. With max_hops large, DFS would loop
        // forever without per-edge visited tracking.
        let conn = fresh_conn();
        let a = mk_node(&conn, "P");
        let b = mk_node(&conn, "P");
        mk_edge(&conn, a, b, "R");
        mk_edge(&conn, b, a, "R");

        let paths = traverse_paths(
            &conn,
            a,
            &["R"],
            Direction::Outgoing,
            1,
            10,
            &HashMap::new(),
            None,
            0,
        )
        .expect("traverse");

        // Outgoing-only: a->b (depth 1), then b->a using the second edge
        // (depth 2). Both edges consumed, no further hops possible.
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().any(|(n, p)| *n == b && p.len() == 1));
        assert!(paths.iter().any(|(n, p)| *n == a && p.len() == 2));
    }

    #[test]
    fn parallel_edges_yield_distinct_paths() {
        // Two edges a-R->b with seq=0 and seq=1 must be enumerated as
        // independent 1-hop paths (parallel-edge support).
        let conn = fresh_conn();
        let a = mk_node(&conn, "P");
        let b = mk_node(&conn, "P");
        mk_edge(&conn, a, b, "R");
        mk_edge(&conn, a, b, "R");

        let paths = traverse_paths(
            &conn,
            a,
            &["R"],
            Direction::Outgoing,
            1,
            1,
            &HashMap::new(),
            None,
            0,
        )
        .expect("traverse");

        assert_eq!(paths.len(), 2, "parallel edges should produce 2 paths");
        let mut seqs: Vec<u64> = paths.iter().map(|(_, p)| p[0].edge_seq).collect();
        seqs.sort_unstable();
        assert_eq!(seqs, vec![0, 1]);
    }

    #[test]
    fn prop_filter_prunes_edges_at_each_hop() {
        // a -R{w:1}-> b, a -R{w:2}-> c. Filter w=1 keeps only a->b.
        let conn = fresh_conn();
        let a = mk_node(&conn, "P");
        let b = mk_node(&conn, "P");
        let c = mk_node(&conn, "P");
        let mut p1 = Properties::new();
        p1.insert("w".into(), Value::I64(1));
        let mut p2 = Properties::new();
        p2.insert("w".into(), Value::I64(2));
        create_edge(&conn, a, b, "R", p1).expect("edge1");
        create_edge(&conn, a, c, "R", p2).expect("edge2");

        let mut filter = HashMap::new();
        filter.insert("w".to_string(), Value::I64(1));

        let paths = traverse_paths(
            &conn,
            a,
            &["R"],
            Direction::Outgoing,
            1,
            1,
            &filter,
            None,
            0,
        )
        .expect("traverse");

        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].0, b);
    }

    #[test]
    fn min_hops_zero_includes_start_with_empty_path() {
        let conn = fresh_conn();
        let a = mk_node(&conn, "P");
        let b = mk_node(&conn, "P");
        mk_edge(&conn, a, b, "R");

        let paths = traverse_paths(
            &conn,
            a,
            &["R"],
            Direction::Outgoing,
            0,
            1,
            &HashMap::new(),
            None,
            0,
        )
        .expect("traverse");

        // Zero-length match plus the 1-hop result.
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().any(|(n, p)| *n == a && p.is_empty()));
        assert!(paths.iter().any(|(n, p)| *n == b && p.len() == 1));
    }

    #[test]
    fn max_hops_zero_returns_only_zero_length_when_min_is_zero() {
        let conn = fresh_conn();
        let a = mk_node(&conn, "P");
        let b = mk_node(&conn, "P");
        mk_edge(&conn, a, b, "R");

        let paths = traverse_paths(
            &conn,
            a,
            &["R"],
            Direction::Outgoing,
            0,
            0,
            &HashMap::new(),
            None,
            0,
        )
        .expect("traverse");

        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].0, a);
        assert!(paths[0].1.is_empty());
    }

    #[test]
    fn direction_both_follows_incoming_and_outgoing() {
        // a -R-> b and c -R-> a. Direction::Both from `a` should reach
        // both b (outgoing) and c (incoming) at depth 1.
        let conn = fresh_conn();
        let a = mk_node(&conn, "P");
        let b = mk_node(&conn, "P");
        let c = mk_node(&conn, "P");
        mk_edge(&conn, a, b, "R");
        mk_edge(&conn, c, a, "R");

        let paths = traverse_paths(
            &conn,
            a,
            &["R"],
            Direction::Both,
            1,
            1,
            &HashMap::new(),
            None,
            0,
        )
        .expect("traverse");

        let mut dsts: Vec<NodeId> = paths.iter().map(|(n, _)| *n).collect();
        dsts.sort_by_key(|n| n.0);
        let mut expected = vec![b, c];
        expected.sort_by_key(|n| n.0);
        assert_eq!(dsts, expected);
    }

    #[test]
    fn fuel_cap_returns_size_limit_error() {
        // Tiny budget against any non-trivial graph triggers the cap.
        let conn = fresh_conn();
        let a = mk_node(&conn, "P");
        let b = mk_node(&conn, "P");
        let c = mk_node(&conn, "P");
        mk_edge(&conn, a, b, "R");
        mk_edge(&conn, b, c, "R");

        let err = traverse_paths(
            &conn,
            a,
            &["R"],
            Direction::Outgoing,
            1,
            10,
            &HashMap::new(),
            None,
            1, // 1 edge visit allowed; will exhaust on the second hop
        )
        .expect_err("expected SizeLimit");
        match err {
            GraphError::SizeLimit { what, .. } => {
                assert!(what.contains("variable-length traversal work"));
            }
            other => panic!("expected SizeLimit, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::NodeId;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn create_edge_increments_edge_type_counter() {
        use crate::stats::get_edge_type_count;
        let conn = fresh_conn();
        let a = crate::storage::node::create_node(&conn, &[], Default::default()).unwrap();
        let b = crate::storage::node::create_node(&conn, &[], Default::default()).unwrap();
        super::create_edge(&conn, a, b, "KNOWS", Default::default()).unwrap();
        super::create_edge(&conn, a, b, "KNOWS", Default::default()).unwrap();
        super::create_edge(&conn, a, b, "KNOWS", Default::default()).unwrap();
        assert_eq!(get_edge_type_count(&conn, "KNOWS").unwrap(), 3);
    }

    #[test]
    fn delete_single_edge_decrements_counter() {
        use crate::stats::get_edge_type_count;
        let conn = fresh_conn();
        let a = crate::storage::node::create_node(&conn, &[], Default::default()).unwrap();
        let b = crate::storage::node::create_node(&conn, &[], Default::default()).unwrap();
        super::create_edge(&conn, a, b, "KNOWS", Default::default()).unwrap();
        super::create_edge(&conn, a, b, "KNOWS", Default::default()).unwrap();
        let all = super::get_all_edge_props(&conn, a, b, "KNOWS").unwrap();
        let (seq, _) = all.first().expect("at least one parallel edge");
        super::delete_single_edge(&conn, a, b, "KNOWS", *seq).unwrap();
        assert_eq!(get_edge_type_count(&conn, "KNOWS").unwrap(), 1);
    }

    #[test]
    fn delete_edge_decrements_by_parallel_count() {
        use crate::stats::get_edge_type_count;
        let conn = fresh_conn();
        let a = crate::storage::node::create_node(&conn, &[], Default::default()).unwrap();
        let b = crate::storage::node::create_node(&conn, &[], Default::default()).unwrap();
        super::create_edge(&conn, a, b, "KNOWS", Default::default()).unwrap();
        super::create_edge(&conn, a, b, "KNOWS", Default::default()).unwrap();
        super::create_edge(&conn, a, b, "KNOWS", Default::default()).unwrap();
        super::delete_edge(&conn, a, b, "KNOWS").unwrap();
        assert_eq!(get_edge_type_count(&conn, "KNOWS").unwrap(), 0);
    }

    #[test]
    fn label_from_edge_props_key_round_trips() {
        let key = super::edge_props_key(NodeId(1), NodeId(2), "KNOWS", 7);
        assert_eq!(super::label_from_edge_props_key(&key), Some("KNOWS"));
    }

    #[test]
    fn label_from_edge_props_key_rejects_short_key() {
        assert_eq!(super::label_from_edge_props_key(&[0u8; 8]), None);
    }

    #[test]
    fn label_from_edge_props_key_rejects_missing_nul() {
        let mut key = Vec::new();
        key.extend_from_slice(&1u64.to_be_bytes());
        key.extend_from_slice(&2u64.to_be_bytes());
        key.extend_from_slice(b"KNOWS");
        // no 0x00 terminator
        assert_eq!(super::label_from_edge_props_key(&key), None);
    }
}
