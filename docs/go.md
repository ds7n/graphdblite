# Go API

## Installation

The Go binding uses CGo and requires a pre-built `libgraphdblite_ffi` shared library.

```bash
go get github.com/ds7n/graphdblite/bindings/go
```

Place the shared library (`libgraphdblite_ffi.so` on Linux, `.dylib` on macOS) in
`bindings/go/lib/` alongside the binding, or set `CGO_LDFLAGS` to point to its location.

## Getting started

```go
import graphdblite "github.com/ds7n/graphdblite/bindings/go"

db, err := graphdblite.Open("my.db")
if err != nil {
    log.Fatal(err)
}
defer db.Close()

// Write data
res, err := db.Execute("CREATE (n:Person {name: 'Alice', age: 30})")
if err != nil {
    log.Fatal(err)
}
res.Free()

// Read data — results are column-indexed, not named maps
result, err := db.Query("MATCH (n:Person) RETURN n.name, n.age")
if err != nil {
    log.Fatal(err)
}
defer result.Free()

for i := range result.RowCount() {
    name := result.ValueStr(i, 0)
    age  := result.ValueI64(i, 1)
    fmt.Printf("%s is %d years old\n", name, age)
}
```

## Database type

### Open

```go
db, err := graphdblite.Open(path string) (*Database, error)
```

Opens or creates a database file. Default busy timeout is 5000 ms. The path
`":memory:"` opens a private in-memory database (no file is created).

### OpenMemory

```go
db, err := graphdblite.OpenMemory() (*Database, error)
```

Creates a temporary in-memory database. Useful for testing.

### OpenWithTimeout

```go
db, err := graphdblite.OpenWithTimeout(path string, busyTimeoutMs uint32) (*Database, error)
```

Opens a database with a custom write-lock wait timeout (milliseconds).

### Query vs Execute

| Method | Use for | Transaction type |
|--------|---------|-----------------|
| `db.Query(cypher)` | Read-only (`MATCH ... RETURN`) | Read snapshot |
| `db.Execute(cypher)` | Writes (`CREATE`, `SET`, `DELETE`, `MERGE`) | Read-write |

Both return `(*Result, error)`. The caller must call `result.Free()` when done.

### Parameters

`QueryParams` / `ExecuteParams` (on `Database`, and `QueryParams` / `ExecuteParams`
on transactions) take `$name` parameters as a `map[string]any`:

```go
res, err := db.QueryParams("MATCH (p:Person) WHERE p.age > $min RETURN p.name",
    map[string]any{"min": 30})
```

| Go | Cypher |
|----|--------|
| `nil`, nil pointer / slice / map | null |
| `bool` | boolean |
| `int*`, `uint*` (up to `math.MaxInt64`) | integer |
| `float32`, `float64` | float (`1.0` stays a float) |
| `string` | string |
| slice, array | list |
| `map[string]T` | map |

NaN, infinities, unsigned values above `math.MaxInt64` and other types (structs,
maps with non-string keys, `time.Time`) are rejected with an error naming the
parameter. Pass temporal values as ISO strings and convert in Cypher (`date($d)`).

### BeginRead / BeginWrite

```go
tx, err := db.BeginRead()  *ReadTransaction
tx, err := db.BeginWrite() *WriteTransaction
```

Start an explicit transaction. See [Transactions](#transactions).

### Close

```go
db.Close()
```

Closes the database and frees its resources. Safe to call multiple times. A runtime
finalizer will call `Close` if the caller forgets, but explicit close is preferred.

### SnapshotTo

```go
if err := db.SnapshotTo("backup.db"); err != nil {
    return err
}
```

`SnapshotTo(path string) error` writes a consistent, single-file copy of the live
DB to `path`. Uses SQLite's `VACUUM INTO` under the hood: the output is one
self-contained file with no `-wal` / `-shm` sidecars, defragmented and compacted,
suitable for atomic-swap deploys or backup snapshots. Returns an error if a
transaction is active on the handle or if `path` already exists — pick a fresh
path or delete it first.

## Result type

Results are accessed by zero-based `(row, col)` index. Column names can be looked up
with `ColumnName`. All results must be freed with `Free()`.

| Method | Signature | Description |
|--------|-----------|-------------|
| `RowCount` | `() int64` | Number of rows |
| `ColumnCount` | `() int64` | Number of columns |
| `ColumnName` | `(col int64) string` | Column name at index |
| `ValueStr` | `(row, col int64) string` | Value as string (works for any type) |
| `ValueI64` | `(row, col int64) int64` | Integer value; 0 if not an integer |
| `ValueF64` | `(row, col int64) float64` | Float value; 0.0 if not a float |
| `ValueBool` | `(row, col int64) bool` | Boolean value; false if not a bool |
| `ValueType` | `(row, col int64) int32` | Type constant (see below) |
| `JSON` | `() string` | Full result as a JSON string |
| `Free` | `()` | Release result memory; safe to call multiple times |

### Value type constants

```go
graphdblite.TypeNull          // 0
graphdblite.TypeBool          // 1
graphdblite.TypeI64           // 2
graphdblite.TypeF64           // 3
graphdblite.TypeString        // 4
graphdblite.TypeList          // 5
graphdblite.TypePath          // 6
graphdblite.TypeMap           // 7
graphdblite.TypeNode          // 8
graphdblite.TypeRelationship  // 9
graphdblite.TypeDate          // 10
graphdblite.TypeLocalTime     // 11
graphdblite.TypeTime          // 12
graphdblite.TypeLocalDateTime // 13
graphdblite.TypeDateTime      // 14
graphdblite.TypeDuration      // 15
```

Check for null values with `result.ValueType(row, col) == graphdblite.TypeNull`.

### Structured values

Lists, maps, nodes, relationships and paths are read through `JSON()`:

| Value | JSON |
|-------|------|
| float | always has a fraction or exponent (`1.0`); NaN and infinities are `null` |
| node | `{"__id": 1, "__labels": ["Person"], ...properties}` |
| relationship | `{"__src": 1, "__dst": 2, "__type": "KNOWS", "__label": "KNOWS", "__edge_seq": 0, ...properties}` (`__label` is a deprecated alias of `__type`) |
| path | `{"nodes": [node, ...], "relationships": [relationship, ...]}` |

Property names starting with `__` are reserved for the metadata keys (writes are
rejected). `ValueStr` strings are copied out of the result, so they stay valid
after `Free`.

## Transactions

### WriteTransaction

```go
tx, err := db.BeginWrite()
if err != nil { ... }

res, err := tx.Execute("CREATE (n:Person {name: 'Eve'})")
res.Free()

res, err = tx.Query("MATCH (n:Person) RETURN n.name")
// ... use result ...
res.Free()

if err := tx.Commit(); err != nil { ... }
// or: tx.Rollback()
```

`Execute` and `Query` are equivalent within a `WriteTransaction` — both can read and write.
Call `Commit` or `Rollback` exactly once; the transaction is invalid after either.

### ReadTransaction

```go
tx, err := db.BeginRead()
if err != nil { ... }

res, err := tx.Query("MATCH (n:Person) RETURN n.name")
// ... use result ...
res.Free()

tx.Commit() // releases the read snapshot
```

`ReadTransaction` has only `Query` and `Commit` (no `Rollback`, no `Execute`).

### WithWriteTx / WithReadTx

Callback wrappers that handle commit/rollback automatically — parity with the
Node binding's `db.withWriteTx(fn)` / `db.withReadTx(fn)`. The transaction is
committed when the callback returns `nil`, rolled back if it returns an error,
and rolled back before re-raising on a panic.

```go
err := db.WithWriteTx(func(tx *graphdblite.WriteTransaction) error {
    res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
    if err != nil {
        return err
    }
    res.Free()
    return nil
})
// err is the callback's error (or the commit error if commit failed).

var name string
_ = db.WithReadTx(func(tx *graphdblite.ReadTransaction) error {
    res, err := tx.Query("MATCH (n:Person) RETURN n.name AS name LIMIT 1")
    if err != nil {
        return err
    }
    name = res.ValueStr(0, 0)
    res.Free()
    return nil
})
```

## Examples

### Social graph

```go
db, _ := graphdblite.OpenMemory()
defer db.Close()

people := []struct{ name string; age int }{
    {"Alice", 30}, {"Bob", 25}, {"Carol", 28},
}
for _, p := range people {
    res, _ := db.Execute(fmt.Sprintf(
        "CREATE (n:Person {name: '%s', age: %d})", p.name, p.age,
    ))
    res.Free()
}

res, _ := db.Execute(`
    MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
    CREATE (a)-[:KNOWS]->(b)`)
res.Free()

res, _ = db.Execute(`
    MATCH (a:Person {name: 'Bob'}), (b:Person {name: 'Carol'})
    CREATE (a)-[:KNOWS]->(b)`)
res.Free()

result, _ := db.Query(`
    MATCH (a:Person {name: 'Alice'})-[:KNOWS*2]->(fof:Person)
    RETURN fof.name`)
defer result.Free()

for i := range result.RowCount() {
    fmt.Println(result.ValueStr(i, 0))
}
```

### Aggregation

```go
result, err := db.Query(`
    MATCH (a:Person)-[:KNOWS]->(b:Person)
    RETURN a.name, count(*) AS friends, collect(b.name) AS friend_names
    ORDER BY friends DESC`)
if err != nil {
    log.Fatal(err)
}
defer result.Free()

for i := range result.RowCount() {
    name    := result.ValueStr(i, 0)
    friends := result.ValueI64(i, 1)
    names   := result.ValueStr(i, 2) // list rendered as string
    fmt.Printf("%s has %d friend(s): %s\n", name, friends, names)
}
```

### JSON output

```go
result, _ := db.Query("MATCH (n:Person) RETURN n.name, n.age")
defer result.Free()
fmt.Println(result.JSON())
// [{"n.name":"Alice","n.age":30}, ...]
```

## Error handling

All functions return a Go `error` as the second return value. Errors include parse
failures, unknown variables, constraint violations, and I/O errors.

```go
result, err := db.Query("MATCH (n:Invalid Syntax")
if err != nil {
    fmt.Println("query failed:", err)
}
```

Querying or executing on a closed database returns `graphdblite.ErrClosed`
immediately without calling into the C library; any call on a committed or rolled
back transaction returns `graphdblite.ErrTxFinished` (test with `errors.Is`).

Inside a transaction, a statement that fails is rolled back on its own (its
partial writes are undone) and the transaction stays open: the caller can
continue or `Rollback`. A failed `Commit` keeps the transaction open, so
`Rollback` still works.

The binding is safe to use from many goroutines: each C call and its error
message read run on one locked OS thread, and every method keeps its
`Database` / `Result` alive for the duration of the C call, so the finalizer
cannot close a handle mid-call.

## Build requirements

- **CGo** must be enabled (`CGO_ENABLED=1`, the default).
- A pre-built `libgraphdblite_ffi` shared library for your target platform must be
  available. Place it in `bindings/go/lib/` or set `CGO_LDFLAGS` accordingly.
- The `graphdblite.h` FFI header is included automatically via the binding's `#include`
  directive — no manual header setup required.
- Cross-compilation requires a cross-compiled version of the shared library.

## Supported platforms

| OS      | Arch    | Triple                       | Status        |
|---------|---------|------------------------------|---------------|
| Linux   | x86_64  | `x86_64-unknown-linux-gnu`   | ✅ Supported   |
| Linux   | aarch64 | `aarch64-unknown-linux-gnu`  | ✅ Supported   |
| macOS   | arm64   | `aarch64-apple-darwin`       | ✅ Supported   |
| Windows | x86_64  | `x86_64-pc-windows-gnu`      | ✅ Supported (MinGW) |
| macOS   | x86_64  | `x86_64-apple-darwin`        | ❌ Not built — Intel Macs are not in the FFI release matrix. Build `libgraphdblite_ffi.a` from source and drop it into `bindings/go/lib/darwin_amd64/` if you need it. |
| Linux   | i686 / armv7 | 32-bit Linux | ❌ Not built — no release artifacts. Build from source if needed. |
| Windows | x86_64 (MSVC) | `x86_64-pc-windows-msvc` | ❌ Not used by Go — cgo expects MinGW-style linking (`libgraphdblite_ffi.a`), so the MSVC build is shipped only for C/Node/Python consumers. |

The supported triples correspond to the per-platform directories under
`bindings/go/lib/{linux_amd64,linux_arm64,darwin_arm64,windows_amd64}/`, which
are populated at release time by `scripts/populate-go-libs.sh` (see
`docs/publishing.md`).
