# graphdblite v0.1.0 — Gaps & Needed Fixes

> Working document for tracking what's missing, broken, or rough before v0.1
> can be considered production-ready for symtext integration.

## Quick Fixes (< 1 hour each)

### ~~CLI: suppress internal fields in output~~ ✅ DONE
`exec_project()` filters `__` fields for all projection paths; CLI has safety net filter.

---

### ~~RETURN * support~~ ✅ DONE
`exec_project()` handles `Expr::Star` — expands all non-internal `alias.prop` fields.

---

### ~~Expression eval: allow identifiers as node references~~ ✅ DONE
`exec_project()` handles `Expr::Variable` — expands bare `n` to all `n.prop` fields.

---

## Medium Fixes (1-4 hours each)

### ~~Multi-clause queries: MATCH ... CREATE / MATCH ... MERGE with edges~~ ✅ DONE
New `match_create_stmt` grammar rule, `MatchCreate` AST/IR variants, and `exec_match_create()`.
Also fixed multi-pattern MATCH with `CrossProduct` IR op (was discarding all but last pattern).

---

### OPTIONAL MATCH
**Problem:** Not implemented. OPTIONAL MATCH works like a left outer join —
if the pattern doesn't match, variables are bound to NULL instead of filtering
the row out.

**Fix:** Add `OptionalMatch` to AST, plan it as an Expand that preserves
input records with NULL dst bindings when no match is found.

**Files:** `src/cypher/grammar.pest`, `src/cypher/ast.rs`, `src/cypher/parser.rs`,
`src/cypher/planner.rs`, `src/cypher/executor.rs`

---

### ~~MATCH without RETURN (side-effect-only queries)~~ ✅ DONE
Covered by MATCH...CREATE implementation. DELETE and SET already worked without RETURN.

---

### ~~WHERE clause: property existence check and IS NULL~~ ✅ DONE
Added `is_null_check` and `is_not_null_check` grammar rules, `IsNull`/`IsNotNull` AST variants,
and eval support.

---

### Aggregate group-by correctness
**Problem:** `RETURN n.label, count(*) AS cnt` works in theory but the
group-by implementation in `exec_aggregate` uses `Vec` linear scan for group
matching. Also untested with real grouped data.

**Fix:** Add integration tests for grouped aggregates. Consider switching to
a `HashMap` keyed by serialized group values for correctness and performance.

**File:** `src/cypher/executor.rs:exec_aggregate()`

---

### Index-aware query planning
**Problem:** The planner always generates `Scan` + `Filter`. It never checks
if a secondary index exists to use `IndexLookup` instead. For example,
`MATCH (n:Person {name: 'Alice'})` does a full label scan even if an index
on `Person.name` exists.

**Fix:** In the planner, when a node pattern has inline property equality
filters, check if indexes exist and emit `IndexLookup` instead of `Scan`.
This requires passing a connection or index metadata to the planner.

**Files:** `src/cypher/planner.rs`, possibly `src/cypher/executor.rs`

---

## Larger Gaps (4+ hours, not blocking v0.1)

### Value::List type
**Problem:** No list/array value type. This blocks `collect()` aggregate
(currently returns count instead of list), `UNWIND`, and list comprehensions.

**Fix:** Add `Value::List(Vec<Value>)` to `src/types.rs`. Update serialization,
expression evaluator, and aggregate executor.

---

### WITH clause (query chaining)
**Problem:** `WITH` allows piping results between query parts:
`MATCH (n) WITH n.name AS name WHERE name STARTS WITH 'A' RETURN name`.
Not implemented.

**Fix:** Add `WITH` as an intermediate projection/filter/aggregation step
in the AST, planner, and executor.

---

### CASE expressions
**Problem:** No conditional expressions: `CASE WHEN n.age > 30 THEN 'senior' ELSE 'junior' END`.

---

### ~~Multiple MATCH clauses (implicit join)~~ ✅ DONE
Added `CrossProduct` IR op — multi-pattern MATCH now produces a nested-loop cross-product
filtered by WHERE. Hash join deferred as optimization.

---

### DETACH DELETE
**Problem:** `DELETE n` cascades (deletes edges), which is actually DETACH DELETE
semantics. Standard Cypher `DELETE` should fail if the node has edges.
`DETACH DELETE` explicitly cascades.

**Fix:** Add `DETACH DELETE` to grammar, make plain `DELETE` fail on nodes
with edges.

---

### Error messages
**Problem:** Parse errors from pest are technical and reference grammar rules.
Users need human-readable error messages pointing to the location in their query.

**Fix:** Post-process pest errors into user-friendly messages with line/column
and a pointer to the problematic token.

---

### Python: maturin packaging
**Problem:** Python bindings compile but aren't installable via `pip install graphdblite`.
Requires `maturin develop` in the repo.

**Fix:** Set up CI to build wheels with `maturin build`, publish to PyPI.
`pyproject.toml` and `python/graphdblite/__init__.py` are already in place.

---

## Also Fixed (not originally in this list)

### Label-optional MATCH ✅
`MATCH (n) RETURN *` scans all nodes regardless of label. One-line change in
`node::find_nodes_by_label()` to skip label filter when empty.

---

## Not Planned for v0.1 (from DESIGN.md)

These were explicitly scoped out:
- Subqueries
- `CALL` procedures
- `UNWIND`
- `EXISTS{}` patterns
- List comprehensions
- Shortest-path functions
- Cost-based optimizer (IR supports it, just no statistics collection yet)
- Additional query languages (GQL, SPARQL, Gremlin)
