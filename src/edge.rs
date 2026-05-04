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
fn edge_props_prefix(src: NodeId, dst: NodeId, label: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(16 + label.len() + 1);
    key.extend_from_slice(&src.to_be_bytes());
    key.extend_from_slice(&dst.to_be_bytes());
    key.extend_from_slice(label.as_bytes());
    key.push(0x00);
    key
}

/// Extract the sequence number from an edge_props key.
fn edge_seq_from_key(key: &[u8], prefix_len: usize) -> u64 {
    if key.len() >= prefix_len + 8 {
        let bytes: [u8; 8] = key[prefix_len..prefix_len + 8].try_into().unwrap_or([0; 8]);
        u64::from_be_bytes(bytes)
    } else {
        0
    }
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

    Ok(())
}

/// Create multiple edges in batch, coalescing adjacency blob updates.
///
/// All edges share the same label. Each edge is (src, dst, properties).
/// Adjacency blobs are grouped by node so each blob is read and written once,
/// regardless of how many edges touch the same node.
#[allow(dead_code)]
pub fn batch_create_edges(
    conn: &Connection,
    label: &str,
    edges: &[(NodeId, NodeId, Properties)],
) -> Result<()> {
    validate_name(label)?;
    for (_, _, properties) in edges {
        for key in properties.keys() {
            validate_name(key)?;
        }
    }

    // Group outgoing: src → [dst, dst, ...]
    let mut out_groups: HashMap<u64, Vec<u64>> = HashMap::new();
    // Group incoming: dst → [src, src, ...]
    let mut in_groups: HashMap<u64, Vec<u64>> = HashMap::new();

    for (src, dst, _) in edges {
        out_groups.entry(src.0).or_default().push(dst.0);
        in_groups.entry(dst.0).or_default().push(src.0);
    }

    // Coalesced outgoing adjacency updates.
    for (src_raw, new_dsts) in &out_groups {
        let out_key = adj_key(NodeId(*src_raw), label);
        let mut ids = match kv::get(conn, kv::TABLE_ADJ_OUT, &out_key)? {
            Some(data) => decode_id_list(&data),
            None => Vec::new(),
        };
        for &dst in new_dsts {
            insert_into_sorted(&mut ids, dst);
        }
        kv::put(conn, kv::TABLE_ADJ_OUT, &out_key, &encode_id_list(&ids))?;
    }

    // Coalesced incoming adjacency updates.
    for (dst_raw, new_srcs) in &in_groups {
        let in_key = adj_key(NodeId(*dst_raw), label);
        let mut ids = match kv::get(conn, kv::TABLE_ADJ_IN, &in_key)? {
            Some(data) => decode_id_list(&data),
            None => Vec::new(),
        };
        for &src in new_srcs {
            insert_into_sorted(&mut ids, src);
        }
        kv::put(conn, kv::TABLE_ADJ_IN, &in_key, &encode_id_list(&ids))?;
    }

    // Always store edge_props rows so relationship counting works correctly.
    // Use global monotonic edge sequence so keys are never recycled.
    for (src, dst, properties) in edges {
        let seq = crate::id::next_edge_seq(conn)?;
        let props_key = edge_props_key(*src, *dst, label, seq);
        let data = rmp_serde::to_vec(properties).map_err(|e| GraphError::Serialization {
            context: String::new(),
            source: e.to_string(),
            hint: None,
        })?;
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

    // Delete all parallel edge properties for this (src, dst, label).
    let prefix = edge_props_prefix(src, dst, label);
    let entries = kv::scan_prefix(conn, kv::TABLE_EDGE_PROPS, &prefix)?;
    for (key, _) in entries {
        kv::delete(conn, kv::TABLE_EDGE_PROPS, &key)?;
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
    kv::delete(conn, kv::TABLE_EDGE_PROPS, &props_key)?;

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
mod tests {
    use super::*;
    use crate::types::Value;
    use crate::Database;

    fn props(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn batch_create_edges_basic() {
        let mut db = Database::open_memory().unwrap();
        let tx = db.write_tx().unwrap();

        let a = tx
            .create_node("Node", props(&[("name", Value::String("A".into()))]))
            .unwrap();
        let b = tx
            .create_node("Node", props(&[("name", Value::String("B".into()))]))
            .unwrap();
        let c = tx
            .create_node("Node", props(&[("name", Value::String("C".into()))]))
            .unwrap();

        batch_create_edges(
            tx.connection(),
            "KNOWS",
            &[(a, b, HashMap::new()), (a, c, HashMap::new())],
        )
        .unwrap();

        let neighbors = tx.get_neighbors(a, "KNOWS", Direction::Outgoing).unwrap();
        assert_eq!(neighbors.len(), 2);
        assert!(neighbors.contains(&b));
        assert!(neighbors.contains(&c));

        let incoming_b = tx.get_neighbors(b, "KNOWS", Direction::Incoming).unwrap();
        assert!(incoming_b.contains(&a));

        tx.commit().unwrap();
    }

    #[test]
    fn batch_create_edges_with_properties() {
        let mut db = Database::open_memory().unwrap();
        let tx = db.write_tx().unwrap();

        let a = tx.create_node("Node", HashMap::new()).unwrap();
        let b = tx.create_node("Node", HashMap::new()).unwrap();

        batch_create_edges(
            tx.connection(),
            "CALLS",
            &[(a, b, props(&[("line", Value::I64(42))]))],
        )
        .unwrap();

        let edge_props = tx.get_edge_properties(a, b, "CALLS").unwrap();
        assert_eq!(edge_props.get("line"), Some(&Value::I64(42)));

        tx.commit().unwrap();
    }

    #[test]
    fn batch_create_edges_coalescing_many_from_same_source() {
        let mut db = Database::open_memory().unwrap();
        let tx = db.write_tx().unwrap();

        let src = tx.create_node("Node", HashMap::new()).unwrap();
        let mut targets = Vec::new();
        for _ in 0..50 {
            targets.push(tx.create_node("Node", HashMap::new()).unwrap());
        }

        let edges: Vec<_> = targets.iter().map(|&t| (src, t, HashMap::new())).collect();
        batch_create_edges(tx.connection(), "E", &edges).unwrap();

        let neighbors = tx.get_neighbors(src, "E", Direction::Outgoing).unwrap();
        assert_eq!(neighbors.len(), 50);
        for t in &targets {
            assert!(neighbors.contains(t));
        }

        tx.commit().unwrap();
    }

    #[test]
    fn batch_create_edges_matches_individual_create() {
        let mut db = Database::open_memory().unwrap();

        let tx = db.write_tx().unwrap();
        let a = tx.create_node("Node", HashMap::new()).unwrap();
        let b = tx.create_node("Node", HashMap::new()).unwrap();
        let c = tx.create_node("Node", HashMap::new()).unwrap();
        tx.commit().unwrap();

        let tx = db.write_tx().unwrap();
        batch_create_edges(
            tx.connection(),
            "E",
            &[
                (a, b, props(&[("w", Value::I64(1))])),
                (a, c, props(&[("w", Value::I64(2))])),
                (b, c, HashMap::new()),
            ],
        )
        .unwrap();

        let a_out = tx.get_neighbors(a, "E", Direction::Outgoing).unwrap();
        assert_eq!(a_out.len(), 2);
        let b_out = tx.get_neighbors(b, "E", Direction::Outgoing).unwrap();
        assert_eq!(b_out.len(), 1);
        let c_in = tx.get_neighbors(c, "E", Direction::Incoming).unwrap();
        assert_eq!(c_in.len(), 2);

        let props_ab = tx.get_edge_properties(a, b, "E").unwrap();
        assert_eq!(props_ab.get("w"), Some(&Value::I64(1)));
        let props_ac = tx.get_edge_properties(a, c, "E").unwrap();
        assert_eq!(props_ac.get("w"), Some(&Value::I64(2)));

        tx.commit().unwrap();
    }
}
