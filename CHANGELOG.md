# Changelog

All notable changes to graphdblite are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/) once a stable release
ships.

## [Unreleased]

### Added
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

### Changed
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
