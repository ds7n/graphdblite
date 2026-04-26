# Variable-Length Path Execution

**Date**: 2026-04-26
**Status**: Approved
**Scope**: ~15 TCK scenarios (Match4[4,5,7,8], Match6[14-17,19,20,25], Match7[12,14], Path2[1,2], Path3[1], Match9[1,9])

## Problem

Grammar, parser, AST, and planner already handle variable-length relationship syntax (`[*1..3]`). Execution is broken: `edge::traverse()` returns only destination `NodeId`s, discarding the edge sequence. TCK requires:

- Relationship variables in var-length patterns bind to **lists of edges**, not single edges
- `relationships(p)` returns the full ordered edge sequence from a path
- Zero-length paths (`*0..`) match a node to itself with an empty edge list
- Property predicates on var-length edges filter each hop

## Design

### 1. Edge Traversal: Path-Returning BFS

Modify or wrap `edge::traverse_paths()` to return `Vec<(NodeId, Vec<EdgeInfo>)>` where `EdgeInfo` captures `(edge_id, src, dst, type, properties)`.

Semantics:
- BFS up to `max_hops`, emit at each depth >= `min_hops`
- No node deduplication — same node via different paths = different result rows
- **Relationship uniqueness**: no edge repeated within a single path (standard Cypher)
- Zero-length (`min_hops=0`): emit `(start_node, vec![])` as first result
- Edge type filtering: only traverse edges matching `edge_types` constraint
- Property filtering: prune paths where any hop's edge fails property predicates

### 2. Executor: `exec_expand` (var_length=true)

Replace `traverse()` call with path-returning traversal. For each `(dest_node, edge_list)`:

- Bind `dst_alias` to destination node (flat bindings: `dst.__id`, `dst.__label`, `dst.prop`, etc.)
- Bind `rel_alias` to `Value::List` of `Value::Edge` objects
- Set marker `rel_alias.__var_length = true` so downstream operators know this is a list binding

For `min_hops=0`: emit row where `dst_alias = src_alias` and `rel_alias = []`.

### 3. Iterator: `ExpandIter` (correlated var-length)

Mirror `exec_expand` changes. When `var_length=true`:
- Call path-returning traversal from current source
- Buffer results, emit one row per path
- Bind rel_alias to edge list per row

### 4. Path Materialization: `exec_materialize_path`

Currently assumes one edge per rel_alias. For var-length rel_aliases:
- Detect list binding (check `__var_length` marker or value type)
- Expand the edge list into the path's alternating node-edge-node sequence
- Fetch intermediate node data from edge src/dst pairs
- Result: `Value::Path { nodes: [...], edges: [...] }` with full sequence

### 5. `build_compound_binding`

When rel_alias has `__var_length=true`, return `Value::List` directly instead of reconstructing a single `Value::Edge`.

### 6. Functions

No changes needed — `relationships()`, `length()`, `last()` already work on correctly-formed paths and lists.

## Files Changed

| File | Change |
|------|--------|
| `src/edge.rs` | Adapt `traverse_paths()` to return edge sequences with properties; add relationship uniqueness; add property filtering |
| `src/cypher/executor.rs` | `exec_expand`: var-length branch calls path traversal, binds rel_alias to list; `exec_materialize_path`: handle list rel_aliases; `build_compound_binding`: handle var-length marker |
| `src/cypher/iter.rs` | `ExpandIter`: var-length branch mirrors executor changes |
| `tests/tck/skiplist.txt` | Remove passing scenarios |

## Edge Cases

- **Scenario Match4[7]**: Bound relationship reused in var-length pattern — relationship uniqueness handles this (bound rel excluded from traversal candidates)
- **Scenario Match4[8]**: List of relationships used as var-length spec — this is an advanced feature, may need special handling in planner/executor
- **Scenario Match6[25]**: Path variable already bound to non-path value — error validation, likely already handled or simple to add
- **Scenario Match6[14]**: Undirected var-length — `Direction::Both` must try both directions at each hop

## Non-Goals

- Grammar/parser/planner changes (already complete)
- Performance optimization (correctness first)
- Var-length in CREATE/MERGE (correctly rejected already)
