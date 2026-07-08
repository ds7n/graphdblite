# Changelog

All notable changes to graphdblite are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/) once a stable release
ships.

## [Unreleased]

## [0.1.2] - 2026-07-08

### Added

- **Composite (multi-column) secondary indexes.** New
  `Database::create_composite_index(label, &[props])` /
  `drop_composite_index` plus matching `WriteTransaction` methods, with
  parity in the Python, Node, Go, and C/FFI bindings. Cypher DDL
  extended: `CREATE INDEX ON :Label(prop1, prop2, …)` and
  `DROP INDEX ON :Label(prop1, prop2, …)`. The planner picks the
  index with the longest leftmost-prefix matched by equality predicates
  — both inline pattern properties (`MATCH (p:Person {tenant_id: 1,
  ext_id: 'a'})`) and WHERE-clause conjuncts (`MATCH (p:Person) WHERE
  p.tenant_id = 1 AND p.ext_id = 'a'`) drive the same lookup. Tie-breaks
  prefer the smaller total index width, then lex on joined property
  names. Storage uses `node_idx$<label>$<p1>$…$<pN>` tables with
  msgpack-concat tuple keys (`$` is forbidden by `validate_name`, so
  the separator is unambiguous); single-prop indexes keep their
  existing `node_idx_<label>_<prop>` naming for backward compatibility
  — both APIs produce the same on-disk table for `N=1`. `db.indexes()`
  emits one row per covered property (`kind = "btree"`), matching the
  multi-prop FTS convention. Nodes missing any covered property are
  skipped from index entries (no `NULL` placeholder).

### Fixed

- **Sort now orders all value types correctly.** The pull-based `ORDER BY`
  paths (named- and slot-iterator) used a stripped comparator that returned
  "equal" for booleans, temporals (`Date`/`Time`/`DateTime`), `Duration`, and
  lists, silently leaving those columns in insertion order. Both paths now use
  the canonical comparator (NaN sorts last, lists compared element-wise).
- **`MERGE … ON CREATE/ON MATCH SET`** with a map (`SET n += {…}` / `SET n =
  {…}`) or label (`SET n:Label`) now maintains secondary and FTS indexes. These
  arms previously wrote node storage directly, so an index-served query could
  miss a MERGE-updated node or return a stale one.
- **`SET n:Label` / `REMOVE n:Label`** now re-index the node. Index/FTS
  maintenance is keyed per-label rather than only on the sorted-first "primary"
  label, so adding or removing a label no longer strands index entries on a
  multi-label node. (All `update_indexes_for_node` / `remove_indexes_for_node`
  callers now pass the full label set.)
- **Secondary-index and FTS OR-chain dedup no longer duplicates FTS rows.** A
  node matching two disjuncts of an OR-chain rewritten to `Union(FullTextLookup,
  …)` receives a different per-branch BM25 `__fts_score`; UNION dedup now ignores
  that synthetic key so the node collapses to one row.
- **Index and FTS table names are collision-free across underscores.**
  Single-property secondary-index tables now use the unambiguous
  `node_idx$<label>$<prop>` scheme and single-property fulltext tables use
  `node_fts$<label>$<prop>` (legacy `_`-delimited tables still resolve for
  existing databases), so `(A_b, c)` and `(A, b_c)` no longer collide onto one
  physical table. The index-lookup executor also re-verifies label membership
  defensively.
- **Contradictory equality predicates on an indexed property** (`n.x = 1 AND n.x
  = 2`) return zero rows again. The index-pushdown pass previously folded only
  the last value into the lookup and dropped the other conjunct, returning wrong
  rows; conflicting same-property equalities now stay in the residual filter.
- **`substring` on multibyte strings no longer panics.** It is now
  character-indexed per the Cypher spec (was byte-indexed, panicking at a
  non-char boundary, e.g. `substring('é', 1)`).
- **C FFI entry points no longer abort the host on a query panic.** Query and
  execute paths run untrusted Cypher under `catch_unwind`; a panic in the core
  now returns `-1` + `graphdb_last_error` instead of unwinding across the
  `extern "C"` boundary (undefined behavior / process abort).
- **`DELETE` of a duplicate-bound parallel edge** no longer under-counts the
  edge-type stats counter. `delete_single_edge` only decrements when a row was
  actually removed, so `db.counts()` and planner cardinality stay accurate.
- **Cypher `DELETE` now cleans up secondary indexes.** `exec_delete`
  previously called `storage::node::delete_node` directly, bypassing
  the index- and FTS-maintenance path that
  `WriteTransaction::delete_node` performs. The bug was latent for
  single-property btree indexes (the executor silently fetched deleted
  nodes back through stale entries) and surfaced as `NodeNotFound`
  errors against composite indexes once the planner reliably picked
  them for follow-up lookups. `exec_delete` now invokes
  `index::remove_indexes_for_node` and `fts::remove_fts_for_node`
  before removing each node, matching the typed-transaction path.
- **OR-chain rewrite across FTS indexes.** `WHERE A OR B OR …` where
  every disjunct is a text predicate (`CONTAINS` / `STARTS WITH` /
  `ENDS WITH`) against an FTS-indexed property of the same label now
  plans as `Union(FullTextLookup, FullTextLookup, …)` with row dedup.
  Mixed AND/OR and a single non-FTS-eligible disjunct still fall back
  to a label scan.
- **Full-text indexes.** New `Database::create_fulltext_index(label,
  property)` and `Database::drop_fulltext_index(...)` plus matching
  `WriteTransaction` methods. Python binding gains parity
  (`WriteTransaction.create_fulltext_index` /
  `.drop_fulltext_index`). Backed by SQLite FTS5 with the trigram
  tokenizer (`case_sensitive 1`). The planner transparently rewrites
  `CONTAINS`, `STARTS WITH`, and `ENDS WITH` predicates on indexed
  `(label, property)` pairs to a `FullTextLookup` operator, with a
  position post-filter for the two anchored ops. Term-length floor
  (`<3` codepoints) falls back to a scan. See `docs/cypher.md` for
  storage-cost trade-offs. Node, Go, and C bindings do not yet
  expose this surface; they will gain it alongside a future
  index-DDL parity project.
- **Regex match operator (`=~`).** OpenCypher full-match semantics
  backed by the Rust `regex` crate. Per-query compiled-regex cache,
  NULL propagation, ReDoS-safe by construction. See
  `docs/cypher.md` for the full feature/limitation matrix.
- **Index DDL parity across Node / Go / C/FFI bindings.** Each
  binding's `WriteTransaction` wrapper now exposes
  `create_index` / `drop_index` / `create_fulltext_index` /
  `drop_fulltext_index` (case-converted per language idiom).
  Closes BC-11 in `docs/BINDING_CONFORMANCE.md`. Python's surface
  is unchanged.
- **Index introspection via `CALL db.indexes()`.** New built-in
  procedure returns `(label, property, kind)` rows for every secondary
  and fulltext index in the database. No new grammar — reuses the
  existing CALL surface.

### Internal

- **pyo3 0.22 → 0.28 upgrade.** Python binding migrated to the newer
  `IntoPyObject` API. No public Python API change. The
  `#![allow(clippy::useless_conversion)]` module-level workaround is
  gone. Follow-on API churn folded in: `PyDict::new_bound` →
  `PyDict::new`, `Python::allow_threads` → `Python::detach`,
  `Bound::downcast` → `cast`, and an `unsendable` pyclass attribute on
  the three wrapper classes (rusqlite's `Connection` is `!Send`).

## [0.1.1] - 2026-05-23

### Performance

- **`WHERE id(n) = expr` now plans as an O(1) `IdLookup`** instead of a
  full node-table scan + filter. Critical fix for downstream batch
  helpers that use `UNWIND $rows AS row MATCH (a) WHERE id(a) = row.s`
  patterns: a 200-node graph with a 500-edge UNWIND batch dropped from
  ~4 s to <0.2 s, and large-scale reproductions (1176 edges) that
  previously cost ~67 s / 16 GB RSS are now milliseconds at sane RSS.
  Applies to both the non-correlated form and the correlated form
  inside `CorrelatedJoin` (predicate-pushdown into the join's right
  side). Empty-label `MATCH (n)` only; labeled `MATCH (n:Foo) WHERE
  id(n) = ...` still uses the scan path.

### Added

- `Database::snapshot_to(path)` — write a consistent, single-file snapshot of
  the database via `VACUUM INTO`. Produces a self-contained SQLite file (no
  `-wal` / `-shm` sidecars), defragmented and compacted. Rejects when a
  transaction is active or the destination path already exists. Exposed
  in every binding (Python `snapshot_to`, Node `snapshotTo`, C
  `graphdb_snapshot_to`, Go `SnapshotTo`).

### Python bindings

- PEP 561 type information: ship a `py.typed` marker plus a complete
  `__init__.pyi` covering `Database`, `WriteTransaction`,
  `ReadTransaction`, the exception hierarchy, batch APIs, and the
  optional `params` argument on `query` / `execute`. Downstream callers
  can drop their `# type: ignore[import-untyped]` workarounds. Also
  re-exports the transaction classes and exceptions from the top-level
  `graphdblite` package.

### Documentation

- Document the `MERGE (a)-[:R]->(b)` edge-upsert pattern with
  `ON CREATE SET r += $props` / `ON MATCH SET r += $props` in the
  Cypher reference (`docs/cypher.md`) and the Python / Node guides.
  Regression tests in `tests/e2e_query_tests.rs` cover the upsert
  flow and inline-prop discrimination of parallel edges. No behavior
  change — just confirms and documents what already worked.

## [0.1.0] - 2026-05-16

First public release.

### Core

- Embedded graph database backed by SQLite (WAL mode; multi-process safe).
- ACID semantics, crash-safe — provided by the SQLite engine.
- Single-file storage; zero configuration.

### Query language

- Cypher: parse → plan → execute pipeline with parse cache and plan cache.
- **openCypher TCK conformance: 100%** (3895/3895 scenarios).
- 50+ functions across string, list, map, math, temporal, predicate, and aggregation categories.
- Variable-length paths, shortest path, all-shortest-paths.
- Pattern comprehensions, list comprehensions, quantifier predicates.
- CALL procedures with YIELD (registry-based; internal use today).
- EXPLAIN for plan inspection.

### Types

- Full Cypher temporal types: Date, Time, LocalTime, DateTime, LocalDateTime, Duration.
- Named-timezone DateTime survives storage round-trips.
- Node and edge property values: i64, f64, bool, string, list, map, path, plus all temporal types.

### Transactions

- Two coexisting APIs on `Database`:
  - Stateful: `begin_write` / `begin_read` / `execute` / `commit` / `rollback`.
  - RAII guards: `write_tx()` / `read_tx()` returning `WriteTxGuard` / `ReadTxGuard`.
- Auto-tx on bare `Database::execute` calls (chooses read or write mode by query class).
- Read-only enforcement: write Cypher inside a read transaction is rejected.

### Storage

- Adjacency-list edge storage; traversals are direct key lookups.
- Parallel edges per `(src, dst, type)` triple, tracked by `__edge_seq`.
- Per-label secondary indexes; index-aware planner picks `IndexLookup` over `Scan` when applicable.

### Language bindings

- Rust (this crate).
- Python — `pip install graphdblite`, full PyPI wheel matrix (glibc + musl Linux, macOS, Windows).
- Node.js — `npm install graphdblite`, native addons for all 7 supported platforms.
- Go — `go get github.com/ds7n/graphdblite/bindings/go`.
- C / FFI — `libgraphdblite_ffi.{so,a,dylib,dll}` + `graphdblite.h` on the GitHub release page.
- Binding conformance contract (`BC-01..BC-10`) documented in `docs/BINDING_CONFORMANCE.md`.

### Public API surface

- Locked down ahead of `1.0.0`: only `graphdblite::*` is public; internal modules are `pub(crate)`. See `STABILITY.md`.
- `Record` exposes accessors only (no public field).
- API surface verifiable via `cargo public-api --simplified`.
