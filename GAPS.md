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

### ~~OPTIONAL MATCH~~ ✅ DONE
Added `optional_match_clause` grammar rule, `optional_patterns` field on `MatchStatement`,
`LeftOuterJoin` IR operator, and executor support. Shared aliases are join keys;
unmatched rows get NULL bindings for optional aliases and their properties.

---

### ~~MATCH without RETURN (side-effect-only queries)~~ ✅ DONE
Covered by MATCH...CREATE implementation. DELETE and SET already worked without RETURN.

---

### ~~WHERE clause: property existence check and IS NULL~~ ✅ DONE
Added `is_null_check` and `is_not_null_check` grammar rules, `IsNull`/`IsNotNull` AST variants,
and eval support.

---

### ~~Aggregate group-by correctness~~ ✅ DONE
Added integration tests for grouped count and grouped collect. Group-by
implementation verified correct with real multi-group data. Vec linear scan
is fine at current scale; HashMap optimization deferred.

---

### ~~Index-aware query planning~~ ✅ DONE
Planner now accepts `&Connection` and checks `list_indexes_for_label()` when
a node pattern has inline property equality filters. If an index exists for
one of the properties, emits `IndexLookup` IR op instead of `Scan` + `Filter`.
Remaining non-indexed properties become an inline filter on the lookup results.
Executor handles `IndexLookup` via `index::index_lookup()` + `node::get_node()`.

---

## Larger Gaps (4+ hours, not blocking v0.1)

### ~~Value::List type~~ ✅ DONE
Added `Value::List(Vec<Value>)` variant to `Value` enum with Display, serde,
and comparison support. Fixed `collect()` aggregate to return a proper list
instead of count. Unblocks `UNWIND` and list comprehensions in future.

---

### ~~WITH clause (query chaining)~~ ✅ DONE
Added `with_clause` grammar rule, `WithClause` AST struct, and `plan_with()`
in the planner. WITH acts as intermediate Project (+Aggregate if items contain
function calls) + optional Filter. Supports projection, aliases, WHERE, and
aggregation (e.g., `WITH n.dept AS dept, count(*) AS cnt WHERE cnt > 1`).

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
