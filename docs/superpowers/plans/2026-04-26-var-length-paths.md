# Variable-Length Path Execution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix variable-length path execution so that relationship variables bind to edge lists, paths materialize correctly, and all 15+ skiplisted TCK scenarios pass.

**Architecture:** Grammar/parser/planner already work. Changes are concentrated in the execution layer: `ExpandIter` (iterator path), `exec_correlated` (correlated path), `exec_materialize_path` (path construction), and `build_compound_binding` (projection). The `traverse_paths()` function in `edge.rs` already returns full edge sequences — we just need to use it everywhere.

**Tech Stack:** Rust, SQLite (via rusqlite), openCypher TCK (Gherkin)

---

### Task 1: Fix `ExpandIter` to use `traverse_paths()` for var-length

The iterator-based execution path (`ExpandIter` in `iter.rs`) currently calls `edge::traverse()` for multi-hop, which returns only node IDs. It needs to call `traverse_paths()` and bind the rel_alias to a `Value::List` of `Value::Edge`.

**Files:**
- Modify: `src/cypher/iter.rs:244-393` (ExpandIter)

- [ ] **Step 1: Add `var_length` field to `ExpandIter`**

The `ExpandIter` struct currently has `min_hops` and `max_hops` but no `var_length` flag. Add it and wire it through from `build_iter`.

In `src/cypher/iter.rs`, add field to `ExpandIter`:

```rust
pub struct ExpandIter<'a> {
    input: Box<dyn RecordIter + 'a>,
    conn: &'a Connection,
    src_alias: String,
    dst_alias: String,
    rel_alias: Option<String>,
    edge_types: Vec<String>,
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    var_length: bool,  // NEW
    buffer: std::vec::IntoIter<Record>,
}
```

In `build_iter`, the `LogicalOp::Expand` match arm currently uses `..` to ignore `var_length`. Change it to capture and pass it:

```rust
LogicalOp::Expand {
    input,
    src_alias,
    dst_alias,
    rel_alias,
    edge_types,
    direction,
    min_hops,
    max_hops,
    var_length,  // was: ..
} => {
    let input_iter = build_iter(conn, input)?;
    Ok(Box::new(ExpandIter {
        input: input_iter,
        conn,
        src_alias: src_alias.clone(),
        dst_alias: dst_alias.clone(),
        rel_alias: rel_alias.clone(),
        edge_types: edge_types.clone(),
        direction: *direction,
        min_hops: *min_hops,
        max_hops: *max_hops,
        var_length: *var_length,  // NEW
        buffer: Vec::new().into_iter(),
    }))
}
```

- [ ] **Step 2: Implement var-length branch in `ExpandIter::next_record`**

Replace the current multi-hop logic in `ExpandIter::next_record`. The current code (lines 278-289) calls `edge::traverse()` for non-1..1 hops. Replace the entire neighbor-fetching and record-building section with var-length-aware logic.

The key change: when `self.var_length` is true, call `traverse_paths()` and for each path result, bind the rel_alias to `Value::List` of `Value::Edge`, bind the dst_alias as a node. When `self.var_length` is false, keep the existing single-hop logic.

Replace the body of `ExpandIter::next_record` (the inner loop after pulling the input record) with:

```rust
impl<'a> RecordIter for ExpandIter<'a> {
    fn next_record(&mut self) -> Result<Option<Record>> {
        loop {
            if let Some(rec) = self.buffer.next() {
                return Ok(Some(rec));
            }

            let rec = match self.input.next_record()? {
                Some(r) => r,
                None => return Ok(None),
            };

            let src_id = match rec.get(&self.src_alias) {
                Some(Value::I64(id)) => NodeId(*id as u64),
                _ => continue,
            };

            let bound_dst = rec.get(&self.dst_alias).and_then(|v| match v {
                Value::I64(id) => Some(NodeId(*id as u64)),
                _ => None,
            });

            if self.var_length {
                // Variable-length: discover labels if needed, then traverse_paths.
                let _owned_labels: Vec<String>;
                let labels: Vec<&str> = if self.edge_types.is_empty() {
                    let all = edge::get_all_edge_labels(self.conn, src_id, self.direction)?;
                    _owned_labels = all.into_iter().map(|(l, _)| l).collect();
                    _owned_labels.iter().map(|s| s.as_str()).collect()
                } else {
                    self.edge_types.iter().map(|s| s.as_str()).collect()
                };

                let paths = edge::traverse_paths(
                    self.conn, src_id, &labels, self.direction,
                    self.min_hops, self.max_hops,
                )?;

                let mut expanded = Vec::new();
                for (dst_id, steps) in paths {
                    if let Some(required) = bound_dst {
                        if dst_id != required {
                            continue;
                        }
                    }
                    let dst_node = node::get_node(self.conn, dst_id)?;
                    let mut new_rec = rec.clone();
                    new_rec.set(self.dst_alias.clone(), Value::I64(dst_id.0 as i64));
                    for (key, val) in &dst_node.properties {
                        new_rec.set(format!("{}.{key}", self.dst_alias), val.clone());
                    }
                    new_rec.set(
                        format!("{}.__label", self.dst_alias),
                        Value::String(dst_node.labels.join(":")),
                    );
                    new_rec.set(
                        format!("{}.__labels", self.dst_alias),
                        Value::List(
                            dst_node.labels.iter().map(|l| Value::String(l.clone())).collect(),
                        ),
                    );
                    new_rec.set(
                        format!("{}.__id", self.dst_alias),
                        Value::I64(dst_id.0 as i64),
                    );
                    if let Some(ref r_alias) = self.rel_alias {
                        let edge_list: Vec<Value> = steps
                            .iter()
                            .map(|step| {
                                let props = edge::get_edge_properties(
                                    self.conn, step.edge_src, step.edge_dst, &step.edge_label,
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
                        new_rec.set(r_alias.clone(), Value::List(edge_list));
                    }
                    expanded.push(new_rec);
                }
                self.buffer = expanded.into_iter();
            } else {
                // Single-hop: existing logic unchanged.
                let label = self.edge_types.first().map(|s| s.as_str()).unwrap_or("");
                let neighbors = edge::get_neighbors(self.conn, src_id, label, self.direction)?;

                let mut expanded = Vec::with_capacity(neighbors.len());
                for dst_id in neighbors {
                    if let Some(required) = bound_dst {
                        if dst_id != required {
                            continue;
                        }
                    }
                    let dst_node = node::get_node(self.conn, dst_id)?;
                    let mut new_rec = rec.clone();
                    new_rec.set(self.dst_alias.clone(), Value::I64(dst_id.0 as i64));
                    for (key, val) in &dst_node.properties {
                        new_rec.set(format!("{}.{key}", self.dst_alias), val.clone());
                    }
                    new_rec.set(
                        format!("{}.__label", self.dst_alias),
                        Value::String(dst_node.labels.join(":")),
                    );
                    new_rec.set(
                        format!("{}.__labels", self.dst_alias),
                        Value::List(
                            dst_node.labels.iter().map(|l| Value::String(l.clone())).collect(),
                        ),
                    );
                    new_rec.set(
                        format!("{}.__id", self.dst_alias),
                        Value::I64(dst_id.0 as i64),
                    );
                    if let Some(ref r_alias) = self.rel_alias {
                        let (edge_src, edge_dst) = match self.direction {
                            Direction::Incoming => (dst_id, src_id),
                            Direction::Outgoing => (src_id, dst_id),
                            Direction::Both => {
                                if edge::edge_exists(self.conn, src_id, dst_id, label)
                                    .unwrap_or(false)
                                {
                                    (src_id, dst_id)
                                } else {
                                    (dst_id, src_id)
                                }
                            }
                        };

                        // Relationship uniqueness check (same as current code).
                        let (es, ed) = (edge_src.0 as i64, edge_dst.0 as i64);
                        let ek = (es.min(ed), es.max(ed), label);
                        let mut dup = false;
                        for (key, _) in &new_rec.fields {
                            if key.ends_with(".__src") && !key.starts_with(&format!("{r_alias}.")) {
                                let oa = &key[..key.len() - 6];
                                if let (
                                    Some(Value::I64(os)),
                                    Some(Value::I64(od)),
                                    Some(Value::String(ot)),
                                ) = (
                                    new_rec.get(key),
                                    new_rec.get(&format!("{oa}.__dst")),
                                    new_rec.get(&format!("{oa}.__type")),
                                ) {
                                    let ok = ((*os).min(*od), (*os).max(*od), ot.as_str());
                                    if ok == ek {
                                        dup = true;
                                        break;
                                    }
                                }
                            }
                        }
                        if dup {
                            continue;
                        }

                        new_rec.set(format!("{r_alias}.__src"), Value::I64(edge_src.0 as i64));
                        new_rec.set(format!("{r_alias}.__dst"), Value::I64(edge_dst.0 as i64));
                        new_rec.set(
                            format!("{r_alias}.__type"),
                            Value::String(label.to_string()),
                        );
                        if let Ok(props) =
                            edge::get_edge_properties(self.conn, edge_src, edge_dst, label)
                        {
                            for (key, val) in &props {
                                new_rec.set(format!("{r_alias}.{key}"), val.clone());
                            }
                        }
                    }
                    expanded.push(new_rec);
                }
                self.buffer = expanded.into_iter();
            }
        }
    }
}
```

- [ ] **Step 3: Run basic TCK to verify no regression**

Run: `cargo test --test tck 2>&1 | tail -5`
Expected: Same 3600 passed, 0 failures as before (since read-only var-length queries now go through the iterator path but should produce the same results as exec_expand).

- [ ] **Step 4: Commit**

```bash
git add src/cypher/iter.rs
git commit -m "feat: ExpandIter uses traverse_paths for var-length patterns"
```

---

### Task 2: Fix `exec_correlated` Expand to use `traverse_paths()` for var-length

The correlated execution path (`exec_correlated` in `executor.rs`) handles Expand but uses `traverse()` for multi-hop cases. It needs the same `traverse_paths()` treatment as `exec_expand` already has.

**Files:**
- Modify: `src/cypher/executor.rs:2562-2735` (exec_correlated Expand arm)

- [ ] **Step 1: Replace traverse() with traverse_paths() in exec_correlated Expand**

In `exec_correlated`, the `LogicalOp::Expand` match arm currently ignores `var_length` (line 2571: `var_length: _`). When var_length is true, it should call `traverse_paths()` and bind the rel_alias to an edge list.

Change `var_length: _` to `var_length` and add a var-length branch inside the label loop (after the `bound_rel` handling, lines 2665-2730):

```rust
LogicalOp::Expand {
    input,
    src_alias,
    dst_alias,
    rel_alias,
    edge_types,
    direction,
    min_hops,
    max_hops,
    var_length,  // was: var_length: _
} => {
    // ... existing input_records and labels logic stays the same ...
    // ... existing bound_rel handling stays the same ...

    for &label in &labels {
        if *var_length {
            // Variable-length: use traverse_paths for full edge sequences.
            let label_refs: Vec<&str> = vec![label];
            let paths = edge::traverse_paths(
                conn, src_id, &label_refs, *direction, *min_hops, *max_hops,
            )?;
            for (dst_id, steps) in paths {
                if let Some(expected) = bound_dst {
                    if dst_id != expected {
                        continue;
                    }
                }
                let dst_node = node::get_node(conn, dst_id)?;
                let mut new_rec = rec.clone();
                new_rec.set(dst_alias.to_string(), Value::I64(dst_id.0 as i64));
                for (key, val) in &dst_node.properties {
                    new_rec.set(format!("{dst_alias}.{key}"), val.clone());
                }
                new_rec.set(
                    format!("{dst_alias}.__label"),
                    Value::String(dst_node.labels.join(":")),
                );
                new_rec.set(
                    format!("{dst_alias}.__labels"),
                    Value::List(
                        dst_node.labels.iter().map(|l| Value::String(l.clone())).collect(),
                    ),
                );
                new_rec.set(format!("{dst_alias}.__id"), Value::I64(dst_id.0 as i64));
                if let Some(r_alias) = rel_alias {
                    let edge_list: Vec<Value> = steps
                        .iter()
                        .map(|step| {
                            let props = edge::get_edge_properties(
                                conn, step.edge_src, step.edge_dst, &step.edge_label,
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
        } else {
            // Single-hop: existing get_neighbors logic.
            let dst_ids = edge::get_neighbors(conn, src_id, label, *direction)?;
            // ... rest of existing single-hop code unchanged ...
        }
    }
}
```

- [ ] **Step 2: Run TCK to verify no regression**

Run: `cargo test --test tck 2>&1 | tail -5`
Expected: 3600 passed, 0 failures.

- [ ] **Step 3: Commit**

```bash
git add src/cypher/executor.rs
git commit -m "feat: exec_correlated Expand uses traverse_paths for var-length"
```

---

### Task 3: Fix `exec_materialize_path` to handle var-length edge lists

`exec_materialize_path` (and the correlated copy) calls `build_compound_binding()` on rel_aliases, which returns `Value::Edge` — but for var-length patterns, the rel_alias holds a `Value::List` of edges. The materializer needs to expand these lists into the path's node-edge sequence, including intermediate nodes.

**Files:**
- Modify: `src/cypher/executor.rs:2348-2400` (exec_materialize_path)
- Modify: `src/cypher/executor.rs:2879-2925` (correlated MaterializePath)

- [ ] **Step 1: Update `exec_materialize_path` to handle edge lists**

Replace the rel_aliases loop in `exec_materialize_path` (lines 2376-2385) to handle both single edges and edge lists:

```rust
let mut edges = Vec::new();
for alias in rel_aliases {
    if rec.get(alias) == Some(&Value::Null) {
        has_null = true;
        break;
    }
    // Check if this rel_alias is a var-length list of edges.
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
```

Also update the node construction. For var-length paths, intermediate nodes aren't in `node_aliases` — they must be inferred from the edge sequence. Replace the path construction section (lines 2387-2395):

```rust
let mut new_rec = rec;
if has_null {
    new_rec.set(path_alias.to_string(), Value::Null);
} else if !nodes.is_empty() || !edges.is_empty() {
    // For var-length paths, intermediate nodes may not be in node_aliases.
    // Rebuild the full node list from the edge sequence.
    let mut full_nodes = Vec::new();
    if let Some(first_node) = nodes.first() {
        full_nodes.push(first_node.clone());
    }
    if edges.len() + 1 > nodes.len() {
        // Var-length: fill intermediate nodes from edge endpoints.
        for edge in &edges {
            let next_id = edge.dst;
            let n = node::get_node(conn, next_id)?;
            full_nodes.push(n);
        }
        // Replace first/last with actual node_aliases nodes if available.
        if nodes.len() >= 2 {
            let last = nodes.last().unwrap().clone();
            *full_nodes.last_mut().unwrap() = last;
        }
    } else {
        // Fixed-length: use node_aliases directly.
        full_nodes = nodes;
    }
    new_rec.set(
        path_alias.to_string(),
        Value::Path(PathValue { nodes: full_nodes, edges }),
    );
}
results.push(new_rec);
```

- [ ] **Step 2: Apply the same fix to the correlated MaterializePath**

The correlated copy at lines 2879-2925 needs identical changes. Replace its rel_aliases loop and path construction with the same logic.

- [ ] **Step 3: Run TCK to check for improvements**

Run: `cargo test --test tck 2>&1 | tail -5`
Expected: Should see some newly passing scenarios (Match6[16,17,19,20] which test path materialization with var-length).

- [ ] **Step 4: Commit**

```bash
git add src/cypher/executor.rs
git commit -m "feat: MaterializePath handles var-length edge lists and intermediate nodes"
```

---

### Task 4: Fix `build_compound_binding` for var-length rel variables

When `RETURN r` is used and `r` is a var-length relationship variable, the record holds `r → Value::List(...)` directly. `build_compound_binding()` should return it as-is rather than trying to reconstruct a single edge from `r.__src`/`r.__dst`.

**Files:**
- Modify: `src/cypher/executor.rs:620-676` (build_compound_binding)

- [ ] **Step 1: Check for direct Value::List binding first**

Add a check at the top of `build_compound_binding` (before the edge/node detection):

```rust
pub(crate) fn build_compound_binding(rec: &Record, var: &str) -> Option<Value> {
    use crate::types::{Edge, Node, Properties};

    // Var-length relationship variables are stored directly as Value::List.
    if let Some(val @ Value::List(_)) = rec.get(var) {
        return Some(val.clone());
    }

    // Also handle Value::Path directly (from MaterializePath).
    if let Some(val @ Value::Path(_)) = rec.get(var) {
        return Some(val.clone());
    }

    let prefix = format!("{var}.");
    // ... rest unchanged ...
}
```

- [ ] **Step 2: Run TCK to check for improvements**

Run: `cargo test --test tck 2>&1 | tail -5`
Expected: Match4[1,2,3,6] and Match9[1,2,3] scenarios that test `RETURN r` where r is var-length should now work.

- [ ] **Step 3: Commit**

```bash
git add src/cypher/executor.rs
git commit -m "feat: build_compound_binding returns var-length edge lists directly"
```

---

### Task 5: Handle `traverse_paths` with empty edge_types (any type)

Match7[12] uses `OPTIONAL MATCH (a)-[*]->(b)` — no edge type specified. `traverse_paths()` currently takes `labels: &[&str]` and iterates them. When empty, it does nothing. We need to discover all labels first and pass them in, similar to how `exec_expand` already does.

**Files:**
- Modify: `src/cypher/executor.rs:508-512` (exec_expand var-length branch)

- [ ] **Step 1: Move label discovery before traverse_paths in exec_expand**

The `exec_expand` var-length branch at lines 508-512 currently creates `label_refs` from just the single `label` variable. But `label` comes from `labels` which was already discovered if empty. The issue is that the var-length branch is inside the `for &label in &labels` loop, so it calls `traverse_paths` once per label with a single label. This is correct but we need to ensure the **outer** label discovery (lines 396-403) also runs for var-length.

Check: The current code already discovers labels when `edge_types.is_empty()` at line 397-403. For var-length, the inner loop at line 510 wraps the single label in a vec. This should work correctly. The potential issue is that `traverse_paths` is called per-label, but each call only searches that one type. For any-type patterns, it will be called once per discovered label, producing paths for each type separately.

This is actually correct behavior — but `traverse_paths` should be called once with ALL labels to allow mixed-type paths. Fix by calling `traverse_paths` **outside** the label loop when var_length is true:

```rust
if var_length {
    // Variable-length traversal: pass all labels at once to allow mixed-type paths.
    let paths = edge::traverse_paths(
        conn, src_id, &labels, direction, min_hops, max_hops,
    )?;
    for (dst_id, steps) in paths {
        if let Some(required) = bound_dst_id {
            if dst_id != required {
                continue;
            }
        }
        // ... existing dst_node + edge_list binding code ...
    }
} else {
    for &label in &labels {
        // ... existing single-hop code ...
    }
}
```

Move the var-length block **out** of the `for &label in &labels` loop.

- [ ] **Step 2: Apply same fix to exec_correlated and ExpandIter**

In `exec_correlated` Expand (from Task 2) and `ExpandIter` (from Task 1), ensure the var-length branch calls `traverse_paths` with all labels at once, outside the per-label loop.

- [ ] **Step 3: Run TCK to check Match7 scenarios**

Run: `cargo test --test tck -- Match7 2>&1 | grep -E "passed|failed|scenario"`
Expected: Match7[12] (var-length OPTIONAL MATCH with any type) should now pass or be closer.

- [ ] **Step 4: Commit**

```bash
git add src/cypher/executor.rs src/cypher/iter.rs
git commit -m "feat: var-length traversal passes all labels for mixed-type paths"
```

---

### Task 6: Fix `LeftOuterJoin` null-filling for var-length rel aliases

Match7[12,14] and Match9[9] use `OPTIONAL MATCH` with var-length patterns. When the optional match fails, the rel_alias should be `null`, not an empty list. The `LeftOuterJoin` null-filling logic needs to handle var-length aliases.

**Files:**
- Modify: `src/cypher/executor.rs` (exec_left_outer_join null-fill section)

- [ ] **Step 1: Check current null-filling logic**

Read `exec_left_outer_join` (around line 2431) to see how `optional_aliases` are null-filled. The current code likely sets `alias` to `Value::Null` and `alias.__id`/`alias.__src` etc. For var-length rel aliases, we just need `alias → Value::Null` (no `__src`/`__dst` needed since they're stored as a list, not flat keys).

This should already work correctly since the var-length branch stores `r_alias → Value::List(...)` directly, and the null-fill sets `r_alias → Value::Null`. No change needed unless testing reveals issues.

- [ ] **Step 2: Run TCK for OPTIONAL MATCH scenarios**

Run: `cargo test --test tck -- Match7 2>&1 | tail -10`
Expected: Check if Match7[12,14] pass.

- [ ] **Step 3: Commit if changes were needed**

```bash
git add src/cypher/executor.rs
git commit -m "fix: OPTIONAL MATCH null-filling works with var-length rel aliases"
```

---

### Task 7: Fix property filtering on var-length relationships

Match4[5] uses `[:WORKED_WITH* {year: 1988}]` — property predicates on var-length edges. The planner generates a Filter with `r_alias.year = 1988`, but for var-length patterns, `r_alias` is a list, not a flat edge with `r_alias.year` keys.

**Files:**
- Modify: `src/cypher/executor.rs` or `src/cypher/planner.rs`
- Modify: `src/edge.rs:367-445` (traverse_paths — add property filtering)

- [ ] **Step 1: Add property filter parameter to `traverse_paths`**

Add an optional `prop_filters: &Properties` parameter to `traverse_paths`. At each hop, check the edge properties against the filters and skip edges that don't match.

```rust
pub fn traverse_paths(
    conn: &Connection,
    start: NodeId,
    labels: &[&str],
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    prop_filters: &Properties,  // NEW — empty = no filtering
) -> Result<Vec<(NodeId, Vec<PathStep>)>> {
    // ... existing setup ...

    while let Some((current, path, visited_edges)) = stack.pop() {
        // ... existing depth check ...

        for label in labels {
            let neighbors = get_neighbors(conn, current, label, direction)?;
            for neighbor in neighbors {
                // ... existing edge direction detection ...

                // Property filter: check edge properties match.
                if !prop_filters.is_empty() {
                    let props = get_edge_properties(conn, edge_src, edge_dst, label)?;
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

                // ... rest unchanged ...
            }
        }
    }
}
```

- [ ] **Step 2: Update all callers of `traverse_paths`**

Add `&Properties::new()` (or `&HashMap::new()`) as the last argument to all existing `traverse_paths` calls in `executor.rs` and `iter.rs`.

- [ ] **Step 3: Wire property filters from the planner**

The planner currently generates a separate `Filter` node for relationship property predicates (lines 2649-2658 in planner.rs). For var-length patterns, these filters don't work because `r_alias.year` isn't in the record (it's inside the list). Two approaches:

**Option A** (simpler): Pass the property filters through the `Expand` IR node and into `traverse_paths`. Add a `rel_prop_filters: Properties` field to `LogicalOp::Expand`.

**Option B** (less planner change): In the executor, detect var-length + rel property filters and apply them inside traverse_paths. This requires extracting the filter predicates from the `Filter` node that wraps the `Expand`.

Go with Option A. Add `rel_prop_filters: Properties` to `LogicalOp::Expand` in `ir.rs`. In the planner, when building an Expand for a var-length pattern, move inline relationship properties from a separate Filter into the `rel_prop_filters` field. Update `exec_expand`, `exec_correlated`, and `ExpandIter` to pass them to `traverse_paths`.

- [ ] **Step 4: Run TCK for Match4[5]**

Run: `cargo test --test tck -- "Match4" 2>&1 | tail -10`
Expected: Match4[5] should pass.

- [ ] **Step 5: Commit**

```bash
git add src/edge.rs src/cypher/ir.rs src/cypher/planner.rs src/cypher/executor.rs src/cypher/iter.rs
git commit -m "feat: property filtering on var-length relationship patterns"
```

---

### Task 8: Handle `traverse_paths` max_hops default (15 → higher for long chains)

Match4[4] creates a chain of 22 nodes and matches `[:T*]` which defaults to `*1..15`. This won't reach the end. The default max_hops needs to be higher (e.g., 50 or 100) or configurable.

**Files:**
- Modify: `src/cypher/parser.rs` (default max_hops)

- [ ] **Step 1: Increase default max_hops**

In `parser.rs`, find where `*` without bounds defaults to `(1, 15)`. Change to a higher limit. The standard Neo4j default is unbounded but for practical purposes, 50 should suffice for TCK tests.

Search for the constant `15` in `parse_var_length`:

```rust
// Change: "*" alone → (1, 15) to (1, 50)
// Change: "*1.." → (1, 15) to (1, 50)
// Change: "*.." → (1, 15) to (1, 50)
```

- [ ] **Step 2: Run TCK for Match4[4]**

Run: `cargo test --test tck -- "Match4" 2>&1 | tail -10`
Expected: Match4[4] (22-node chain) should now reach the end.

- [ ] **Step 3: Commit**

```bash
git add src/cypher/parser.rs
git commit -m "feat: increase default var-length max_hops from 15 to 50"
```

---

### Task 9: Remove passing scenarios from skiplist and run full TCK

**Files:**
- Modify: `tests/tck/skiplist.txt`

- [ ] **Step 1: Run full TCK and identify newly passing scenarios**

Run: `cargo test --test tck 2>&1 | tail -10`

- [ ] **Step 2: Try removing skiplist entries one group at a time**

Start with the easy ones. Comment out Match4 entries in the skiplist and run TCK to see which pass:

```bash
# Comment out Match4 entries
cargo test --test tck 2>&1 | grep -i "fail\|error"
```

Repeat for Match6, Match7, Match9, Path2, Path3 groups.

- [ ] **Step 3: Remove all passing entries from skiplist**

Edit `tests/tck/skiplist.txt` to remove entries for scenarios that now pass. Leave entries for scenarios that still fail.

- [ ] **Step 4: Run full TCK — must be 0 failures**

Run: `cargo test --test tck 2>&1 | tail -5`
Expected: 0 failures, higher pass count, reduced skiplist.

- [ ] **Step 5: Commit**

```bash
git add tests/tck/skiplist.txt
git commit -m "feat: TCK var-length paths — remove passing scenarios from skiplist"
```

---

### Task 10: Handle remaining edge cases

Some scenarios may not pass after the core changes. Address them individually.

**Files:**
- Various (depends on failures)

- [ ] **Step 1: Match4[7] — bound relationship in var-length pattern**

Query: `MATCH ()-[r:EDGE]-() MATCH p = (n)-[*0..1]-()-[r]-()-[*0..1]-(m) RETURN count(p) AS c`

This is a complex multi-pattern MATCH with a var-length segment, a fixed bound relationship, and another var-length segment. The planner needs to handle re-using `r` as a bound rel inside a path with var-length segments. This likely requires special handling in the planner or executor and may be deferred if too complex.

- [ ] **Step 2: Match4[8] — relationship list as var-length spec**

Query: `MATCH ()-[r1]->()-[r2]->() WITH [r1, r2] AS rs LIMIT 1 MATCH (first)-[rs*]->(second) RETURN first, second`

This uses a variable (`rs`) that holds a list of relationships AS the var-length pattern. This is a special Cypher syntax where a pre-collected list of rels acts as a path template. This is an advanced feature and may be deferred.

- [ ] **Step 3: Match6[14] — undirected fixed var-length**

Query: `MATCH topRoute = (:Start)<-[:CONNECTED_TO]-()-[:CONNECTED_TO*3..3]-(:End) RETURN topRoute`

This mixes directed and undirected (actually `*3..3` is a fixed-length var-length). The pattern has a directed segment then a var-length undirected segment. Check if the path materializes correctly with intermediate nodes.

- [ ] **Step 4: Match6[25] — error on rebinding path variable**

Query: `WITH <literal> AS p MATCH p = ()-[]-() RETURN p`

This should raise `VariableAlreadyBound`. Check if the planner already detects this. If not, add a check in the planner when a `MaterializePath` alias is already bound to a non-path value.

- [ ] **Step 5: Run full TCK, remove any additional passing scenarios from skiplist**

```bash
cargo test --test tck 2>&1 | tail -5
```

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: var-length path edge cases — bound rels, undirected, error validation"
```

---

### Task 11: Update TODO.md

**Files:**
- Modify: `TODO.md`

- [ ] **Step 1: Update Tier 3 status in TODO.md**

Mark completed items as done, note any remaining scenarios that couldn't be fixed.

- [ ] **Step 2: Run `uv run tests/tck/analyze_blockers.py` to get updated counts**

- [ ] **Step 3: Update the header counts in TODO.md**

- [ ] **Step 4: Commit**

```bash
git add TODO.md
git commit -m "docs: update TODO with var-length path progress"
```
