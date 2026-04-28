# graphdblite

Embedded graph database with Cypher support, built on SQLite.

**Embedded.** Single file, no server, no configuration. Open it, query it, close it — like SQLite.

**Graph-first.** Cypher queries, adjacency-list storage, graph-aware query planning. The data model and query engine are designed around nodes and edges, not rows and joins.

**Multi-process safe.** Multiple processes can read and write the same database concurrently. Crash recovery is automatic. No lock management, no coordination code.

### Design

Graph databases built on SQLite typically store nodes and edges as rows, then translate graph operations into SQL JOINs. This forces graph queries through a relational planner that has no concept of adjacency or traversal.

graphdblite uses SQLite strictly as a storage engine — a crash-safe, single-file, sorted key-value store. Graph data is stored as compact binary structures (packed adjacency lists, serialized node records), and query planning is handled by a graph-aware optimizer above SQLite. Traversals are direct key lookups, not JOINs.

SQLite's resilience, without its relational model.

**Why build on SQLite?** A correct, crash-safe storage engine is enormously difficult to build. SQLite has decades of testing behind its write-ahead log, file locking, and recovery logic. graphdblite uses that foundation, then replaces everything above it with graph-specific structures and planning.

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

**Scalar functions:** `id`, `labels`, `type`, `keys`, `properties`, `length`, `size`, `head`, `last`, `tail`, `range`, `coalesce`, `nodes`, `relationships`, type conversion (`toInteger`, `toFloat`, `toString`, `toBoolean`), math (`abs`, `ceil`, `floor`, `round`, `sqrt`, `log`, `exp`), string (`trim`, `toUpper`, `toLower`, `replace`, `substring`, `split`)
**Aggregations:** `count`, `sum`, `avg`, `min`, `max`, `collect`, `percentileDisc`, `percentileCont`, `stDev`, `stDevP` — all support `DISTINCT`
**Temporal:** `date()`, `time()`, `datetime()`, `duration()` constructors with accessor properties

See the full [Cypher Reference](docs/cypher.md) for complete syntax and examples.

## Architecture

```
Cypher Parser (pest) --> Logical IR --> Query Planner --> Executor (pull-based iterator model)
                              |
                         Graph Storage (adjacency blobs, secondary indexes)
                              |
                         SQLite (KV mode, WAL, WITHOUT ROWID)
```

- **Storage:** SQLite as a B-tree + WAL engine — no SQL JOINs, no relational query planning
- **Concurrency:** SQLite WAL mode handles multi-process reads/writes
- **Planner:** heuristic-based with index selection for equality lookups

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
