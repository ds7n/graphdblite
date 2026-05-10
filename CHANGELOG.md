# Changelog

All notable changes to graphdblite are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/) once a stable release
ships.

## [Unreleased]

### Added
- Database files now stamp `application_id = 0x4744424C` ("GDBL") and
  `user_version = 1` in the SQLite header. `file(1)` reports the ID;
  `Database::open` rejects files whose `application_id` is non-zero and
  doesn't match (catches "I pointed graphdblite at a foreign SQLite file"
  mistakes), and rejects files whose `user_version` is newer than this
  build supports (forward-compat hardening). See
  `docs/sqlite-application-id.md` for upstream `magic.txt` registration
  procedure and `docs/sqlite-magic.patch` for the prepared diff.
- Bench infrastructure: `.github/workflows/bench.yml` (manual) captures a
  criterion baseline on consistent runner hardware and uploads it as an
  artifact. `scripts/bench-compare.sh` runs the suite locally against a
  named baseline and exits non-zero on regressions over a threshold
  (default 15%); requires `critcmp`, no-ops cleanly without it.
- `cargo fuzz` targets (`fuzz/`): `parse` and `parse_and_plan`. Hidden
  `__fuzz` module behind the `fuzzing` feature exposes the internal
  entry points without affecting the public API.
- CI: `test` job runs on ubuntu/macOS/windows (`fail-fast: false`,
  `cargo test --workspace --locked`); fmt+clippy stay Ubuntu-only.
- CLI: `-j`/`--json` flag for NDJSON output (one JSON object per row) and
  `-V`/`--version`. REPL gained `.mode table|json` dot-commands and rejects
  unknown `.commands` instead of forwarding them to the executor. Nodes,
  edges, and paths serialize with a `__type` discriminator; temporal values
  emit ISO-8601 strings.
- Node binding `withWriteTx`/`withReadTx` callback API with `Symbol.dispose`
  support.
- Auto-begin/auto-commit on `Database::execute` when no transaction is active
  (multi-statement transactions still need explicit `begin_*`/`commit`).
- `SET r = $map` and `SET r += $map` on relationships.
- Binding conformance suite (BC-01..BC-05) covering Python, Node, Go, and C
  bindings; read-only transaction enforcement.
- `Config::max_traversal_depth` and `Config::max_traversal_work` runtime caps.
- Error hint system across `GraphError` variants; close-name suggestions on
  `UndefinedVariable`.
- CALL procedure support; list-order comparison; var-length limit pushdown.
- 100% openCypher TCK conformance (3895/3895 scenarios pass).
- MSRV declared: Rust 1.82 (required by `Option::is_none_or` in `planner.rs`).
- Per-`Database` parsed-AST cache (`cypher::parse_cache`). Repeated
  `db.execute(cypher)` / `tx.query(cypher)` calls with the same query
  string skip the pest parse, going straight to plan + execute. Bounded
  FIFO (default capacity 128); caches the AST, not the plan, so it
  needs no invalidation on `CREATE INDEX`/`DROP INDEX` and remains
  parameter-agnostic. Profile-driven (samply against
  `cargo bench --bench cypher`) — pest accounted for ~49% cumulative
  time on hot lookups. Bench impact: `traversal/one_hop` -63%; simpler
  workloads marginal as expected.
- Per-`Database` plan cache (`cypher::plan_cache`). Repeated executes
  reuse the planned `LogicalOp` tree, skipping the planner entirely on
  hits. Built on a param-agnostic planner: `IndexLookup` carries
  `LookupKey::Param(name)` (resolved at exec time via the
  `eval::ParamScope` thread-local) instead of baking literals into the
  plan, so `WHERE n.prop = $x` reuses one plan across every `$x`
  value. Bounded FIFO (capacity 128) keyed on `(cypher,
  schema_epoch)`; the epoch is bumped atomically on `CREATE INDEX` /
  `DROP INDEX` so DDL lazily invalidates entries. Caching is skipped
  for CALL (procedure-registry sensitive at plan time) and for
  SKIP/LIMIT containing `$param` (planner evaluates these to a `u64`
  at plan time). Alloc impact: `indexed_lookup` -21.7%, `var_length`
  -7.6%.

### Changed
- **Relicensed from MPL-2.0 to MIT.** Updated workspace + binding manifests
  (`Cargo.toml`, `benches-crate/Cargo.toml`, `bindings/node/package.json`),
  `LICENSE`, and README. Dropped MPL-2.0 from the `cargo deny` allow-list
  (no transitive dep uses it).
- Public API lockdown (phases 1–6): `cypher::*`, `storage`, `index`, `node`,
  `edge`, `temporal` are now `pub(crate)`. The Cypher pipeline is reachable
  only via `Database::execute` or the typed `WriteTxGuard`/`ReadTxGuard`
  `query`/`execute` methods.
- AST `Expr` split into `kind` + `span` for source-located error messages.
- Integer arithmetic uses checked operations and surfaces clean overflow
  errors.
- `docs/architecture.md` describes the actual materialized-stage executor
  (with the iterator-shaped `iter.rs` fast path for correlated subqueries).
- Go binding module path corrected: `bindings/go/go.mod` now declares
  `github.com/ds7n/graphdblite/bindings/go` (matches README and
  `docs/go.md`; consumers `go get` the subdirectory module from the main
  repo).
- Parser error messages no longer leak pest grammar rule names. New
  `humanize_rule_name` entries cover `multi_create_clause`,
  `multi_merge_clause`, `multi_unwind_clause`, `multi_call_clause`,
  `multi_set_clause`, `multi_remove_clause`, `optional_match_clause`,
  `unwind_clause`, `match_clause`, `delete_clause`, and `symbolic_name`.
- Unresolved parameters now report `ErrorCode::MissingParameter` with a
  hint pointing to `execute_with_params` (was `ErrorCode::Other`).
- `#![deny(missing_docs)]` enabled at the crate root with crate-level
  rustdoc + a quick-start doc-test on `Database`. Variant-heavy enums
  (`Value`, `QueryError`, `GraphError`, `ErrorCode`, `Direction`,
  `SyncMode`) and identity structs (`Node`, `Edge`, `PathValue`, `Span`,
  `Record`) carry `#[allow(missing_docs)]` — documented at the type
  level, variant/field names are self-explanatory.

### Removed
- Record v2 Phase 6 cleanup. Removed the `record-v1`/`record-v2` Cargo
  features, the `executor::Path` enum and its `path` field on
  `ExecContext`, the `query_with_procedures_path` test-only entry point,
  the dual-run harness (`src/cypher/dual_run.rs`,
  `GRAPHDBLITE_DUAL_RUN`, `World::db_slot`, `assert_results_equivalent`),
  the `RowSink` trait (`src/cypher/row_sink.rs`), the planner anon-counter
  snapshot/restore helpers, and the dual `alloc_baseline.txt` /
  `alloc_baseline_v2.txt` split (single canonical baseline now). The slot
  path is the only path; TCK 3895/3895, alloc regression 0.0% drift.
- Dead `edge::batch_create_edges` helper (no callers post-binding migration)
  and its tests.
- Deprecated top-level `ProcedureRegistry` re-export (`procedures::Registry`
  is the only form).
- Unused `thiserror` dependency (flagged by `cargo udeps`, no in-tree
  references).

### Security
- Closed M2–M5 and L1–L5 audit findings.
- All `format!`+SQL sites validate identifiers via `validate_name`
  (`[A-Za-z0-9_]+` only).
- `Registry::register` `debug_assert!`s that procedure names match the
  grammar's `procedure_name` rule.
- `cargo audit`: 0 vulnerabilities across 246 dependencies (2026-05-05).
- `cargo deny check`: passes (advisories, bans, licenses, sources). License
  allow-list constrained to MPL-2.0, MIT, Apache-2.0, BSD-2-Clause, Unicode-3.0;
  unknown registries and git sources denied. Wired into `scripts/check.sh`
  and `.github/workflows/audit.yml`.
- `STABILITY.md` now documents the path-traversal contract for
  `Database::open` (paths are passed directly to SQLite; bindings exposing
  the API to untrusted callers must validate / constrain the path
  themselves).
- Public-API drift gate clarified: `public-api.txt` baseline is enforced
  by CI with `exit 1` on any diff. Stale entries from the removed
  top-level `ProcedureRegistry` alias dropped.
