# Stability

graphdblite is pre-`1.0.0`. **The public API may change in any minor version
release.** Patch releases (`0.X.Y`) are bug fixes only.

## What's public

Anything reachable from `graphdblite::*` and visible in `cargo doc` is the
public surface. Everything else (`cypher::*`, `storage`, `index`, `node`,
`edge`, `temporal`, `types` as a module path, `Database::connection()`) is
`pub(crate)` and not stable.

## Soundness guarantees

These hold regardless of how a caller (or a buggy binding) drives the API:

- `Database::drop` rolls back an open transaction.
- `WriteTxGuard::drop` rolls back if not committed.
- Nested `begin_write` / `begin_read` is rejected.
- Write Cypher inside a read transaction is rejected.
- ACID — provided by the underlying SQLite engine.

A buggy caller can lose uncommitted writes; it cannot corrupt the database.

## Security note for binding authors

`Database::open` accepts any path and passes it directly to SQLite. The
core crate does not sandbox filesystem access. If a binding exposes
`open` to untrusted input, the binding is responsible for path
validation — graphdblite cannot know what paths the embedder considers
safe.

## Bindings

First-party language bindings (Python, Node.js, Go, C) must pass the
conformance suite in [`docs/BINDING_CONFORMANCE.md`](docs/BINDING_CONFORMANCE.md)
(`BC-01..BC-10`).
