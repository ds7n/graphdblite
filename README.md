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

## Language bindings

### Rust

Install — add to `Cargo.toml`:
```toml
[dependencies]
graphdblite = { git = "https://github.com/ds7n/graphdblite.git" }
```

Usage:
```rust
use graphdblite::Database;

let mut db = Database::open("my.db")?;
let tx = db.begin_write()?;
tx.query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")?;
tx.commit()?;

let tx = db.begin_read()?;
let results = tx.query("MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name")?;
```

[Full Rust docs →](docs/rust.md)

### Python

Install:
```bash
pip install graphdblite
```

Usage:
```python
from graphdblite import Database

db = Database("my.db")
db.execute("CREATE (n:Person {name: 'Alice', age: 30})")
results = db.query("MATCH (n:Person) RETURN n.name, n.age")
```

[Full Python docs →](docs/python.md)

### Node.js

Install:
```bash
npm install graphdblite
```

Usage:
```javascript
const { Database } = require("graphdblite");

const db = new Database("my.db");
const results = db.query("MATCH (n:Person) RETURN n.name, n.age");
```

[Full Node.js docs →](docs/node.md)

### Go

Install:
```bash
go get github.com/ds7n/graphdblite/bindings/go
```

Usage:
```go
import "github.com/ds7n/graphdblite/bindings/go"

db, _ := graphdblite.Open("my.db")
defer db.Close()
result, _ := db.Query("MATCH (n:Person) RETURN n.name, n.age")
defer result.Free()
```

[Full Go docs →](docs/go.md)

### C

Install: link against `libgraphdblite_ffi` and include `graphdblite.h`.

Usage:
```c
#include "graphdblite.h"

GraphDB *db;
graphdb_open("my.db", &db);
GraphResult *result;
graphdb_query(db, "MATCH (n:Person) RETURN n.name", &result);
graphdb_result_free(result);
graphdb_close(db);
```

[Full C docs →](docs/c.md)

### CLI

Install:
```bash
cargo install --path .
```

Usage:
```bash
graphdblite my.db                                       # interactive REPL
graphdblite my.db -q "MATCH (n:Person) RETURN n.name"   # single query
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

Clauses: `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH`, `ORDER BY`, `SKIP`, `LIMIT`, `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `UNWIND`, `UNION`, `CALL...YIELD`, `EXPLAIN`.

Expressions: list comprehensions, pattern comprehensions, `CASE`, `EXISTS {}` subqueries, quantifier predicates (`any`, `all`, `none`, `single`), temporal constructors, regex matching.

Functions: 50+ scalar, string, math, aggregation, and temporal functions. See the full [Cypher Reference](docs/cypher.md).

**openCypher TCK conformance: 100%** (3895/3895 scenarios passing).

## Architecture

```
Cypher string → Parser (pest PEG) → Logical IR → Executor (pull-based iterators)
                                                       |
                                              Graph Storage (adjacency blobs, indexes)
                                                       |
                                              SQLite (KV mode, WAL, WITHOUT ROWID)
```

More details: [Architecture](docs/architecture.md)

## Building

```bash
cargo build --release          # Rust library + CLI
cargo test --tests             # unit + integration tests
cargo test --test tck          # openCypher TCK conformance
maturin develop --release      # Python wheel (dev)
```

## License

[MPL-2.0](LICENSE)
