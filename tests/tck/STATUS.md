# TCK Conformance Status

Last updated: 2026-04-17

## Current pass rate

Full openCypher TCK vendored (220 feature files, commit `677cbaf`).

```
191 features parsed (1 parse error: Match5.feature)
3868 total scenario instances (incl. outline expansions)
1630 unique Feature::Scenario pairs
  414 passed
 1216 skiplisted (known failures)
 ────
  414/1630 unique scenarios passing (25.4%)
```

## Pass rate by area

Regenerate with: `cargo test --test tck 2>&1 > /tmp/tck_output.txt && uv run tests/tck/analyze.py /tmp/tck_output.txt`

| Area | Skipped | Total | Pass% | Notes |
|------|--------:|------:|------:|-------|
| Call | 2 | 41 | 95% | Procedures |
| String | 4 | 32 | 88% | |
| ReturnSkipLimit | 7 | 31 | 77% | Pagination |
| Union | 4 | 12 | 67% | |
| ReturnOrderBy | 14 | 35 | 60% | |
| Literals | 55 | 131 | 58% | |
| With | 14 | 29 | 52% | |
| WithSkipLimit | 5 | 9 | 44% | |
| Match | 76 | 132 | 42% | Core pattern matching |
| Graph | 28 | 48 | 42% | Node/rel property access |
| ExistentialSubquery | 6 | 10 | 40% | |
| Return | 40 | 63 | 37% | |
| CountingSubgraphMatches | 7 | 11 | 36% | |
| Unwind | 9 | 14 | 36% | |
| MatchWhere | 24 | 34 | 29% | |
| WithWhere | 15 | 19 | 21% | |
| TypeConversion | 39 | 47 | 17% | toInteger(), toFloat(), etc. |
| Mathematical | 5 | 6 | 17% | |
| Map | 16 | 19 | 16% | |
| Delete | 35 | 41 | 15% | |
| Create | 67 | 78 | 14% | |
| Pattern | 31 | 36 | 14% | |
| Comparison | 29 | 33 | 12% | |
| Merge | 66 | 75 | 12% | |
| Boolean | 32 | 36 | 11% | |
| WithOrderBy | 102 | 111 | 8% | Sorting |
| Set | 50 | 53 | 6% | |
| List | 103 | 108 | 5% | |
| Quantifier | 96 | 100 | 4% | ALL/ANY/NONE predicates |
| Aggregation | 26 | 27 | 4% | |
| Precedence | 43 | 43 | 0% | Operator precedence |
| Temporal | 89 | 89 | 0% | Date/time |
| TriadicSelection | 19 | 19 | 0% | Needs named graphs + var-length |
| Remove | 33 | 33 | 0% | Property/label removal |
| Path | 7 | 7 | 0% | Path expressions |
| Null | 16 | 16 | 0% | |
| Conditional | 2 | 2 | 0% | CASE expressions |

## Highest-impact work items

Regenerate with: `uv run tests/tck/analyze_blockers.py`

Ranked by how many skiplisted scenarios each construct *solely* blocks
(fixing it alone would make the scenario pass). "Impact" column shows
scenarios unblocked when combined with at most one other fix.

| Sole | Impact | Construct | Notes |
|-----:|-------:|-----------|-------|
| 151 | 321 | Write-clause RETURN (CREATE/MERGE ... RETURN) | #1 lever — parser/planner |
| 44 | 56 | `IN [list]` expression | WHERE x IN [...] |
| 36 | 55 | Temporal types | datetime(), date(), duration(), etc. |
| 33 | 53 | Quantifier predicates | single(), none(), any(), all() |
| 29 | 44 | CREATE (no RETURN, result handling) | Writes return empty vs rows |
| 23 | 24 | Hex/octal integer literals | 0xFF, 0o77 |
| — | 46 | OPTIONAL MATCH | Always paired with other gaps |
| — | 39 | MERGE clause | |
| — | 35 | Aggregation (sum/avg/min/max/collect) | Beyond count() |
| — | 34 | IS NULL / IS NOT NULL | |
| — | 31 | SET property/label | |
| 8 | 24 | ORDER BY | |
| 8 | 11 | XOR operator | |
| 8 | 9 | List slicing `[a..b]` | |
| 7 | 20 | List functions | range(), reverse(), tail(), head(), etc. |
| 7 | 19 | String functions | toString(), toInteger(), replace(), etc. |
| — | 23 | Variable-length relationships `[*]` | |
| — | 19 | DELETE / DETACH DELETE | |
| — | 16 | Parameter `$param` support | |

125 skiplisted scenarios have no detected missing construct — these are
likely harness comparison bugs or subtle execution-order issues.

## Failure modes (2964 failing steps)

| Count | Root cause |
|------:|------------|
| 2451 | Parser rejects valid Cypher (87% of failures) |
| 167 | Wrong result values (execution logic) |
| 121 | Parse error (missing grammar production) |
| 115 | Expected error not raised (missing validation) |
| 21 | Write clause returns rows instead of empty |
| 19 | Named graph loading (multi-statement CREATE) |
| 3 | Harness compare parser can't parse expected value |

## Skiplist

`skiplist.txt` lists 1216 known-failing `Feature::Scenario` pairs.
The harness filters these out and exits non-zero only if a *non-skiplisted*
scenario fails — making the TCK a regression gate.

## Phase 4 — scaling up (done)

- [x] Vendor the full `tck/features/` tree (220 feature files)
- [x] Vendor `tck/graphs/` (binary-tree-1, binary-tree-2)
- [x] Add `tests/tck/skiplist.txt` for known-failing scenarios
- [x] Flip exit status: harness fails on non-skiplisted regressions
- [x] Track pass-rate over time

## Phase 3 — pilot (complete)

Original 5 pilot feature files, 112 scenarios, 100% pass rate.
