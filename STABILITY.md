# Stability Policy

graphdblite has not yet shipped a `1.0.0` release. This document describes
the public API surface that downstream consumers and binding authors can
rely on, and the stability guarantees that apply at the current version.

## Current version

`0.1.0` — pre-release. Nothing has been published to crates.io, PyPI, or
npm. The lockdown described here is a contract between the core crate and
its first-party bindings (Python, Node.js, Go, C/FFI), not a public
stability promise yet.

A `1.0.0` release will be cut when:

- All first-party binding conformance suites (`docs/BINDING_CONFORMANCE.md`,
  scenarios `BC-01..BC-10`) stay green in CI for a sustained period.
- The public surface listed below has been stable across at least two
  minor releases.
- A `cargo public-api` snapshot is committed and CI fails on unexpected
  drift.

The current snapshot lives at `public-api.txt` and is checked by the
`public-api` job in `.github/workflows/dev-build.yml`. To regenerate after
an intentional surface change:

```bash
cargo install cargo-public-api --locked --version 0.51.0  # one-time
cargo public-api --simplified > public-api.txt
```

## Public surface

Anything reachable through the items in this list is public. Anything
else (including `cypher::*`, `storage`, `index`, `node`, `edge`,
`temporal`, and `Database::connection()`) is `pub(crate)` and may
change between any two commits.

### Core types

- `graphdblite::Database`
- `graphdblite::Config`, `graphdblite::SyncMode`
- `graphdblite::WriteTxGuard`, `graphdblite::ReadTxGuard`

### Database methods

The stateful API every binding consumes:

- `Database::open`, `open_with_config`, `open_memory`
- `Database::begin_write`, `begin_read`
- `Database::execute`, `execute_with_params`
- `Database::commit`, `rollback`
- `Database::create_index`, `drop_index`

The Rust RAII API (recommended for in-tree Rust callers):

- `Database::write_tx() -> WriteTxGuard<'_>`
- `Database::read_tx()  -> ReadTxGuard<'_>`

The two styles are mutually exclusive on a single handle: starting one
while the other is active returns `GraphError::Transaction`. The
underlying `WriteTransaction` / `ReadTransaction` types are
`pub(crate)`-only — they live in a private module and are not
re-exported, so they are unnameable from outside the crate. Their
methods reach external code via `Deref` on the guards.

### Value & graph types

`Value`, `Node`, `Edge`, `NodeId`, `Direction`, `PathValue`,
`Properties`, `Record` — see `src/types.rs` and `src/cypher/record.rs`.
Variants of `Value` and fields of the structs are public. Adding new
variants to `Value` is a breaking change; reordering or removing
variants likewise.

### Temporal types

`CypherDate`, `CypherTime`, `CypherDateTime`, `CypherLocalTime`,
`CypherLocalDateTime`, `CypherDuration` — re-exported from
`graphdblite::types`.

### Errors

`GraphError`, `QueryError`, `QueryPhase`, `ErrorCode`, `Span`. Adding new
variants to these enums is a breaking change at the source level.
Bindings should treat unknown error kinds defensively.

### Procedures (CALL support)

`graphdblite::procedures::{Registry, Def, Param}` for callers that need
to register custom procedures (primarily the TCK harness).
`ProcedureRegistry` is also re-exported at the crate root as a
deprecated alias.

## Stability guarantees by item

| Item | Guarantee |
|------|-----------|
| `Database::{open,open_with_config,open_memory}` | Stable signature. |
| Stateful API (`begin_*`, `execute*`, `commit`, `rollback`) | Stable signature. `execute*` auto-begins/auto-commits when no txn is active (mode chosen by query type); rolls back on error. Inside an explicit txn, a failed `execute` leaves the txn open for the caller to commit or roll back. |
| `Database::create_index`, `drop_index` | Stable signature. Idempotency contract documented in `docs/BINDING_CONFORMANCE.md`. |
| `WriteTxGuard` / `ReadTxGuard` | Stable as guards. Drop semantics (rollback on drop, warn for write guards) are part of the contract. |
| `Value` enum | Adding variants is breaking; treat as exhaustive at the source level. |
| Error enums | Adding variants is breaking. Display format is best-effort, not stable. |
| `Config` fields | Adding fields with sensible defaults is non-breaking when constructed via `Config::default()` / struct update syntax. |
| `Record` | Stable. `fields: HashMap<String, Value>` is the row representation. |
| `procedures::{Registry, Def, Param}` | Stable. The legacy `ProcedureRegistry` alias may be removed in a future minor release. |
| `cypher::*`, `storage`, `index`, `node`, `edge`, `temporal` | **Not public.** May change at any time. |
| `Database::connection()` | **Not public** (`pub(crate)`). |

## Soundness guarantees enforced by the core

These run regardless of binding behavior. A buggy binding cannot corrupt
data — the worst it can do is lose uncommitted writes.

1. `Database::drop` issues `ROLLBACK` if a transaction is open and emits
   a `tracing::warn!`.
2. `WriteTxGuard::drop` rolls back if not committed and emits a
   `tracing::warn!`.
3. `Database::begin_write` / `begin_read` reject nested transactions with
   `GraphError::Transaction`.
4. Write Cypher inside a read transaction is rejected by
   `cypher::execute_cypher` via `ExecContext::require_read_only`.
5. All ACID properties — provided by SQLite below us, independent of
   bindings.

## Writing a compliant binding

First-party bindings drive the database through the stateful API
(`begin_*` / `execute*` / `commit` / `rollback`) and ship a
language-idiomatic safety wrapper (Python `with`, Node callback / `using`,
C explicit `_begin/_execute/_commit/_rollback`).

Every binding must pass the conformance suite documented in
`docs/BINDING_CONFORMANCE.md`. The checklist (`BC-01..BC-10`) covers:

- Transaction lifecycle (commit persists, rollback discards, drop rolls
  back).
- Snapshot isolation visibility across multiple `Database` handles.
- Resource hygiene (no leaks, no use-after-free, no dangling txns at
  process exit).
- Read-only enforcement (writes inside `begin_read` are rejected).
- Multi-process safety under WAL.

CI runs the suite for each first-party binding on every push. Each test
names its checklist ID (`test_BC_NN_*`) so coverage is grep-able across
languages.

## Versioning

graphdblite follows Cargo's semver conventions for the Rust crate. For
the binding packages (PyPI, npm, Go module), the version tracks the core
crate — a binding release at version `X.Y.Z` is built against core
`X.Y.Z`. Bindings do not version independently.

Until `1.0.0`, minor-version bumps (`0.X.0`) may include breaking
changes; patch-version bumps (`0.X.Y`) are bug fixes only.
