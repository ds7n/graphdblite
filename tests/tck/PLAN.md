# TCK Conformance Improvement Plan

## Status: Phases 1-17 complete (2026-04-19)

1952 scenarios passing (74.2%), 678 skiplisted, 0 failures.

---

## Completed Phases

### Phase 1: Harness Bug Fixes ✓

- **1A** — Fixed label counting SQL in `world.rs` (`'%_label_cnt'` → `'stats:label_count:%'`)
- **1B** — Always store `edge_props` rows in `edge.rs` (property-less edges were invisible to count)
- **1C** — IS NULL scenarios already unskipped in prior work

### Phase 2: Unified expression grammar ✓

Merged `bool_expr` into `expr` so AND/OR/XOR/NOT/comparisons/IS NULL/IN work in all expression contexts (RETURN, WITH, CASE, UNWIND, list literals, etc.).

Key implementation details:
- Restructured `bool_primary` as `cmp_or_value` to avoid PEG exponential backtracking
- Added `@` atomic word-boundary rules (`or_op`, `and_op`, `is_kw`, `in_kw`, etc.) to prevent keyword prefix matching (e.g. "OR" in "ORDER")
- `bool_expr` became thin wrapper `{ expr }` for backward compat with `where_clause`
- ~108 scenarios moved to skiplist (expect compile-time type checking we don't do)
- ~25 scenarios unlocked (boolean ops in RETURN context)

### Phase 3: Modulo operator ✓

Added `%` to `mul_op`, `BinOp::Mod`, eval with null propagation and div-by-zero → Null.

### Phase 4: Float literal improvements ✓

Extended `float_literal` grammar: leading dot (`.5`), exponents (`1e9`, `1.0E-5`), overflow detection (`1.34E999` → SyntaxError). ~19 Literals5 scenarios unlocked.

### Phase 5: Batch unskip ✓

Systematically removed skiplist entries for Boolean, Null, Comparison, Precedence, Literals5, and Mathematical scenarios. Re-added those that still fail due to deeper issues (null propagation, map/NaN comparisons, UNWIND interaction).

### Phase 6: Skiplist cleanup & targeted fixes ✓

Three code fixes and a systematic skiplist sweep:
- **symbolic_name rule**: Labels and relationship types now allow keywords (e.g. `CREATE (:End)`, `CREATE (:Not)`) via a new `symbolic_name` grammar rule used in `label_spec` and `rel_type_spec`. This exposed 4 previously-unparseable feature files.
- **Null property filtering**: `CREATE ({p: null})` no longer stores `p`. Added null check in `exec_create_sequence` and `exec_match_create` property loops.
- **CREATE dedup**: `CREATE (a), (a)-[:R]->(b)` no longer creates duplicate nodes. `plan_create_pattern` tracks `seen` named variables across patterns, skipping `CreateNode` for already-seen aliases.
- **Batch unskip**: Removed 45 confirmed-passing skiplist entries (Create1-6, Comparison2, List3, Literals7-8, Merge2-3-5-7, Precedence2, Return3-6, TypeConversion4, WithOrderBy3).

### Phase 17: Quantifier edge cases, rand(), CASE+operator fix ✓

- Added `rand()` function
- Fixed CASE WHEN in arithmetic (was short-circuiting in `cmp_primary`)
- Fixed WITH alias shadowing in projection (coalesce(x, y) AS x)
- Result: 1816 → 1952 passing (+136), skiplist 706 → 678

### Phase 16: TypeConversion + Graph functions ✓

- Added `toBoolean()`, fixed `toInteger()`/`toFloat()`/`toString()` TypeError handling
- Added `properties()`, `relationships()` functions
- Fixed `labels()`, `type()`, `keys()`, `id()` for compound Value types
- Added dynamic property access (`n['key']`)
- Added `with_stmt` grammar rule for standalone `WITH ... RETURN`
- Result: 1795 → 1816 passing (+21), skiplist 750 → 706

### Phase 15: Boolean/Comparison/Precedence overhaul ✓

- Restructured expression precedence hierarchy with exponentiation, chained comparisons
- Three-valued boolean type checking, NaN handling, boolean/list ordering
- Result: 1512 → 1795 passing (+283), skiplist 828 → 750

### Phase 14: Multi-clause CREATE/MERGE ✓

- `multi_clause_stmt` grammar and `MultiClauseStatement` AST
- Relationship variable binding in CREATE/MERGE edge operations
- CREATE validation (VariableAlreadyBound) and null property handling
- Result: 1468 → 1512 passing (+44), skiplist 873 → 828

### Phase 13: Aggregation DISTINCT + percentile/stdev ✓

- Per-function DISTINCT, `percentileDisc`, `percentileCont`, `stDev`, `stDevP`
- Result: 1452 → 1468 passing (+16), skiplist 881 → 873

### Phase 12: OPTIONAL MATCH + SET extensions ✓

- OPTIONAL MATCH null handling (property access, LeftOuterJoin, MaterializePath)
- `n:Label` predicate in WHERE, opt_filter semantics
- SET `n:Label`, `SET n = {map}`, `SET n += {map}` — full pipeline
- Result: 1413 → 1452 passing (+39), skiplist 920 → 881

### Phase 11: Write-clause RETURN support for SET and DELETE ✓

- Added optional RETURN clause to `set_stmt` and `delete_stmt` grammar rules
- Extended `SetStatement`/`DeleteStatement` AST with return_clause, order_by, skip, limit, optional_patterns
- Executor: `exec_set_property` returns modified records with updated properties; `exec_delete` returns input records
- Planner: `apply_return_projection()` for SET/DELETE; optional_patterns LeftOuterJoin for SET
- Result: 1404 → 1413 passing (+9), skiplist 929 → 920

### Phase 10: Temporal sorting/arithmetic, UNWIND...CREATE...WITH ✓

- Temporal types in `compare_values_for_sort` (Date, LocalTime, Time, DateTime, Duration)
- Temporal arithmetic: Date/Time/DateTime ± Duration, Duration ± Duration, Duration × Number
- Extended `unwind_create` grammar for intermediate WITH/MATCH/UNWIND clauses
- Planner support for intermediate clauses in UnwindBody::Create
- Result: 1393 → 1404 passing (+11), skiplist 940 → 929

### Phase 9: Aggregation, ORDER BY, IS NULL, Parameters (partial) ✓

Five targeted fixes:
- **Aggregate column name mismatch**: `exec_aggregate` was storing results as `max(*)` but `exec_project` looked up `max(x)`. New `agg_col_name` helper uses `expr_to_column_name` for consistency. Unlocked 13 aggregation scenarios (Count, Min/Max, Sum, Collect).
- **Sort comparator**: Added `Value::Bool` and `Value::List` to `compare_values_for_sort`, plus Cypher cross-type ordering via `type_rank()`.
- **WITH clause ordering**: Moved WHERE filter before ORDER BY/SKIP/LIMIT in `plan_with` per Cypher spec.
- **Map property access**: `Expr::Property(var, prop)` now checks if `var` is bound to a `Value::Map` and extracts the key. Fixes `map.key IS NULL` patterns.
- **Parameter resolution**: Replaced `value_to_literal` with `value_to_expr` to support List/Map parameter values.

- **UNWIND...WITH grammar extension**: Extended `unwind_return` grammar rule to support intermediate WITH/MATCH/UNWIND clauses before RETURN, mirroring `match_stmt`. Updated planner to apply intermediate clauses in `plan_unwind`. Unlocked 12 WithOrderBy scenarios (boolean, integer, float, string sorting).

Remaining blockers:
- WithOrderBy (89 scenarios): Need temporal sorting, mixed-type sorting, aggregation in WITH context, node/relationship sorting.
- Parameters: Most scenarios need `n[$param]` dynamic property access (subscript on node/map with parameter index), which is a deeper eval issue.
- IS NULL: Remaining scenarios involve OPTIONAL MATCH null propagation edge cases.

### Phase 8: Temporal types ✓

Full temporal type system: 6 wrapper types in `src/temporal.rs` (CypherDate, CypherLocalTime, CypherTime, CypherLocalDateTime, CypherDateTime, CypherDuration). ISO 8601 parsing (calendar, week, ordinal dates; colon/compact time formats), map construction, Display, Serialize/Deserialize for MessagePack storage. Value enum extended with 6 temporal variants. Temporal constructor functions (date, localtime, time, localdatetime, datetime, duration) with string and map dispatch. Dotted function grammar rule for `datetime.fromepoch`/`datetime.fromepochmillis`. Temporal component accessors (d.year, t.hour, etc.). Comparison operators for temporal types. TCK harness updated for temporal-vs-string comparison.

8 temporal scenarios pass (Temporal2 string parsing for 5 basic types + 3 from new feature files). 81 remain skiplisted (complex map construction with week/ordinal dates, named timezones, storage, rendering, arithmetic, duration computation, truncation).

### Phase 7: Quantifier functions, REMOVE statement, labels() ✓

Three features and a batch unskip:

- **Quantifier predicates**: `none()`, `single()`, `any()`, `all()` with `(x IN list WHERE pred)` syntax. New `quantifier_expr` grammar rule placed before `function_call` in `atom_primary`. `QuantifierKind` enum and `Expr::Quantifier` AST variant. Evaluation uses three-valued null logic (tracks true/false/null counts per element). ~59 scenarios unlocked; 41 remain skiplisted (map/node/rel items, invariants needing `rand()` etc.).

- **REMOVE statement**: Full pipeline — grammar, AST (`RemoveStatement`, `RemoveItem::Property`/`Label`), parser, `LogicalOp::Remove` IR, planner, executor. `node::remove_node_label()` added. Records are updated in-place after removal so downstream RETURN sees correct values. Fixed `decrement_label_count` to delete stat entry when count reaches 0 (was leaving stale entries). 21 scenarios unlocked; 13 remain (OPTIONAL MATCH + REMOVE, WITH/aggregation side effects).

- **`labels()` function**: Added to grammar `function_name` and `eval_function_call`. Reads from `__labels` record field or falls back to database lookup.

- **Batch unskip**: 77 scenarios removed from skiplist total.

---

## Next priorities (by impact)

Regenerate with: `uv run tests/tck/analyze_blockers.py`

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
| 4 | 6 | pattern comprehension |
| 3 | 13 | CREATE (no RETURN) |
| 1 | 18 | DELETE/DETACH DELETE |
| 1 | 18 | SET property/label |
| 1 | 14 | aggregation (non-count) |

### Recommended next phases

**Phase 18: Temporal types (remaining 30 sole-blockers, 49 impact)**
Map construction with week/ordinal dates, named timezones, storage round-trip,
rendering, duration computation, truncation. Extends Phase 8 work.

**Phase 19: WithOrderBy/ORDER BY (74 skiplisted, 14 sole-blockers)**
ORDER BY in WITH clauses — temporal sorting, mixed-type sorting, aggregation
in WITH context, node/relationship sorting.

**Phase 20: List operations (49 skiplisted)**
List slicing [a..b], list indexing error handling, list functions
(range, reverse, tail), list/pattern comprehension.

**Phase 21: Remaining write-result / side-effect issues (113 sole-blockers)**
Many are harness categorization artifacts. Remaining real blockers:
ON CREATE/ON MATCH SET with labels, MERGE path binding, snapshot isolation,
multi-clause aggregation.

**Phase 22: Pattern features (33 skiplisted)**
Pattern comprehension, pattern predicates. Requires deeper planner work.

## Critical files
- `tests/tck_support/world.rs` — harness counts
- `tests/tck_support/steps.rs` — step definitions
- `src/cypher/grammar.pest` — PEG grammar
- `src/cypher/parser.rs` — AST construction
- `src/cypher/ast.rs` — AST types
- `src/cypher/eval.rs` — expression evaluator
- `src/edge.rs` — edge storage
- `tests/tck/skiplist.txt` — known failures
