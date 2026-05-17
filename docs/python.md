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

## Error handling

Both `query()` and `execute()` raise `RuntimeError` on failure (parse errors, constraint
violations, etc.).

```python
try:
    db.query("MATCH (n:Invalid Syntax")
except RuntimeError as e:
    print(f"Query failed: {e}")
```

## Rust API

For lower-level access (direct node/edge CRUD, index management, traversal), use the
Rust API. See [rust.md](rust.md) for the full reference.
