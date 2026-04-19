# TCK Conformance Status

Last updated: 2026-04-19

## Current pass rate

Full openCypher TCK vendored (220 feature files, commit `677cbaf`).

```
130 features parsed (1 parse error: Match5.feature)
1494 total scenario instances running
1413 passed
  81 skipped (cucumber-level)
 920 skiplisted (known failures)
 ────
1413/2333 unique scenarios passing (60.6%)
```

## Pass rate by area

Regenerate with: `cargo test --test tck 2>&1 > /tmp/tck_output.txt && uv run tests/tck/analyze.py /tmp/tck_output.txt`

All running scenarios pass at 100%. The table below shows total scenarios
(running + skiplisted) per area:

| Area | Running | Skiplisted | Total |
|------|--------:|-----------:|------:|
| Match | 56 | 76 | 132 |
| Literals | 122 | 9 | 131 |
| WithOrderBy | 32 | 79 | 111 |
| List | 46 | 62 | 108 |
| Quantifier | 59 | 41 | 100 |
| Temporal | 8 | 81 | 89 |
| Create | 43 | 35 | 78 |
| Merge | 25 | 50 | 75 |
| Return | 25 | 38 | 63 |
| Set | 3 | 49 | 52 |
| Graph | 21 | 27 | 48 |
| TypeConversion | 9 | 38 | 47 |
| Precedence | 6 | 37 | 43 |
| Call | 39 | 2 | 41 |
| Boolean | 11 | 25 | 36 |
| Pattern | 3 | 33 | 36 |
| ReturnOrderBy | 21 | 14 | 35 |
| MatchWhere | 10 | 24 | 34 |
| Comparison | 10 | 23 | 33 |
| Delete | 7 | 26 | 33 |
| Remove | 21 | 12 | 33 |
| String | 29 | 3 | 32 |
| ReturnSkipLimit | 24 | 7 | 31 |
| With | 15 | 14 | 29 |
| Aggregation | 14 | 13 | 27 |
| WithWhere | 4 | 15 | 19 |
| Map | 3 | 16 | 19 |
| TriadicSelection | 0 | 19 | 19 |
| Null | 8 | 8 | 16 |
| Unwind | 5 | 9 | 14 |
| Union | 8 | 4 | 12 |
| CountingSubgraphMatches | 4 | 7 | 11 |
| ExistentialSubquery | 4 | 6 | 10 |
| WithSkipLimit | 4 | 5 | 9 |
| Path | 0 | 7 | 7 |
| Mathematical | 2 | 4 | 6 |
| Conditional | 0 | 2 | 2 |
| **Total** | **701** | **920** | **1621** |

## Highest-impact work items

Regenerate with: `uv run tests/tck/analyze_blockers.py`

### Sole blockers (fixing this alone unblocks the scenario)

| Sole | Impact | Construct |
|-----:|-------:|-----------|
| 139 | 294 | Write-clause RETURN side effects (CREATE/MERGE...RETURN) |
| 30 | 49 | Temporal types |
| 13 | 27 | CREATE (no RETURN) side effects |
| 13 | 20 | Quantifier predicates (remaining) |
| 10 | 12 | IN [list] |
| 9 | 26 | IS NULL / IS NOT NULL |
| 8 | 15 | Parameter $param |
| 8 | 14 | ORDER BY |
| 8 | 8 | List slicing [a..b] |
| 6 | 18 | List functions |
| 6 | 18 | String functions |
| 5 | 33 | MERGE |
| 5 | 10 | NOT prefix |
| 4 | 7 | XOR |
| 4 | 6 | Pattern comprehension |
| 4 | 4 | UNION |
| 2 | 5 | List indexing [n] |
| 1 | 45 | OPTIONAL MATCH |
| 1 | 31 | SET property/label |
| 1 | 23 | Aggregation (non-count) |
| 1 | 18 | DELETE/DETACH DELETE |
| 1 | 5 | Float literal |
| 1 | 1 | CASE/WHEN |
| 1 | 1 | CONTAINS/STARTS/ENDS |

### High-impact (in scenarios with ≤2 missing constructs)

| Impact | Construct |
|-------:|-----------|
| 294 | Write-clause RETURN side effects |
| 49 | Temporal types |
| 45 | OPTIONAL MATCH |
| 33 | MERGE |
| 31 | SET property/label |
| 27 | CREATE (no RETURN) side effects |
| 26 | IS NULL / IS NOT NULL |
| 23 | Aggregation (non-count) |
| 23 | Var-length rel `*` |
| 20 | Quantifier predicates |
| 18 | DELETE/DETACH DELETE |
| 18 | List functions |
| 18 | String functions |
| 15 | Parameter $param |
| 14 | ORDER BY |

88 scenarios have 0 detected blockers (harness/comparison issues).
52 skiplisted scenarios not matched to feature file.

## Skiplist

`skiplist.txt` lists 920 known-failing `Feature::Scenario` pairs.
The harness filters these out and exits non-zero only if a *non-skiplisted*
scenario fails — making the TCK a regression gate.

## History

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
