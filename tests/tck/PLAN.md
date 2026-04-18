# TCK Conformance Improvement Plan

## Status: Phases 1-6 complete (2026-04-17)

841 scenarios passing, 1051 skiplisted, 0 failures.

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

---

## Next priorities (by impact)

Regenerate with: `uv run tests/tck/analyze_blockers.py`

| Sole | Impact | Construct | Notes |
|-----:|-------:|-----------|-------|
| 141 | 299 | Write-clause RETURN side effects | Side-effect delta tracking |
| 36 | 55 | Temporal types | datetime(), date(), duration() |
| 33 | 53 | Quantifier predicates | single(), none(), any(), all() |
| 13 | 27 | CREATE (no RETURN) side effects | Side-effect assertion gaps |
| 10 | 21 | IN [list] | Remaining: null semantics |
| 9 | 30 | IS NULL / IS NOT NULL | Property access on missing props |
| 9 | 16 | Parameter $param | |
| 8 | 24 | ORDER BY | WITH...ORDER BY interaction |
| 6 | 34 | Aggregation (non-count) | sum/avg/min/max/collect |
| 6 | 19 | List functions | range(), reverse(), tail(), etc. |
| 6 | 18 | String functions | toString(), replace(), etc. |
| 5 | 33 | MERGE | |

### Recommended next phase

**Quantifier functions** (33 sole-blocker, 53 impact): Adding `all()`, `any()`, `none()`, `single()` as list predicate functions. Relatively self-contained — grammar rule, parser, eval.

**Aggregation functions** (6 sole-blocker, 34 impact): Extending beyond `count()` to `sum`, `avg`, `min`, `max`, `collect`.

**REMOVE statement** (33 scenarios): Grammar + AST + parser + planner + executor for property/label removal.

## Critical files
- `tests/tck_support/world.rs` — harness counts
- `tests/tck_support/steps.rs` — step definitions
- `src/cypher/grammar.pest` — PEG grammar
- `src/cypher/parser.rs` — AST construction
- `src/cypher/ast.rs` — AST types
- `src/cypher/eval.rs` — expression evaluator
- `src/edge.rs` — edge storage
- `tests/tck/skiplist.txt` — known failures
