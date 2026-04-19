# TCK Conformance Status

Last updated: 2026-04-19

## Current pass rate

Full openCypher TCK vendored (220 feature files, commit `677cbaf`).

```
130 features parsed (1 parse error: Match5.feature)
2034 total scenario instances running
1952 passed
  82 skipped (cucumber-level)
 678 skiplisted (known failures)
 ────
1952/2630 unique scenarios passing (74.2%)
```

## Pass rate by area

Regenerate with: `cargo test --test tck 2>&1 > /tmp/tck_output.txt && uv run tests/tck/analyze.py /tmp/tck_output.txt`

All running scenarios pass at 100%. The table below shows total scenarios
(running + skiplisted) per area:

| Area | Running | Skiplisted | Total |
|------|--------:|-----------:|------:|
| Literals | 122 | 0 | 122 |
| Match | 68 | 52 | 120 |
| Quantifier | 88 | 13 | 101 |
| WithOrderBy | 29 | 74 | 103 |
| List | 46 | 49 | 95 |
| Temporal | 8 | 81 | 89 |
| Create | 67 | 10 | 77 |
| Merge | 46 | 27 | 73 |
| Precedence | 43 | 0 | 43 |
| Return | 24 | 28 | 52 |
| TypeConversion | 40 | 8 | 48 |
| Graph | 38 | 10 | 48 |
| Call | 39 | 2 | 41 |
| Set | 22 | 28 | 50 |
| Boolean | 36 | 0 | 36 |
| Pattern | 3 | 33 | 36 |
| Delete | 15 | 18 | 33 |
| String | 29 | 3 | 32 |
| Remove | 20 | 12 | 32 |
| ReturnSkipLimit | 24 | 7 | 31 |
| Comparison | 26 | 0 | 26 |
| ReturnOrderBy | 21 | 8 | 29 |
| Aggregation | 22 | 5 | 27 |
| With | 14 | 13 | 27 |
| MatchWhere | 18 | 12 | 30 |
| TriadicSelection | 0 | 19 | 19 |
| Map | 3 | 13 | 16 |
| WithWhere | 4 | 15 | 19 |
| Null | 10 | 6 | 16 |
| Unwind | 5 | 9 | 14 |
| Union | 8 | 4 | 12 |
| CountingSubgraphMatches | 4 | 7 | 11 |
| ExistentialSubquery | 4 | 6 | 10 |
| WithSkipLimit | 4 | 5 | 9 |
| Path | 0 | 7 | 7 |
| Mathematical | 2 | 3 | 5 |
| Conditional | 0 | 2 | 2 |
| **Total** | **952** | **678** | **1630** |

## Highest-impact work items

Regenerate with: `uv run tests/tck/analyze_blockers.py`

### Sole blockers (fixing this alone unblocks the scenario)

| Sole | Impact | Construct |
|-----:|-------:|-----------|
| 113 | 218 | write-result (CREATE/MERGE ... RETURN) |
| 30 | 49 | temporal types |
| 8 | 14 | ORDER BY |
| 8 | 14 | parameter $param |
| 7 | 7 | list slicing [a..b] |
| 6 | 14 | list functions |
| 5 | 6 | IN [list] |
| 4 | 16 | MERGE |
| 4 | 4 | UNION |
| 4 | 6 | pattern comprehension |
| 4 | 4 | single()/none()/any()/all() |
| 3 | 13 | CREATE (no RETURN) |
| 2 | 4 | list indexing [n] |
| 1 | 18 | DELETE/DETACH DELETE |
| 1 | 18 | SET property/label |
| 1 | 14 | aggregation (non-count) |
| 1 | 13 | IS NULL / IS NOT NULL |
| 1 | 4 | float literal |
| 1 | 2 | math functions |
| 1 | 1 | CASE/WHEN |

### High-impact (in scenarios with ≤2 missing constructs)

| Impact | Construct |
|-------:|-----------|
| 218 | write-result (CREATE/MERGE ... RETURN) |
| 49 | temporal types |
| 23 | OPTIONAL MATCH |
| 23 | var-length rel `*` |
| 18 | DELETE/DETACH DELETE |
| 18 | SET property/label |
| 16 | MERGE |
| 14 | aggregation (non-count) |
| 14 | parameter $param |
| 14 | ORDER BY |
| 14 | list functions |

## Skiplist

`skiplist.txt` lists 678 known-failing `Feature::Scenario` pairs.
The harness filters these out and exits non-zero only if a *non-skiplisted*
scenario fails — making the TCK a regression gate.

## History

### Phase 17 — Quantifier edge cases, rand(), CASE+operator fix (2026-04-19)

- [x] Add `rand()` function
- [x] Fix CASE WHEN in arithmetic expressions (was short-circuiting in `cmp_primary`)
- [x] Fix WITH alias shadowing in projection (coalesce(x, y) AS x)
- Result: 1816 → 1952 passing scenarios (+136), skiplist 706 → 678

### Phase 16 — TypeConversion + Graph functions (2026-04-19)

- [x] Add `toBoolean()` function with TypeError for invalid types
- [x] Fix `toInteger()`, `toFloat()`, `toString()` to raise TypeError for invalid types
- [x] Add `properties()` function for nodes, edges, maps
- [x] Add `relationships()` function for paths
- [x] Fix `labels()`, `type()`, `keys()`, `id()` for compound Value types
- [x] Add dynamic property access (`n['key']`)
- [x] Add `with_stmt` grammar rule for standalone `WITH ... RETURN`
- [x] Fix aggregate function name case sensitivity
- Result: 1795 → 1816 passing scenarios (+21), skiplist 750 → 706

### Phase 15 — Boolean/Comparison/Precedence overhaul (2026-04-19)

- [x] Restructure expression precedence: `expr → xor → and → not → predicate → cmp → add → mul → exp → unary → atom`
- [x] Add exponentiation operator (`^`)
- [x] Add chained comparison desugaring (`a < b < c` → `a < b AND b < c`)
- [x] Three-valued boolean type checking (AND/OR/XOR/NOT error on non-boolean)
- [x] NaN handling (`0.0/0.0` → NaN, comparisons → false)
- [x] Boolean/list ordering in comparisons
- [x] Three-valued list and map equality with null propagation
- Result: 1512 → 1795 passing scenarios (+283), skiplist 828 → 750

### Phase 14 — Multi-clause CREATE/MERGE (2026-04-19)

- [x] Add `multi_clause_stmt` grammar and `MultiClauseStatement` AST
- [x] Multi-clause planner threading LogicalOp through CREATE/MERGE/WITH chains
- [x] Relationship variable binding in CREATE/MERGE edge operations
- [x] CREATE validation (VariableAlreadyBound) and null property handling
- Result: 1468 → 1512 passing scenarios (+44), skiplist 873 → 828

### Phase 13 — Aggregation DISTINCT + percentile/stdev (2026-04-19)

- [x] Per-function DISTINCT (`count(DISTINCT x)`, `collect(DISTINCT x)`)
- [x] Add `percentileDisc`, `percentileCont`, `stDev`, `stDevP` aggregation functions
- [x] Thread `distinct: bool` and `extra_arg` through grammar → AST → IR → executor
- Result: 1452 → 1468 passing scenarios (+16), skiplist 881 → 873

### Phase 12 — OPTIONAL MATCH + SET extensions (2026-04-19)

- [x] OPTIONAL MATCH: null property access, LeftOuterJoin null-filling, MaterializePath null handling
- [x] Add `n:Label` predicate (has_label) in WHERE clauses
- [x] OPTIONAL MATCH WHERE uses opt_filter (null-fill instead of drop rows)
- [x] SET `n:Label`, `SET n = {map}`, `SET n += {map}` — new SetItem enum, grammar, parser, IR, planner, executor
- [x] `add_node_label()` and `set_all_node_properties()` storage functions
- Result: 1413 → 1452 passing scenarios (+39), skiplist 920 → 881

### Phase 11 — Write-clause RETURN support for SET and DELETE (2026-04-19)

- [x] Add optional RETURN clause to `set_stmt` and `delete_stmt` grammar rules
- [x] Add `optional_match_clause*` to `set_stmt` grammar
- [x] Extend `SetStatement` and `DeleteStatement` AST with return_clause, order_by, skip, limit fields
- [x] Update parser (`parse_set`, `parse_delete`) to extract RETURN fields
- [x] Update `resolve_params` for new AST fields
- [x] Planner: add `apply_return_projection()` calls for SET and DELETE
- [x] Planner: add optional_patterns LeftOuterJoin for SET
- [x] Executor: `exec_set_property` returns modified records (updates record with new property values)
- [x] Executor: `exec_delete` returns input records for downstream RETURN projection
- [x] Batch unskip: 10 scenarios newly passing (8 Delete6 side-effect persistence, 2 Set1)
- Result: 1404 → 1413 passing scenarios (+9), skiplist 929 → 920

### Phase 10 — Temporal sorting/arithmetic, UNWIND...CREATE...WITH (2026-04-19)

- [x] Add temporal types to `compare_values_for_sort` (Date, LocalTime, Time, DateTime, Duration)
- [x] Implement temporal arithmetic: Date/Time/DateTime ± Duration, Duration ± Duration, Duration * Number
- [x] Extend `unwind_create` grammar for intermediate WITH/MATCH/UNWIND clauses
- [x] Planner support for intermediate clauses in UnwindBody::Create
- Result: 1393 → 1404 passing scenarios (+11), skiplist 940 → 929

### Phase 9 — Aggregation, ORDER BY, IS NULL, Parameters (2026-04-19)

- [x] Fix aggregate column name mismatch (`max(*)` → `max(x)`) via `agg_col_name` helper
- [x] Add Bool and List to `compare_values_for_sort` with Cypher type ordering
- [x] Fix WITH clause ordering: WHERE before ORDER BY/SKIP/LIMIT
- [x] Add map property access in eval (map.key patterns)
- [x] Add `value_to_expr` for List/Map parameter resolution
- [x] Extend `unwind_return` grammar to support intermediate WITH/MATCH/UNWIND clauses
- [x] Planner: handle intermediate clauses in `plan_unwind`
- Result: 1351 → 1393 passing scenarios (+42), skiplist 966 → 940

### Phase 8 — Temporal types (2026-04-19)

- [x] Add `chrono` and `chrono-tz` dependencies
- [x] Create `src/temporal.rs` with 6 wrapper types (CypherDate, CypherLocalTime, CypherTime, CypherLocalDateTime, CypherDateTime, CypherDuration)
  - ISO 8601 Display, string parsing, map construction
  - Custom Serialize/Deserialize for MessagePack storage
  - PartialEq, Eq, Hash implementations
- [x] Extend Value enum with 6 temporal variants
- [x] Add temporal constructor functions to grammar and evaluator
- [x] Add `dotted_function_call` grammar rule for `datetime.fromepoch` etc.
- [x] Add temporal component accessors (d.year, t.hour, etc.)
- [x] Extend values_equal and compare_values for temporal types
- [x] Update TCK harness for temporal-vs-string comparison
- Result: 1298 → 1351 passing scenarios (+53), skiplist 974 → 966

### Phase 7 — Quantifier functions, REMOVE statement, labels() (2026-04-19)

- [x] Implement quantifier predicates: `none()`, `single()`, `any()`, `all()`
  - New `quantifier_expr` grammar rule with `(x IN list WHERE pred)` syntax
  - `QuantifierKind` enum + `Expr::Quantifier` AST variant
  - Three-valued null logic in `eval_quantifier()`
  - Added `none`, `single`, `any` to keyword list
- [x] Implement REMOVE statement: `REMOVE n.prop` and `REMOVE n:Label`
  - Grammar, AST (`RemoveStatement`, `RemoveItem`), parser, IR, planner, executor
  - `node::remove_node_label()` storage function
  - Record updates after removal for correct RETURN results
  - Fix `decrement_label_count` to delete entry when count reaches 0
- [x] Add `labels()` function (grammar + eval)
- [x] Batch unskip: 77 scenarios removed from skiplist
- Result: 841 → 1298 passing scenarios (+457), skiplist 1051 → 974

### Phase 6 — Skiplist cleanup & targeted fixes (2026-04-17)

- [x] Allow keywords as labels/rel types (`symbolic_name` grammar rule)
- [x] Filter null properties in CREATE (`{p: null}` no longer stored)
- [x] Fix CREATE pattern duplicate nodes (named variable dedup across patterns)
- [x] Batch unskip: 45 confirmed-passing scenarios removed from skiplist
- [x] Add newly-exposed failing scenarios from grammar fix to skiplist
- Result: 785 → 841 passing scenarios (+56), skiplist 1096 → 1051

### Phase 5 — Grammar & expression improvements (2026-04-17)

- [x] Fix label counting SQL in test harness
- [x] Always store edge_props rows for accurate relationship counting
- [x] Unify expr/bool_expr grammar (boolean ops in all expression contexts)
- [x] Add modulo operator (%)
- [x] Add float exponent and leading-dot literals
- [x] Batch unskip: Boolean, Null, Comparison, Precedence, Literals5
- Result: 494 → 785 passing scenarios (+291), skiplist 1136 → 1096

### Phase 4 — scaling up (2026-04-17)

- [x] Vendor the full `tck/features/` tree (220 feature files)
- [x] Vendor `tck/graphs/` (binary-tree-1, binary-tree-2)
- [x] Add `tests/tck/skiplist.txt` for known-failing scenarios
- [x] Flip exit status: harness fails on non-skiplisted regressions
- [x] Track pass-rate over time

### Phase 3 — pilot (complete)

Original 5 pilot feature files, 112 scenarios, 100% pass rate.
