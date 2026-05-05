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
- MSRV declared: Rust 1.74.

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

### Removed
- Dead `edge::batch_create_edges` helper (no callers post-binding migration)
  and its tests.
- Deprecated top-level `ProcedureRegistry` re-export (`procedures::Registry`
  is the only form).

### Security
- Closed M2–M5 and L1–L5 audit findings.
- All `format!`+SQL sites validate identifiers via `validate_name`
  (`[A-Za-z0-9_]+` only).
- `Registry::register` `debug_assert!`s that procedure names match the
  grammar's `procedure_name` rule.
- `cargo audit`: 0 vulnerabilities across 246 dependencies (2026-05-05).
