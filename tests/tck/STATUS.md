# TCK Conformance Status

Last updated: 2026-04-19

## Current pass rate

Full openCypher TCK vendored (220 feature files, commit `677cbaf`).

```
129 features parsed (1 parse error: Match5.feature)
1485 total scenario instances running
1404 passed
  81 skipped (cucumber-level)
 929 skiplisted (known failures)
 ────
1404/2414 unique scenarios passing (58.2%)
```

## Pass rate by area

Regenerate with: `cargo test --test tck 2>&1 > /tmp/tck_output.txt && uv run tests/tck/analyze.py /tmp/tck_output.txt`

All running scenarios pass at 100%. The table below shows total scenarios
(running + skiplisted) per area:

| Area | Running | Skiplisted | Total | Notes |
|------|--------:|-----------:|------:|-------|
| Literals | 122 | 19 | 141 | Float exponent/leading-dot added |
| Match | 56 | 76 | 132 | Core pattern matching |
| Quantifier | 59 | 41 | 100 | none/single/any/all implemented |
| Call | 39 | 16 | 55 | Procedures |
| List | 46 | 62 | 108 | IN in expr context |
| WithOrderBy | 10 | 101 | 111 | Sorting |
| String | 29 | 4 | 33 | |
| Create | 42 | 36 | 78 | Labels-as-keywords fixed |
| Merge | 25 | 50 | 75 | Write-clause RETURN |
| Return | 25 | 38 | 63 | |
| Set | 3 | 50 | 53 | |
| Match (where) | 10 | 24 | 34 | |
| Delete | 7 | 34 | 41 | |
| Remove | 21 | 13 | 34 | Property/label removal |
| Graph | 21 | 27 | 48 | labels() added |
| TypeConversion | 9 | 38 | 47 | toInteger(), toFloat(), etc. |
| Comparison | 10 | 23 | 33 | Equality + ordering |
| Boolean | 11 | 25 | 36 | AND/OR/XOR/NOT in expr context |
| Temporal | 0 | 89 | 89 | Date/time (not implemented) |
| Precedence | 6 | 37 | 43 | Operator precedence |
| Pattern | 3 | 33 | 36 | Pattern predicate |
| ReturnSkipLimit | 24 | 7 | 31 | Pagination |
| ReturnOrderBy | 21 | 14 | 35 | |
| Aggregation | 1 | 26 | 27 | |
| With | 15 | 14 | 29 | |
| TriadicSelection | 0 | 19 | 19 | Needs named graphs + var-length |
| Map | 3 | 16 | 19 | |
| WithWhere | 4 | 15 | 19 | |
| Null | 7 | 9 | 16 | IS NULL / IS NOT NULL |
| Unwind | 5 | 9 | 14 | |
| Union | 8 | 4 | 12 | |
| CountingSubgraphMatches | 4 | 7 | 11 | |
| ExistentialSubquery | 4 | 6 | 10 | |
| WithSkipLimit | 4 | 5 | 9 | |
| Path | 0 | 7 | 7 | Path expressions |
| Mathematical | 2 | 4 | 6 | Modulo added |
| Conditional | 0 | 2 | 2 | CASE expressions |

## Highest-impact work items

Regenerate with: `uv run tests/tck/analyze_blockers.py`

| Sole | Impact | Construct | Notes |
|-----:|-------:|-----------|-------|
| 141 | 298 | Write-clause RETURN side effects | Side-effect delta tracking |
| 36 | 55 | Temporal types | datetime(), date(), duration() |
| 13 | 27 | CREATE (no RETURN) side effects | |
| 13 | 20 | Quantifier predicates (remaining) | Map/node/rel list items, invariants |
| 10 | 12 | IN [list] | Null semantics |
| 9 | 26 | IS NULL / IS NOT NULL | Property access |
| 9 | 16 | Parameter $param | |
| 8 | 24 | ORDER BY | WITH...ORDER BY |
| 6 | 34 | Aggregation (non-count) | sum/avg/min/max/collect |
| 6 | 18 | List functions | range(), reverse(), tail() |
| 6 | 18 | String functions | |
| 5 | 33 | MERGE | |

## Skiplist

`skiplist.txt` lists 974 known-failing `Feature::Scenario` pairs.
The harness filters these out and exits non-zero only if a *non-skiplisted*
scenario fails — making the TCK a regression gate.

## History

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
