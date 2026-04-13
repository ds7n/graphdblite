# graphdblite

Embedded graph database with Cypher support. SQLite-grade simplicity, graph-native performance.

## What is this?

"SQLite but for graphs." Single-file, multi-process-safe, embeddable. Uses SQLite as a
key-value engine (WAL mode, `WITHOUT ROWID` tables) with graph-native data structures on top.

Think of graphdblite not as a replacement for Neo4j, but as a replacement for stuffing
graph data into JSON files or relational tables. Local graph storage for applications
and tools that need it, without running a server.

## Quick start

### Rust

```rust
use graphdblite::Database;

let mut db = Database::open("my.db")?;

let tx = db.begin_write()?;
tx.query("CREATE (a:Person {name: 'Alice', age: 30})")?;
tx.query("CREATE (b:Person {name: 'Bob', age: 25})")?;
tx.query("MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)")?;
tx.commit()?;

let tx = db.begin_read()?;
let results = tx.query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name")?;
```

### Python

Prebuilt wheels available from [GitHub Releases](https://github.com/ds7n/graphdblite/releases/tag/dev-latest),
or build from source (requires Rust):

```bash
pip install git+https://github.com/ds7n/graphdblite.git
```

```python
from graphdblite import Database

db = Database("my.db")
db.execute("CREATE (n:Person {name: 'Alice', age: 30})")
results = db.query("MATCH (n:Person) RETURN n.name, n.age")

# In-memory database (useful for testing)
db = Database.open_memory()
```

### CLI

```bash
# Interactive REPL
graphdblite my.db

# Single query
graphdblite my.db -q "MATCH (n:Person) RETURN n.name"
```

## Cypher support

```cypher
-- Query
MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 25 RETURN a.name, b.name
MATCH (a)-[:KNOWS*1..3]->(b) RETURN b                -- variable-length paths
MATCH p = shortestPath((a)-[:KNOWS*]->(b)) RETURN p  -- shortest path

-- Mutate
CREATE (n:Person {name: 'Alice', age: 30})
MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true
SET n.age = 31
DELETE n
```

Supports `OPTIONAL MATCH`, `WITH`, `UNWIND`, `ORDER BY`, `LIMIT`, `CASE`,
`EXISTS {}` subqueries, list comprehensions, and `EXPLAIN`.

**Scalar functions:** `length()`, `nodes()`
**Aggregations:** `count(*)`, `sum()`, `avg()`, `min()`, `max()`, `collect()`

See the full [Cypher Reference](docs/cypher.md) for complete syntax and examples.

## Architecture

```
Cypher Parser (pest) --> Logical IR --> Query Planner --> Executor (Volcano iterator model)
                              |              |
                              |         Cost Estimator
                              |         (cardinality stats)
                              |
                         Graph Storage (adjacency blobs, secondary indexes)
                              |
                         SQLite (KV mode, WAL, WITHOUT ROWID)
```

- **Storage:** SQLite as a B-tree + WAL engine — no SQL JOINs, no relational query planning
- **Concurrency:** SQLite WAL mode handles multi-process reads/writes
- **Optimizer:** cost-based planning with cardinality estimation and index selection

More details: [docs/architecture.md](docs/architecture.md)

## Documentation

- [Cypher Reference](docs/cypher.md) — full query language coverage with examples
- [Python API](docs/python.md) — installation, usage, and examples
- [Rust API](docs/rust.md) — types, transactions, and method reference
- [Architecture](docs/architecture.md) — storage design, concurrency, optimizer internals

## Building

```bash
# Rust library + CLI
cargo build --release

# Python wheel
maturin develop --release

# Run tests
cargo test
```

## License

[MPL-2.0](LICENSE)
