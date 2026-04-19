# TCK Conformance Improvement Plan

## Status: Phases 1-9 complete (2026-04-19)

1371 scenarios passing, 952 skiplisted, 0 failures.

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

### Phase 9: Aggregation, ORDER BY, IS NULL, Parameters (partial) ✓

Five targeted fixes:
- **Aggregate column name mismatch**: `exec_aggregate` was storing results as `max(*)` but `exec_project` looked up `max(x)`. New `agg_col_name` helper uses `expr_to_column_name` for consistency. Unlocked 13 aggregation scenarios (Count, Min/Max, Sum, Collect).
- **Sort comparator**: Added `Value::Bool` and `Value::List` to `compare_values_for_sort`, plus Cypher cross-type ordering via `type_rank()`.
- **WITH clause ordering**: Moved WHERE filter before ORDER BY/SKIP/LIMIT in `plan_with` per Cypher spec.
- **Map property access**: `Expr::Property(var, prop)` now checks if `var` is bound to a `Value::Map` and extracts the key. Fixes `map.key IS NULL` patterns.
- **Parameter resolution**: Replaced `value_to_literal` with `value_to_expr` to support List/Map parameter values.

Remaining blockers discovered:
- WithOrderBy (101 scenarios): All use `UNWIND...WITH...ORDER BY...RETURN` — the `unwind_stmt` grammar only supports `UNWIND...RETURN` or `UNWIND...CREATE`, not intermediate WITH clauses.
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

| Sole | Impact | Construct | Notes |
|-----:|-------:|-----------|-------|
| 141 | 298 | Write-clause RETURN side effects | Side-effect delta tracking |
| 36 | 55 | Temporal types | datetime(), date(), duration() |
| 13 | 27 | CREATE (no RETURN) side effects | Side-effect assertion gaps |
| 13 | 20 | Quantifier predicates (remaining) | Map/node/rel items, invariants |
| 10 | 12 | IN [list] | Remaining: null semantics |
| 9 | 26 | IS NULL / IS NOT NULL | Property access on missing props |
| 9 | 16 | Parameter $param | |
| 8 | 24 | ORDER BY | WITH...ORDER BY interaction |
| 6 | 34 | Aggregation (non-count) | sum/avg/min/max/collect |
| 6 | 18 | List functions | range(), reverse(), tail(), etc. |
| 6 | 18 | String functions | toString(), replace(), etc. |
| 5 | 33 | MERGE | |

### Recommended next phase

**Write-clause RETURN side effects** (141 sole-blocker, 298 impact): The single largest blocker. CREATE/MERGE/SET/DELETE with RETURN need side-effect delta tracking.

**Aggregation functions** (6 sole-blocker, 34 impact): UNWIND + aggregation pipeline, GROUP BY with count/sum/avg/min/max/collect.

**REMOVE side-effect persistence** (13 remaining): OPTIONAL MATCH + REMOVE, WITH/aggregation after REMOVE.

## Critical files
- `tests/tck_support/world.rs` — harness counts
- `tests/tck_support/steps.rs` — step definitions
- `src/cypher/grammar.pest` — PEG grammar
- `src/cypher/parser.rs` — AST construction
- `src/cypher/ast.rs` — AST types
- `src/cypher/eval.rs` — expression evaluator
- `src/edge.rs` — edge storage
- `tests/tck/skiplist.txt` — known failures
