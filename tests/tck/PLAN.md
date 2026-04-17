# TCK Conformance Improvement Plan

## Status: Phases 1-5 complete (2026-04-17)

785 scenarios passing, 1096 skiplisted, 0 failures.

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

---

## Next priorities (by impact)

Regenerate with: `uv run tests/tck/analyze_blockers.py`

| Sole | Impact | Construct | Notes |
|-----:|-------:|-----------|-------|
| 151 | 313 | Write-clause RETURN side effects | Side-effect delta tracking |
| 36 | 55 | Temporal types | datetime(), date(), duration() |
| 33 | 53 | Quantifier predicates | single(), none(), any(), all() |
| 23 | 37 | CREATE (no RETURN) side effects | Side-effect assertion gaps |
| 10 | 21 | IN [list] | Remaining: null semantics |
| 9 | 31 | IS NULL / IS NOT NULL | Property access on missing props |
| 9 | 16 | Parameter $param | |
| 8 | 24 | ORDER BY | WITH...ORDER BY interaction |
| 7 | 19 | String functions | toString(), replace(), etc. |
| 6 | 35 | Aggregation (non-count) | sum/avg/min/max/collect |
| 6 | 19 | List functions | range(), reverse(), tail(), etc. |
| 5 | 34 | MERGE | |
| 5 | 10 | NOT prefix | Null propagation in NOT |

### Recommended next phase

**Side-effect delta tracking** (151 sole-blocker scenarios): The `GraphCounts::delta()` method and step assertions need to accurately track `+nodes`, `-nodes`, `+labels`, `+properties`, `+relationships`, `-relationships`. This is the single biggest unlock.

**Quantifier functions** (33 sole-blocker, 53 impact): Adding `all()`, `any()`, `none()`, `single()` as list predicate functions.

**Aggregation functions** (6 sole-blocker, 35 impact): Extending beyond `count()` to `sum`, `avg`, `min`, `max`, `collect`.

## Critical files
- `tests/tck_support/world.rs` — harness counts
- `tests/tck_support/steps.rs` — step definitions
- `src/cypher/grammar.pest` — PEG grammar
- `src/cypher/parser.rs` — AST construction
- `src/cypher/ast.rs` — AST types
- `src/cypher/eval.rs` — expression evaluator
- `src/edge.rs` — edge storage
- `tests/tck/skiplist.txt` — known failures
