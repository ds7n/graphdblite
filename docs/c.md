# C API

## Installation

Build `libgraphdblite_ffi` from source (requires a Rust toolchain — [rustup.rs](https://rustup.rs)):

```bash
git clone https://github.com/ds7n/graphdblite.git
cd graphdblite/bindings/ffi
cargo build --release
# Artifacts: target/release/libgraphdblite_ffi.so (Linux)
#             target/release/libgraphdblite_ffi.dylib (macOS)
#             target/release/graphdblite_ffi.dll (Windows)
```

Copy `bindings/ffi/graphdblite.h` to your include path and link against the shared library.

## Getting started

```c
#include "graphdblite.h"
#include <stdio.h>

int main(void) {
    GraphDB *db = NULL;
    if (graphdb_open("my.db", &db) != 0) {
        fprintf(stderr, "open failed: %s\n", graphdb_last_error());
        return 1;
    }

    GraphResult *result = NULL;
    if (graphdb_execute(db, "CREATE (n:Person {name: 'Alice', age: 30})", &result) != 0) {
        fprintf(stderr, "execute failed: %s\n", graphdb_last_error());
        graphdb_close(db);
        return 1;
    }
    graphdb_result_free(result);

    if (graphdb_query(db, "MATCH (n:Person) RETURN n.name, n.age", &result) != 0) {
        fprintf(stderr, "query failed: %s\n", graphdb_last_error());
        graphdb_close(db);
        return 1;
    }

    int64_t rows = graphdb_result_row_count(result);
    int64_t cols = graphdb_result_column_count(result);
    for (int64_t r = 0; r < rows; r++) {
        for (int64_t c = 0; c < cols; c++) {
            printf("%s = %s\n",
                graphdb_result_column_name(result, c),
                graphdb_result_value_str(result, r, c));
        }
    }

    graphdb_result_free(result);
    graphdb_close(db);
    return 0;
}
```

## API reference

### Database lifecycle

```c
int graphdb_open(const char *path, GraphDB **out);
```
Open or create a database at `path`. Writes the handle to `*out`. Uses the default busy timeout (5000 ms).

---

```c
int graphdb_open_with_timeout(const char *path, uint32_t busy_timeout_ms, GraphDB **out);
```
Open a database with a custom busy timeout. Use when multiple processes may write concurrently.

---

```c
int graphdb_open_memory(GraphDB **out);
```
Open a temporary in-memory database. Useful for testing. Data is lost when the handle is closed.

---

```c
void graphdb_close(GraphDB *db);
```
Close the database and free all resources. Passing `NULL` is a no-op. The handle is invalid after this call.

---

```c
int graphdb_snapshot_to(GraphDB *db, const char *path);
```
Write a consistent, single-file copy of the live DB to `path`. Uses SQLite's `VACUUM INTO` under the hood — the output is one self-contained file with no `-wal` / `-shm` sidecars, defragmented and compacted, suitable for atomic-swap deploys or backup snapshots. Returns non-zero if a transaction is active on the handle or if `path` already exists; call `graphdb_last_error()` for details.

---

### Query and execute

```c
int graphdb_query(GraphDB *db, const char *cypher, GraphResult **out);
```
Execute a read-only Cypher query in a read transaction. Writes the result handle to `*out`. The caller must free it with `graphdb_result_free`.

---

```c
int graphdb_execute(GraphDB *db, const char *cypher, GraphResult **out);
```
Execute a write Cypher query (`CREATE`, `SET`, `DELETE`, `MERGE`) in a write transaction. Writes the result handle to `*out`. The caller must free it with `graphdb_result_free`.

| Function | Use for | Transaction type |
|----------|---------|-----------------|
| `graphdb_query` | Read-only (`MATCH ... RETURN`) | Read snapshot |
| `graphdb_execute` | Writes (`CREATE`, `SET`, `DELETE`, `MERGE`) | Read-write |

---

### Manual transactions

Use these when you need multiple queries in a single atomic transaction.

```c
int graphdb_tx_begin_read(GraphDB *db);
int graphdb_tx_begin_write(GraphDB *db);
```
Begin a read or write transaction on `db`. Do not call other query functions on the same handle until the transaction is committed or rolled back.

---

```c
int graphdb_tx_query(GraphDB *db, const char *cypher, GraphResult **out);
```
Execute a Cypher query within the current transaction. Works for both read and write transactions.

---

```c
int graphdb_tx_commit(GraphDB *db);
int graphdb_tx_rollback(GraphDB *db);
```
Commit or roll back the current transaction.

---

### Result inspection

```c
int64_t graphdb_result_row_count(const GraphResult *result);
int64_t graphdb_result_column_count(const GraphResult *result);
```
Return the number of rows and columns in a result. Return 0 if `result` is `NULL`.

---

```c
const char *graphdb_result_column_name(GraphResult *result, int64_t col);
```
Return the column name at index `col`. Returns `NULL` if out of bounds. The pointer is valid until `graphdb_result_free` is called.

---

```c
const char *graphdb_result_value_str(const GraphResult *result, int64_t row, int64_t col);
```
Return the value at `(row, col)` as a string. All types are formatted: numbers as decimal, booleans as `true`/`false`, lists as `[a, b, c]`, `null` as `"null"`. Returns `NULL` if out of bounds. The pointer is valid until the next call to `graphdb_result_value_str` on any result on the current thread.

---

```c
int64_t graphdb_result_value_i64(const GraphResult *result, int64_t row, int64_t col);
f64    graphdb_result_value_f64(const GraphResult *result, int64_t row, int64_t col);
int    graphdb_result_value_bool(const GraphResult *result, int64_t row, int64_t col);
```
Return the typed value at `(row, col)`. Return 0 / 0.0 / 0 if the value is not the expected type or is out of bounds.

---

```c
int graphdb_result_value_type(const GraphResult *result, int64_t row, int64_t col);
```
Return the type tag of the value at `(row, col)`:

| Code | Type |
|------|------|
| 0 | null |
| 1 | bool |
| 2 | i64 |
| 3 | f64 |
| 4 | string |
| 5 | list |
| 6 | path |
| 7 | map |
| 8 | node |
| 9 | edge |
| 10–15 | temporal (Date, LocalTime, Time, LocalDateTime, DateTime, Duration) |

---

```c
const char *graphdb_result_json(GraphResult *result);
```
Return the entire result as a JSON array of objects. Built lazily on first call; the pointer is valid until `graphdb_result_free` is called.

---

```c
void graphdb_result_free(GraphResult *result);
```
Free a result handle. Passing `NULL` is a no-op. The handle is invalid after this call.

---

### Error handling

```c
const char *graphdb_last_error(void);
```
Return the last error message for the current thread, or `NULL` if no error has occurred. The pointer is valid until the next API call on the current thread.

---

## Ownership rules

- `graphdb_open` / `graphdb_open_with_timeout` / `graphdb_open_memory` — caller owns the `GraphDB*`; must free with `graphdb_close`.
- `graphdb_query` / `graphdb_execute` / `graphdb_tx_query` — caller owns the `GraphResult*`; must free with `graphdb_result_free`.
- Strings returned by `graphdb_result_column_name` and `graphdb_result_json` are owned by the `GraphResult` and are valid until `graphdb_result_free`.
- The string returned by `graphdb_result_value_str` is stored in a thread-local buffer and is valid only until the next call to that function on the current thread.
- The string returned by `graphdb_last_error` is stored in thread-local storage and is valid only until the next API call on the current thread.

## Error handling

All functions that can fail return `0` on success and `-1` on error. Inspect the failure reason with:

```c
const char *msg = graphdb_last_error();
```

Errors include: invalid path, query parse errors, type errors, constraint violations, and SQLite I/O errors. On success, the error is cleared.

```c
GraphResult *result = NULL;
if (graphdb_query(db, "MATCH (n:Person) RETURN n.name", &result) != 0) {
    fprintf(stderr, "query failed: %s\n", graphdb_last_error());
}
```

## Example

Complete program — build a small social graph and query friends-of-friends:

```c
#include "graphdblite.h"
#include <stdio.h>
#include <stdlib.h>

static void die(const char *msg) {
    fprintf(stderr, "%s: %s\n", msg, graphdb_last_error());
    exit(1);
}

static void exec(GraphDB *db, const char *cypher) {
    GraphResult *r = NULL;
    if (graphdb_execute(db, cypher, &r) != 0) die("execute");
    graphdb_result_free(r);
}

int main(void) {
    GraphDB *db = NULL;
    if (graphdb_open_memory(&db) != 0) die("open");

    exec(db, "CREATE (n:Person {name: 'Alice'})");
    exec(db, "CREATE (n:Person {name: 'Bob'})");
    exec(db, "CREATE (n:Person {name: 'Carol'})");
    exec(db, "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)");
    exec(db, "MATCH (a:Person {name: 'Bob'}),  (b:Person {name: 'Carol'}) CREATE (a)-[:KNOWS]->(b)");

    GraphResult *result = NULL;
    const char *q = "MATCH (a:Person {name: 'Alice'})-[:KNOWS*2]->(fof:Person) RETURN fof.name";
    if (graphdb_query(db, q, &result) != 0) die("query");

    int64_t rows = graphdb_result_row_count(result);
    for (int64_t i = 0; i < rows; i++) {
        printf("friend-of-friend: %s\n", graphdb_result_value_str(result, i, 0));
    }

    graphdb_result_free(result);
    graphdb_close(db);
    return 0;
}
```

## Build

```bash
# Linux
gcc main.c -o main \
    -I/path/to/graphdblite/bindings/ffi \
    -L/path/to/graphdblite/target/release \
    -lgraphdblite_ffi -ldl -lpthread -lm \
    -Wl,-rpath,/path/to/graphdblite/target/release

# macOS
clang main.c -o main \
    -I/path/to/graphdblite/bindings/ffi \
    -L/path/to/graphdblite/target/release \
    -lgraphdblite_ffi
```
