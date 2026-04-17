# TCK Conformance Status

Last updated: 2026-04-17

## Current pass rate

Full openCypher TCK vendored (220 feature files, commit `677cbaf`).

```
111 features parsed (1 parse error: Match5.feature)
857 total scenario instances running
785 passed
 72 skipped (cucumber-level)
1096 skiplisted (known failures)
 ────
785/1881 unique scenarios passing (41.7%)
```

## Pass rate by area

Regenerate with: `cargo test --test tck 2>&1 > /tmp/tck_output.txt && uv run tests/tck/analyze.py /tmp/tck_output.txt`

All running scenarios pass at 100%. The table below shows total scenarios
(running + skiplisted) per area:

| Area | Running | Skiplisted | Total | Notes |
|------|--------:|-----------:|------:|-------|
| Literals | 119 | 19 | 138 | Float exponent/leading-dot added |
| Match | 56 | 76 | 132 | Core pattern matching |
| Call | 39 | 2 | 41 | Procedures |
| List | 41 | 67 | 108 | IN in expr context |
| WithOrderBy | 9 | 102 | 111 | Sorting |
| String | 29 | 4 | 33 | |
| Create | 18 | 60 | 78 | Write-clause RETURN |
| Merge | 19 | 56 | 75 | Write-clause RETURN |
| Return | 23 | 40 | 63 | |
| Set | 3 | 50 | 53 | |
| Match (where) | 10 | 24 | 34 | |
| Delete | 7 | 34 | 41 | |
| Graph | 20 | 28 | 48 | Node/rel property access |
| TypeConversion | 8 | 39 | 47 | toInteger(), toFloat(), etc. |
| Comparison | 8 | 25 | 33 | Equality + ordering |
| Boolean | 11 | 25 | 36 | AND/OR/XOR/NOT in expr context |
| Quantifier | 4 | 96 | 100 | ALL/ANY/NONE predicates |
| Temporal | 0 | 89 | 89 | Date/time (not implemented) |
| Precedence | 5 | 38 | 43 | Operator precedence |
| Pattern | 3 | 33 | 36 | Pattern predicate |
| Remove | 0 | 33 | 33 | Property/label removal |
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
| 151 | 313 | Write-clause RETURN side effects | Side-effect delta tracking |
| 36 | 55 | Temporal types | datetime(), date(), duration() |
| 33 | 53 | Quantifier predicates | single(), none(), any(), all() |
| 23 | 37 | CREATE (no RETURN) side effects | |
| 10 | 21 | IN [list] | Null semantics |
| 9 | 31 | IS NULL / IS NOT NULL | Property access |
| 9 | 16 | Parameter $param | |
| 8 | 24 | ORDER BY | WITH...ORDER BY |
| 7 | 19 | String functions | |
| 6 | 35 | Aggregation (non-count) | sum/avg/min/max/collect |
| 6 | 19 | List functions | range(), reverse(), tail() |
| 5 | 34 | MERGE | |

## Skiplist

`skiplist.txt` lists 1096 known-failing `Feature::Scenario` pairs.
The harness filters these out and exits non-zero only if a *non-skiplisted*
scenario fails — making the TCK a regression gate.

## History

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
