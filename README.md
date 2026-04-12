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
db.query("CREATE (n:Person {name: 'Alice', age: 30})")
results = db.query("MATCH (n:Person) RETURN n.name, n.age")
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
-- Pattern matching
MATCH (a:Label)-[:EDGE_TYPE]->(b)
MATCH (a)-[:TYPE*1..3]->(b)           -- variable-length paths
OPTIONAL MATCH (a)-[:KNOWS]->(b)

-- Filtering and projection
WHERE a.name = 'Alice' AND b.age > 25
WHERE a.name IS NULL
RETURN a.name, count(*) AS cnt, collect(b.name) AS names
ORDER BY cnt DESC
LIMIT 10

-- Intermediate processing
WITH a, count(*) AS cnt WHERE cnt > 1 RETURN a.name, cnt

-- Conditionals
RETURN CASE WHEN a.age > 30 THEN 'senior' ELSE 'junior' END AS level

-- Mutations
CREATE (n:Label {key: value})
CREATE (a)-[:TYPE]->(b)
SET n.property = value
DELETE n
DETACH DELETE n
MERGE (n:Label {key: value}) ON CREATE SET n.created = true
```

**Aggregations:** `count(*)`, `collect()`, `sum()`, `avg()`, `min()`, `max()`

## Architecture

```
Cypher Parser (pest) ──► Logical IR ──► Executor (Volcano iterator model)
                              │
                         Graph Storage (adjacency blobs, secondary indexes)
                              │
                         SQLite (KV mode, WAL, WITHOUT ROWID)
```

- **Storage:** SQLite as a B-tree + WAL engine — no SQL JOINs, no relational query planning
- **Concurrency:** SQLite WAL mode handles multi-process reads/writes
- **Encoding:** msgpack for node/edge properties, sorted varint arrays for adjacency lists
- **Indexes:** secondary property indexes for O(log n) lookups, used automatically by the query planner

See [DESIGN.md](DESIGN.md) for the full design document.

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
