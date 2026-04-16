# TCK Conformance Status

Last updated: 2026-04-16 (commit e730e56)

## Current pass rate

```
5 features, 112 scenarios
102 passed, 10 failed (91%)
347/357 steps passed
```

## Pilot feature files

| Feature | Scenarios | Passing | Notes |
|---------|-----------|---------|-------|
| Literals1 (Boolean and Null) | 6 | 6 | |
| Literals2 (Integer) | 12 | 10 | 2 fail: overflow panics instead of error |
| Match1 (Match nodes) | 6 + 80 outlines | all but 1 | 1 fail: multi-label compare |
| Return1 | 2 | 1 | 1 fail: undefined variable not detected |
| With1 (Forward variable) | 6 | 0 | 3 fail: inline edge CREATE; 2 fail: OPTIONAL MATCH + null; 1 fail: path literal in compare |

## Remaining failures — categorized

### 1. `CREATE (:A)-[:REL]->(:B)` inline edge creation (3 failures)

**Scenarios**: With1 [1], [2], [3]

Our `CREATE` statement only supports node-only patterns. Creating nodes and
edges in a single `CREATE` clause (e.g. `CREATE (a:A)-[:REL]->(b:B)`) is not
implemented — we require `MATCH...CREATE` or the typed API for edge creation.

**Fix**: Extend `create_stmt` in the grammar and `plan_create` / executor to
detect edge patterns within CREATE and create both endpoints + edge. Medium
effort — touches grammar, parser, planner, executor.

### 2. Integer overflow panics (2 failures)

**Scenarios**: Literals2 [9] (too large), [10] (too small)

Queries like `RETURN 10000000000000000000000` trigger an `unwrap()` panic in
the TCK step's `When executing query:` handler because the error propagates
as `Serialization("invalid integer: ...")` and the step assumes success.

**Fix**: The step handler already captures errors into `world.last_error`, but
the panic comes from *parsing* the integer inside the query executor, not from
the step. The parser/executor should return a structured `SyntaxError` instead
of panicking on integer overflow. Small fix in `parser.rs` integer literal
handling.

### 3. Multi-label node comparison (1 failure)

**Scenario**: Match1 [3] — expected cell `(:A:B)` not parsed by compare.rs

The expected-value parser in `tests/tck_support/compare.rs` only handles
single-label node patterns like `(:A {props})`. Multi-label syntax `(:A:B)`
needs a loop in `parse_node()` to collect multiple `:Label` segments.

**Fix**: Small change to `compare.rs::parse_node()`. Graphdblite itself does
not yet support multi-label nodes, so the query may also need work — but the
compare parser should not be the bottleneck.

### 4. Undefined variable detection (1 failure)

**Scenario**: Return1 [2] — `MATCH (n) RETURN r` should raise SyntaxError
(UndefinedVariable) but currently succeeds with null.

The planner does not validate that all variables referenced in the RETURN
clause are actually bound by the MATCH pattern.

**Fix**: Add a semantic validation pass in the planner that collects variables
bound by MATCH patterns and checks RETURN items against them. Medium effort.

### 5. OPTIONAL MATCH + null forwarding via WITH (2 failures)

**Scenarios**: With1 [5] (forwarding null), [6] (forwarding possibly-null node)

These scenarios use `OPTIONAL MATCH` to produce null rows, then `WITH` to
forward them. The exact failure needs investigation — likely an interaction
between the compound-value projection and OPTIONAL MATCH null handling.

**Fix**: Needs investigation. May be a bug in how `LeftOuterJoin` emits null
rows for compound bindings, or in how `WITH` propagates them.

### 6. Path literal `<()>` in compare.rs (1 failure)

**Scenario**: With1 [4] — expected cell `<()>` represents a single-node path.

The expected-value parser does not handle path literal syntax
(`<(:A)-[:R]->(:B)>`). Deferred to Phase 4 scope.

**Fix**: Add `parse_path()` to `compare.rs` that parses `<...>` path syntax.
Small effort.

## Phase 4 — scaling up (not yet started)

- Vendor the full `tck/features/` tree (hundreds of feature files)
- Add `tests/tck/skiplist.txt` for known-failing scenarios
- Flip exit status: harness fails on non-skiplisted regressions
- Track pass-rate over time
