# graphdblite v0.1.0 — Gaps & Needed Fixes

> Working document for tracking what's missing, broken, or rough before v0.1
> can be considered production-ready for symtext integration.

## Quick Fixes (< 1 hour each)

### CLI: suppress internal fields in output
**Problem:** `RETURN n` shows `n.__id`, `n.__label` alongside real properties.
These are internal executor bookkeeping fields leaked into output.

**Fix:** In `src/cypher/executor.rs`, filter fields starting with `__` from
projected records. Or in `src/bin/cli.rs`, strip them at display time.

**File:** `src/bin/cli.rs:print_records()` or `src/cypher/executor.rs:exec_project()`

---

### RETURN * support
**Problem:** `MATCH (n:Person) RETURN *` fails to parse. The grammar accepts
`*` as an expression but the executor doesn't know how to project all bound
variables when it encounters `Expr::Star` in a RETURN item.

**Fix:** In `exec_project`, when a ReturnItem is `Expr::Star`, copy all
non-`__` fields from the input record to the output record.

**File:** `src/cypher/executor.rs:exec_project()`

---

### Expression eval: allow identifiers as node references
**Problem:** `RETURN n` evaluates to the raw node ID integer, not a useful
representation. Users expect either the full node (label + properties) or
at minimum a dict of properties.

**Fix:** When projecting a bare variable, expand it to all `{var}.{prop}`
fields as a nested structure, or at minimum include all flattened properties.

**File:** `src/cypher/executor.rs:exec_project()`

---

## Medium Fixes (1-4 hours each)

### Multi-clause queries: MATCH ... CREATE / MATCH ... MERGE with edges
**Problem:** Cypher allows `MATCH (a:Person {name:'Alice'}), (b:Person {name:'Bob'}) CREATE (a)-[:KNOWS]->(b)`.
Currently the parser only handles `MATCH ... RETURN`, `MATCH ... DELETE`,
`MATCH ... SET`, standalone `CREATE`, and standalone `MERGE`. There's no
combined MATCH+CREATE for wiring edges between existing nodes.

**Fix:** Add a `MatchCreate` variant to the AST and planner that runs the
MATCH pipeline, then uses the bound variables to execute CREATE operations.
Grammar change: allow `create_pattern_list` after a `MATCH ... WHERE` block
without requiring RETURN.

**Files:** `src/cypher/grammar.pest`, `src/cypher/ast.rs`, `src/cypher/parser.rs`,
`src/cypher/planner.rs`, `src/cypher/executor.rs`

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

### MATCH without RETURN (side-effect-only queries)
**Problem:** `MATCH (n:Person) DELETE n` works, but the grammar requires
RETURN for plain MATCH queries. Queries like `MATCH (a),(b) CREATE (a)-[:E]->(b)`
need to work without a RETURN clause.

**Fix:** Make `return_clause` optional in the grammar for match_stmt when
followed by a mutation clause.

**File:** `src/cypher/grammar.pest`, `src/cypher/parser.rs`

---

### WHERE clause: property existence check and IS NULL
**Problem:** No way to write `WHERE n.email IS NOT NULL` or `WHERE EXISTS(n.email)`.
The expression evaluator treats missing properties as NULL but there's no
`IS NULL` / `IS NOT NULL` syntax in the grammar.

**Fix:** Add `is_null` and `is_not_null` as comparison operators in the grammar
and expression evaluator.

**Files:** `src/cypher/grammar.pest`, `src/cypher/ast.rs`, `src/cypher/eval.rs`

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

### Multiple MATCH clauses (implicit join)
**Problem:** `MATCH (a:Person), (b:Company) WHERE a.employer = b.name RETURN a, b`
parses the pattern list but the planner naively discards all but the last pattern.
Should produce a cross-product filtered by WHERE, or ideally a hash join.

**Fix:** Implement proper cross-product or hash join in the planner/executor
for multi-pattern MATCH.

**File:** `src/cypher/planner.rs:plan_patterns()`, `src/cypher/executor.rs`

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
