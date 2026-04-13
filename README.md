# graphdblite

Embedded graph database with Cypher support. SQLite-grade simplicity, graph-native performance.

## What is this?

"SQLite but for graphs." Single-file, multi-process-safe, embeddable. Uses SQLite as a
key-value engine (WAL mode, `WITHOUT ROWID` tables) with graph-native data structures on top.

Not competing with Neo4j on billion-edge workloads — designed for <1M nodes / <5M edges
with a ceiling around 50M nodes.

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

```bash
pip install graphdblite
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
MATCH (a:Person)-[:KNOWS]->(b:Person)           -- pattern matching
MATCH (a)-[:KNOWS*1..3]->(b)                     -- variable-length paths
OPTIONAL MATCH (a)-[:KNOWS]->(b)                 -- optional patterns
MATCH p = shortestPath((a)-[:KNOWS*]->(b))       -- graph algorithms
WHERE a.name STARTS WITH 'A' AND b.age > 25     -- filtering
WHERE EXISTS { (a)-[:KNOWS]->(b) }               -- subquery predicates
RETURN a.name, count(*) AS cnt, collect(b.name)  -- aggregation
WITH a, count(*) AS cnt WHERE cnt > 1            -- intermediate processing
UNWIND [1, 2, 3] AS x RETURN x                   -- list expansion
[x IN list WHERE x > 2 | x * 10]                -- list comprehensions
CASE WHEN a.age > 30 THEN 'senior' END          -- conditionals
CREATE (n:Label {key: value})                    -- mutations
MERGE (n:Label {key: val}) ON CREATE SET ...     -- upsert
EXPLAIN MATCH (a:Person) RETURN a.name           -- query planning
```

**Aggregations:** `count(*)`, `collect()`, `sum()`, `avg()`, `min()`, `max()`
**Scalar functions:** `length()`, `nodes()`

Full Cypher reference: [docs/cypher.md](docs/cypher.md)

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
