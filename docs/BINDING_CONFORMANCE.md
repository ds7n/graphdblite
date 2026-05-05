# Binding Conformance Checklist

Every first-party graphdblite binding (Python, Go, Node, C/FFI, future) MUST
pass every scenario below. Third-party bindings should run the same checklist
to verify they preserve the core's soundness contract.

Each scenario has a stable ID (`BC-NN`). Per-binding test files include the ID
in the test name or a comment so coverage is grep-able across languages.

The Rust core enforces these guarantees regardless of binding behavior — the
worst a buggy binding can do is lose uncommitted writes. The checklist exists
so bindings expose that contract correctly to their host language.

---

## Transaction lifecycle

### BC-01 — implicit rollback on scope exit without commit

A write transaction that goes out of scope without an explicit commit MUST
NOT persist its writes. After scope exit, a fresh read on the same database
handle MUST observe the pre-transaction state.

In language-idiomatic terms:
- **Python**: `with db.begin_write() as tx: tx.execute(...)` and let the
  block end via an unhandled exception (or explicit `tx.rollback()`).
- **Go**: `tx, _ := db.BeginWrite(); ...; tx.Rollback()`.
- **C**: `graphdb_tx_begin_write(...); ... ; graphdb_tx_rollback(...)`.
- **Node**: `db.withWriteTx(async tx => { throw ... })`.

### BC-02 — commit persists writes

A write transaction that calls commit MUST make its writes visible on
subsequent reads (same handle and any other handle on the same DB file).

### BC-03 — exception/panic mid-transaction discards writes

If the host language raises an exception or the user panics inside a write
transaction, the transaction MUST be rolled back (either by the binding's
context-manager/`defer` machinery or by the core's drop-time rollback) and
the next operation on the same handle MUST succeed.

### BC-04 — nested begin returns an error

Calling `begin_write` (or `begin_read`) while a transaction is already open
on the same handle MUST return an error. The first transaction MUST remain
usable after the rejected nested call.

### BC-05 — write inside a read transaction returns an error

Executing a write Cypher query (CREATE/SET/DELETE/MERGE/REMOVE) inside a
read-only transaction MUST return an error. The transaction MUST remain
usable for subsequent read queries.

### BC-06 — commit/rollback without an active transaction returns an error

Calling commit or rollback on a handle with no active transaction MUST
return an error rather than silently no-op.

### BC-07 — operations on a finished transaction return an error

After commit or rollback, further `execute`/`query`/`commit`/`rollback`
calls on the same transaction object MUST return an error.

---

## Multi-process / cross-handle visibility

### BC-08 — concurrent reader sees pre-txn snapshot

While a writer holds an open write transaction, a reader opened on a
separate handle (or process) on the same DB file MUST observe the
pre-write-transaction state until the writer commits. After commit, the
reader MUST see the new state on its next query.

This validates that bindings do not bypass SQLite's WAL snapshot semantics.

---

## Resource hygiene

### BC-09 — closing the database while a transaction is open does not corrupt data

Closing the database handle (or letting it fall out of scope) while a write
transaction is open MUST roll back the transaction. Reopening the same DB
file MUST NOT observe the uncommitted writes and MUST NOT report
corruption.

### BC-10 — result handles freed independently of transactions

Query result handles (where the binding exposes them, e.g. C/Go
`GraphResult`) MUST remain freeable after the producing transaction has
been committed or rolled back. Freeing a result MUST NOT require an
active transaction.

---

## Coverage matrix

| Scenario | Python | Go | C/FFI | Node |
|----------|:------:|:--:|:-----:|:----:|
| BC-01    | ✓      | ✓  | ✓     | ✓    |
| BC-02    | ✓      | ✓  | ✓     | ✓    |
| BC-03    | ✓      | —¹ | —¹    | ✓    |
| BC-04    | ✓      | ✓  | ✓     | ✓    |
| BC-05    | ✓      | ✓  | ✓     | ✓    |
| BC-06    | ✓      | ✓  | ✓     | ✓    |
| BC-07    | ✓      | ✓  | ✓     | ✓    |
| BC-08    | ✓      | —² | —²    | ✓    |
| BC-09    | ✓      | ✓  | ✓     | ✓    |
| BC-10    | —³     | ✓  | ✓     | —³   |

BC-05 enforcement lives in `cypher::execute_cypher` (`src/cypher/mod.rs`):
when `ExecContext::require_read_only` is set, the planner's output tree is
checked via `executor::is_read_only` before execution and any write operator
fails with `GraphError::Transaction`. The flag is wired automatically by
`Database::execute_with_params` (set when `tx_state == Read`) and by the
typed `ReadTransaction::query*` methods.

¹ BC-03 is panic-driven; idiomatic in Python only. Go/C exercise the
explicit-rollback equivalent under BC-01. The core's drop-time rollback is
covered there.

² BC-08 (multi-process) is exercised by the Rust integration suite
(`tests/multiprocess_tests.rs`) — running it again per binding adds little
value once the Python runner confirms the binding doesn't shortcut WAL.

³ Python and Node don't surface a separate result-handle object — query
results are plain lists/arrays. Not applicable.

---

## Running the suites

| Binding | Command |
|---------|---------|
| Python  | `cd bindings/python && uv run pytest tests/conformance/` |
| Go      | `cd bindings/go && go test -run Conformance` |
| C/FFI   | `cd bindings/ffi && make conformance && ./conformance` |
| Node    | `cd bindings/node && npm test` |

Each runner prints `[BC-NN]` in test names so failures point at the
checklist row directly.
