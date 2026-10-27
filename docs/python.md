# Python API

## Installation

```bash
pip install graphdblite
```

### From source

Requires a Rust toolchain ([rustup.rs](https://rustup.rs)).

```bash
# Install directly from GitHub
pip install git+https://github.com/ds7n/graphdblite.git

# Or clone and build locally
git clone https://github.com/ds7n/graphdblite.git
cd graphdblite
pip install .

# For development (editable install with maturin)
pip install maturin
maturin develop --release
```

## Getting started

```python
from graphdblite import Database

# Open or create a database file
db = Database("my.db")

# Write data with execute()
db.execute("CREATE (n:Person {name: 'Alice', age: 30})")
db.execute("CREATE (n:Person {name: 'Bob', age: 25})")
db.execute("""
    MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
    CREATE (a)-[:KNOWS]->(b)
""")

# Read data with query()
results = db.query("MATCH (n:Person) RETURN n.name, n.age")
for row in results:
    print(row["n.name"], row["n.age"])
```

## Database class

### Constructor

```python
db = Database(path, busy_timeout_ms=5000)
db = Database(pathlib.Path("my.db"))   # any os.PathLike works
db = Database(":memory:")              # private in-memory database
```

- `path` — file path to the database (created if it doesn't exist)
- `busy_timeout_ms` — how long to wait for a write lock (milliseconds, default 5000)

### In-memory database

```python
db = Database.open_memory()
```

Creates a temporary in-memory database. Useful for testing.

### query() vs execute()

| Method | Use for | Transaction type |
|--------|---------|-----------------|
| `db.query(cypher)` | Read-only queries (`MATCH ... RETURN`) | Read snapshot |
| `db.execute(cypher)` | Writes (`CREATE`, `SET`, `DELETE`, `MERGE`) | Read-write |

Both return `list[dict]` — each dict is one result row with column names as keys.

```python
# query() for reads
results = db.query("MATCH (n:Person) RETURN n.name, n.age")
# [{"n.name": "Alice", "n.age": 30}, {"n.name": "Bob", "n.age": 25}]

# execute() for writes — returns results if the query has a RETURN clause
db.execute("CREATE (n:Person {name: 'Carol', age: 28})")
```

## Examples

### Create and query a social graph

```python
db = Database("social.db")

# Create people
for name, age in [("Alice", 30), ("Bob", 25), ("Carol", 28), ("Dave", 35)]:
    db.execute(f"CREATE (n:Person {{name: '{name}', age: {age}}})")

# Create relationships
db.execute("""
    MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
    CREATE (a)-[:KNOWS]->(b)
""")
db.execute("""
    MATCH (a:Person {name: 'Bob'}), (b:Person {name: 'Carol'})
    CREATE (a)-[:KNOWS]->(b)
""")
db.execute("""
    MATCH (a:Person {name: 'Carol'}), (b:Person {name: 'Dave'})
    CREATE (a)-[:KNOWS]->(b)
""")

# Query friends-of-friends
results = db.query("""
    MATCH (a:Person {name: 'Alice'})-[:KNOWS*2]->(fof:Person)
    RETURN fof.name
""")
```

### Aggregation

```python
results = db.query("""
    MATCH (a:Person)-[:KNOWS]->(b:Person)
    RETURN a.name, count(*) AS friends, collect(b.name) AS friend_names
    ORDER BY friends DESC
""")
```

### Shortest path

```python
results = db.query("""
    MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Dave'})
    MATCH p = shortestPath((a)-[:KNOWS*]->(b))
    RETURN length(p) AS hops
""")
```

### MERGE (upsert)

`MERGE` works on both nodes and relationships. The pattern is matched
atomically; if nothing matches it is created. Pair with `ON CREATE` /
`ON MATCH` clauses to set different properties on the create vs. match
branches.

```python
# Node upsert
db.execute("""
    MERGE (n:Person {name: 'Alice'})
    ON CREATE SET n.created = true
    ON MATCH SET n.seen = true
""")

# Edge upsert — replaces a manual "DELETE existing + CREATE new" workaround
db.execute(
    """
    MATCH (a:Fn {name: $src}), (b:Fn {name: $dst})
    MERGE (a)-[r:CALLS]->(b)
    ON CREATE SET r += $props
    ON MATCH  SET r += $props
    """,
    {"src": "foo", "dst": "bar", "props": {"lineno": 10, "col": 5}},
)
```

Edge identity: `MERGE (a)-[:R]->(b)` finds an existing `(a)-[:R]->(b)`
edge if any exists. `MERGE (a)-[:R {x: 1}]->(b)` is constrained by the
inline properties — a parallel `(a)-[:R {x: 2}]->(b)` edge will not be
matched, and a new one is created. Direction is significant.

### Bulk insert with UNWIND

```python
db.execute("""
    UNWIND ['Eve', 'Frank', 'Grace'] AS name
    CREATE (n:Person {name: name})
""")
```

### Single-file snapshot / export

```python
# Write a consistent, single-file copy of the live DB.
db.snapshot_to("backup.db")
```

`snapshot_to(path)` uses SQLite's `VACUUM INTO` under the hood. The
output is one self-contained file with no `-wal` / `-shm` sidecars,
defragmented and compacted, suitable for atomic-swap deploys or
backup snapshots. Raises if a transaction is active on the handle or
if `path` already exists — pick a fresh path or delete it first.

## Transactions

```python
with db.begin_write() as tx:          # commits on success, rolls back on exception
    tx.execute("CREATE (:Person {name: $n})", {"n": "Eve"})
    tx.execute("CREATE (:Person {name: $n})", {"n": "Frank"})

with db.begin_read() as tx:           # snapshot; released on exit
    rows = tx.query("MATCH (p:Person) RETURN count(p) AS c")
```

- A statement that fails inside a transaction is rolled back on its own (its
  partial writes are undone); the transaction stays open, so you can catch the
  error and continue, or roll back.
- A transaction object that is dropped (garbage-collected) without
  `commit()` / `rollback()` rolls back and releases the database.
- `db.close()` with an open transaction rolls it back; the transaction object
  then raises `RuntimeError("database is closed")`.
- After `commit()` / `rollback()` the object raises
  `RuntimeError("transaction is already finished")`. A failed `commit()`
  leaves the transaction open.
- `ReadTransaction` has `query`, `commit` and `rollback` (same effect for a
  read). Transaction calls release the GIL while the query runs.

## Result values

| Value | Python |
|-------|--------|
| integer / float / string / boolean / null | `int` / `float` / `str` / `bool` / `None` |
| list / map | `list` / `dict` |
| node | `{"__id": 1, "__labels": ["Person"], ...properties}` |
| relationship | `{"__src": 1, "__dst": 2, "__type": "KNOWS", "__label": "KNOWS", "__edge_seq": 0, ...properties}` (`__label` is a deprecated alias of `__type`) |
| path | `{"nodes": [node, ...], "relationships": [relationship, ...]}` |
| temporal | ISO 8601 `str` |

Property names starting with `__` are reserved for the metadata keys (writes
are rejected); for older data that has one, the metadata key wins.

## Error handling

| Exception | Raised for |
|-----------|------------|
| `GraphDBError` | base class of every graphdblite error (also a caught Rust panic: `internal panic: ...`) |
| `ParseError` (alias `QueryError`) | base class of every Cypher query error |
| `CypherSyntaxError` | syntax errors, undefined variables (`SyntaxError(...)`) |
| `CypherSemanticError` | semantic errors (`SemanticError(...)`) |
| `CypherTypeError` | runtime type errors, e.g. `1 - 'a'` (`TypeError(...)`) |
| `CypherArgumentError` | bad arguments, missing parameters (`ArgumentError(...)`) |
| `CypherArithmeticError` | overflow, out-of-range numbers |
| `EntityNotFoundError`, `ConstraintViolationError`, `ProcedureError` | the matching openCypher kinds |
| `TransactionError` | transaction state: nested begin, write in a read transaction, ... |
| `StorageError` | SQLite / file errors |
| `NodeNotFoundError` | a node id that does not exist |
| `RuntimeError` | closed database, finished transaction |

All Cypher error classes derive from `ParseError` (0.1.x raised `ParseError`
for every query error), so `except graphdblite.ParseError` keeps working.

```python
try:
    db.query("RETURN 1 - 'a' AS x")
except graphdblite.CypherTypeError as e:
    print(f"Query failed: {e}")
```

`graphdblite.__version__` is read from the installed package metadata.

### Fulltext indexes

```python
db.execute("CREATE (n:Doc {body: 'hello world'})")

# Create a fulltext index via WriteTransaction
with db.begin_write() as tx:
    tx.create_fulltext_index("Doc", "body")

# Queries are unchanged — the planner picks up the index automatically.
rows = db.query("MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n.body")
```

See `docs/cypher.md` "Full-text indexes" for semantics, storage
cost, and limitations.

## Rust API

For lower-level access (direct node/edge CRUD, index management, traversal), use the
Rust API. See [rust.md](rust.md) for the full reference.
