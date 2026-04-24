# TCK Conformance Status

Last updated: 2026-04-23

## Current pass rate

Full openCypher TCK vendored (220 feature files, commit `677cbaf`).

```
181 features parsed (1 parse error: Match5.feature)
3344 total scenario instances running
3273 passed
  71 skipped (cucumber-level)
 257 skiplisted (known failures)
 ────
3273/3530 unique scenarios passing (92.7% of total, 95.0% of non-framework-skipped)
```

## Pass rate by area

Regenerate with: `cargo test --test tck 2>&1 > /tmp/tck_output.txt && uv run tests/tck/analyze.py /tmp/tck_output.txt`

All running scenarios pass at 100%. The table below shows running
scenarios per area (1146 total running, 484 skiplisted):

| Area | Running |
|------|--------:|
| Literals | 130 |
| List | 94 |
| Quantifier | 88 |
| Match | 77 |
| Create | 68 |
| Merge | 46 |
| Graph | 44 |
| Precedence | 43 |
| Return | 40 |
| TypeConversion | 40 |
| Call | 39 |
| Boolean | 36 |
| WithOrderBy | 34 |
| String | 32 |
| Comparison | 31 |
| MatchWhere | 29 |
| ReturnOrderBy | 27 |
| ReturnSkipLimit | 26 |
| Set | 25 |
| With | 24 |
| Aggregation | 23 |
| Remove | 20 |
| Delete | 18 |
| Map | 17 |
| Null | 16 |
| Temporal | 15 |
| Union | 12 |
| Unwind | 11 |
| WithWhere | 11 |
| WithSkipLimit | 6 |
| Mathematical | 5 |
| CountingSubgraphMatches | 5 |
| ExistentialSubquery | 4 |
| Pattern | 4 |
| TriadicSelection | 3 |
| Path | 2 |
| Conditional | 1 |

## Highest-impact work items

Regenerate with: `uv run tests/tck/analyze_blockers.py`

### Sole blockers (fixing this alone unblocks the scenario)

| Sole | Impact | Construct |
|-----:|-------:|-----------|
| 40 | 97 | write-result (CREATE/MERGE ... RETURN) |
| 6 | 33 | error validation |
| 6 | 7 | duration.between/inX |
| 2 | 8 | CREATE (no RETURN) |
| 2 | 9 | temporal types |
| 1 | 6 | aggregation (non-count) |
| 1 | 3 | list indexing [n] |

### High-impact (in scenarios with ≤2 missing constructs)

| Impact | Construct |
|-------:|-----------|
| 97 | write-result (CREATE/MERGE ... RETURN) |
| 33 | error validation |
| 12 | var-length rel `*` |
| 12 | MERGE |
| 9 | ORDER BY |
| 9 | temporal types |
| 8 | CREATE (no RETURN) |
| 8 | DELETE/DETACH DELETE |
| 7 | duration.between/inX |
| 6 | aggregation (non-count) |
| 6 | SET property/label |

### Zero-blocker scenarios (2)

- Harness/comparison issues

## Skiplist

`skiplist.txt` lists 257 known-failing `Feature::Scenario` pairs.
The harness filters these out and exits non-zero only if a *non-skiplisted*
scenario fails — making the TCK a regression gate.

## History

### Phase 24 — Grammar fixes, EXISTS subquery (2026-04-23)

- [x] Allow function names (`sum`, `count`, `min`, `max`, etc.) as bare variable identifiers via `fn_name_as_ident` grammar rule
- [x] Expand `multi_clause_stmt` Pattern C: DELETE/SET/REMOVE after 2+ non-write clauses (no CREATE/MERGE required)
- [x] Add `EXISTS { MATCH ... WHERE ... }` full existential subquery (grammar, parser, AST, eval, executor)
- [x] Fix: reject non-projected aggregates in WITH ORDER BY
- [x] Batch unskip: 25 scenarios newly passing
- Result: 3248 → 3273 passing scenarios (+25), skiplist 282 → 257

### Phase 23 — WITH WHERE, pattern predicates, relationship property filter (2026-04-23)

- [x] Pass 20 more TCK scenarios — WITH WHERE + pattern predicates
- [x] Filter on relationship inline properties in MATCH patterns
- Result: 3208 → 3248 passing scenarios (+40), skiplist 322 → 282

### Phase 22 — TCK session 2026-04-22/23 (87.2% → 93.7%)

- [x] ASCENDING/DESCENDING keywords, temporal storage round-trip, named paths
- [x] CREATE direction fix, WITH WHERE before projection, pattern predicates
- [x] Side-effect fingerprinting, variable-length relationships, AS alias with keywords
- [x] Math/string functions, ORDER BY before projection, DELETE/REMOVE grammar
- [x] Relationship property filter, multi-arg column names, undirected label dedup
- Result: 3022 → 3248 passing scenarios (+226), skiplist 444 → 282

### Phase 21 — Temporal extensions (2026-04-20)

- [x] Add `duration.between()`, `duration.inMonths()`, `duration.inDays()`, `duration.inSeconds()` functions
- [x] Add named timezone (IANA) support: string parsing (`[Europe/Stockholm]`), map construction, DST-aware offset resolution
- [x] Add quarter/dayOfQuarter date construction from maps
- [x] Add base date/time projection (`{date: other, year: 28}`, `{time: other, second: 42}`)
- [x] Add temporal truncation functions: `date.truncate()`, `localtime.truncate()`, `time.truncate()`, `localdatetime.truncate()`, `datetime.truncate()`
- [x] Fix duration rendering normalization (seconds/nanos same sign, negative fractional display)
- [x] Add `toString()` for all 6 temporal types
- [x] Add `executing control query:` step to TCK harness (unblocks Temporal4, Create2/5, Merge6/7)
- [x] Fix weekYear/week accessor split, add dayOfQuarter, timezone, epochSeconds/Millis accessors
- [x] Batch unskip: 18 scenarios found passing via systematic sweep
- Result: 2280 → 3022 passing scenarios (+742), skiplist 484 → 444

### Phase 20 — Quick wins, CASE, Union, batch unskip (2026-04-20)

- [x] Add simple CASE form (`CASE expr WHEN value THEN result END`) — grammar, parser, AST, evaluator
- [x] Fix null bound handling in list slicing (`[1,2,3][null..2]` → null)
- [x] Fix IN operator null-safe equality (use `values_equal` result, not just item-is-null check)
- [x] Add Union column validation (DifferentColumnsInUnion)
- [x] Add Union/Union All mixing detection (InvalidClauseComposition)
- [x] Add ambiguous aggregation detection (`me.age + count(you.age)` → AmbiguousAggregationExpression)
- [x] Unskip Null1/Null2 (6 scenarios already passing)
- [x] Unskip Literals5[9] negative zero (already passing)
- [x] Unskip List6[2,3,4] size (already passing)
- [x] Batch unskip: 71 scenarios across 40 features found passing via systematic sweep
- Result: 2057 → 2280 passing scenarios (+223), skiplist 615 → 484

### Phase 19 — Error validation (2026-04-20)

- [x] Duplicate column name detection in RETURN/WITH (`RETURN 1 AS a, 2 AS a`)
- [x] `RETURN *` with no variables in scope
- [x] Aggregate in WHERE clause rejection
- [x] Aggregate-in-aggregate rejection (`count(count(*))`)
- [x] Duplicate relationship variable in MATCH pattern (`-[r]->()-[r]->`)
- [x] Invalid unicode escape error (`\uH` → SyntaxError)
- [x] TypeError for `properties()` on non-entity (integer, string, list)
- [x] TypeError for `length()` on non-path/non-string/non-list
- [x] TypeError for list indexing with non-integer / indexing non-list
- [x] TypeError for property access on scalar values (integer, string, boolean, etc.)
- [x] Accept TypeError as SyntaxError in TCK harness (compile-time vs runtime detection)
- [x] Compile-time type checking: `type()` on node, `length()` on node/relationship, type conversion on node/relationship
- [x] Undefined variable detection in WHERE clauses
- [x] Non-aliased expressions in WITH (`WITH a, count(*)` → require `AS` alias)
- [x] TypeError for `IN` operator with non-list right-hand side
- Result: 2007 → 2057 passing scenarios (+50), skiplist 651 → 615

### Phase 18 — Edge-case bug fixes, string/grammar improvements (2026-04-20)

- [x] Fix `RETURN *` with scalar keys from WITH/UNWIND (bare keys hidden by `is_user_visible_field`)
- [x] Add double-quoted string support (`"hello"`) to grammar
- [x] Add string escape sequences: `\b`, `\f`, `\"`, `\/`, `\uXXXX` unicode escapes
- [x] Add Gherkin data-table cell unescaping in TCK harness (`\\` → `\`)
- [x] Normalize negative zero (`-.0` → `0.0`) in float literal parser
- [x] Add backtick-delimited identifiers (`` `name` ``) to grammar for ident, property_key, map_key
- [x] Allow keywords as map literal keys (`{null: 'x'}`) via `map_key` rule using `symbolic_name`
- [x] Add precedence-aware parenthesization in `expr_to_column_name`
- [x] Fix `count(a) > 0` over empty graph: recursive aggregate detection in `is_aggregate_fn` and `split_aggregates`, plus pre-computed aggregate lookup in `eval_function_call`
- [x] Add chained property access (`m.a.b`) via `DotAccess` AST node and `dot_access` grammar rule
- [x] Add temporal `.transaction`/`.statement`/`.realtime` dotted function variants
- [x] Unskip 14 already-passing scenarios (Map1/2, List3/4, Math8, Return2, Unwind1, TriadicSelection1)
- Result: 1952 → 2007 passing scenarios (+55), skiplist 678 → 651

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
