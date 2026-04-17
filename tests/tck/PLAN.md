# TCK Conformance Improvement Plan

## Context

494/1630 scenarios passing (30.3%). Two harness bugs and one grammar limitation account for the bulk of the remaining failures. This plan targets low-risk, high-impact fixes first.

## Phase 1: Harness Bug Fixes (quick wins)

### 1A — Fix label counting in `GraphCounts::snapshot()`

**File:** `tests/tck_support/world.rs:31-38`

**Bug:** SQL queries `WHERE key LIKE '%_label_cnt'` but engine stores keys as `stats:label_count:<label>` (`src/stats.rs:8`). Labels always count as 0.

**Fix:** Change to `SELECT COUNT(*) FROM metadata WHERE key LIKE 'stats:label_count:%'`. No substr needed — each label is one row.

### 1B — Fix relationship counting in `GraphCounts::snapshot()`

**File:** `tests/tck_support/world.rs:28-29`

**Bug:** Counts `SELECT COUNT(*) FROM edge_props`, but `src/edge.rs:58-64` only inserts into `edge_props` when properties are non-empty. Edges without properties (e.g., `CREATE ()-[:R]->()`) are invisible.

**Fix:** In `src/edge.rs:58`, remove the `if !properties.is_empty()` guard — always store an `edge_props` row. Same for `batch_create_edges`. Empty-map msgpack is 1 byte, negligible cost.

### 1C — Unskip passing IS NULL scenarios

After 1A, remove Null1::[4] (`RETURN null IS NULL AS value`) and Null2::[4] from skiplist — these are pure RETURN + IS NULL, already fully implemented in parser/eval.

**Verify:** `cargo test --test tck` after each sub-phase.

---

## Phase 2: Unify expression grammar (biggest single unlock)

**Files:** `src/cypher/grammar.pest`, `src/cypher/parser.rs`

**Problem:** `expr = { in_expr }` — no boolean ops, comparisons, IS NULL, or NOT available in RETURN/WITH/UNWIND/CASE/list contexts. `bool_expr` has the full tower but is only used in WHERE.

**Fix:** Merge `bool_expr` into `expr`:
```
expr = { xor_term ~ (or_op ~ xor_term)* }
xor_term = { bool_term ~ (xor_op ~ bool_term)* }
bool_term = { bool_factor ~ (and_op ~ bool_factor)* }
bool_factor = { not_op? ~ bool_primary }
bool_primary = { case_expr | exists_subquery | is_not_null_check | is_null_check | in_check | comparison | "(" ~ expr ~ ")" | in_expr }
```

Remove the separate `bool_expr` rule (or alias it to `expr`). Parser: wire `parse_expr()` through the bool chain instead of going straight to `parse_in_expr()`.

**Unlocks:** ~80-120 scenarios across Boolean (32), Precedence (28), Null (8), Comparison (20+), and many scattered scenarios that use boolean expressions in RETURN.

---

## Phase 3: Modulo operator

**Files:** `grammar.pest`, `ast.rs`, `parser.rs`, `eval.rs`

Add `%` to `mul_op`, `BinOp::Mod` variant, eval with null propagation and div-by-zero → Null.

**Unlocks:** ~5-15 scenarios (Mathematical, some Return2).

---

## Phase 4: Float literal improvements

**File:** `grammar.pest`

Current rule requires digits on both sides of `.` and no exponent. Add: `.5`, `1e9`, `1.0e5`, uppercase `E`, signed exponents.

**Unlocks:** ~10-18 Literals5 scenarios.

---

## Phase 5: Batch unskip and triage

After Phases 1-4, systematically:
1. Remove all IS NULL/IS NOT NULL scenarios from skiplist (those not blocked by OPTIONAL MATCH/maps)
2. Remove zero-blocker scenarios in batches of ~20, run tests, identify patterns
3. Update `skiplist.txt` and `STATUS.md`

---

## Execution order & risk

| # | Phase | Risk | Files Changed | Est. Unlocked |
|---|-------|------|---------------|---------------|
| 1 | 1A: Label count fix | None | world.rs | 4-10 |
| 2 | 1B: Relationship count fix | Low | edge.rs, world.rs | 5-15 |
| 3 | 1C: Unskip IS NULL trivial | None | skiplist.txt | 2-4 |
| 4 | 2: Grammar unification | Medium | grammar.pest, parser.rs | 80-120 |
| 5 | 3: Modulo operator | Low | grammar/ast/parser/eval | 5-15 |
| 6 | 4: Float literals | Low | grammar.pest | 10-18 |
| 7 | 5: Batch unskip | None | skiplist.txt | 10-30 |

**Estimated total:** 116-212 more scenarios → ~37-43% pass rate.

## Verification

After each phase:
```bash
cargo test --test tck 2>&1 | tail -20
```

After all phases:
```bash
cargo test --test tck 2>&1 > /tmp/tck_output.txt && uv run tests/tck/analyze.py /tmp/tck_output.txt
uv run tests/tck/analyze_blockers.py
```

## Critical files
- `tests/tck_support/world.rs` — harness counts
- `tests/tck_support/steps.rs` — step definitions
- `src/cypher/grammar.pest` — PEG grammar
- `src/cypher/parser.rs` — AST construction
- `src/cypher/ast.rs` — AST types
- `src/cypher/eval.rs` — expression evaluator
- `src/edge.rs` — edge storage
- `src/stats.rs` — label statistics
- `tests/tck/skiplist.txt` — known failures
