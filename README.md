# graphdblite

Embedded graph database with Cypher support, built on SQLite.

**Embedded.** Single file, no server, no configuration. Open it, query it, close it — like SQLite.

**Graph-first.** Cypher queries, adjacency-list storage, graph-aware query planning. Traversals are direct key lookups, not JOINs.

**Multi-process safe.** Multiple processes can read and write concurrently. Crash recovery is automatic.

graphdblite uses SQLite strictly as a crash-safe key-value store; graph data, query planning, and execution all live in a graph-native layer above it. See [Architecture](docs/architecture.md) for the full rationale.

**openCypher TCK conformance: 100%** (3895/3895 scenarios).

## Quick start

```bash
cargo install graphdblite

graphdblite my.db -q "CREATE (a:Person {name:'Alice'})-[:KNOWS]->(b:Person {name:'Bob'})"
graphdblite my.db -q "MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name"
graphdblite my.db                  # interactive REPL (.help for commands)
```

## Language bindings

| Language | Install | Docs |
|---|---|---|
| Rust    | `cargo add graphdblite`                        | [Rust binding](docs/rust.md) |
| Python  | `pip install graphdblite`                      | [Python binding](docs/python.md) |
| Node.js | `npm install graphdblite`                      | [Node.js binding](docs/node.md) |
| Go      | `go get github.com/ds7n/graphdblite/bindings/go` | [Go binding](docs/go.md) |
| C       | link `libgraphdblite_ffi`, include `graphdblite.h` | [C binding](docs/c.md) |

The canonical embedded use case in Rust:

```rust
use graphdblite::Database;

let mut db = Database::open("my.db")?;
let tx = db.write_tx()?;
tx.query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")?;
tx.commit()?;

let tx = db.read_tx()?;
let results = tx.query("MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name")?;
tx.commit()?;
```

## Cypher

```cypher
MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 25 RETURN a.name, b.name
MATCH (a)-[:KNOWS*1..3]->(b) RETURN b                  // variable-length paths
MATCH p = shortestPath((a)-[:KNOWS*]->(b)) RETURN p    // shortest path
MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true
```

Full reference (clauses, expressions, 50+ functions): [Cypher reference](docs/cypher.md).

## Build & test

```bash
cargo build --release
cargo test --tests
cargo test --test tck --features tck-support
```

Reproducible release builds, benchmarks, and the audit workflow: [Build guide](docs/building.md).

## Stability

Pre-`1.0.0`. Public API surface and stability guarantees are documented in [Stability](STABILITY.md); binding authors should also read [Binding conformance](docs/BINDING_CONFORMANCE.md).

## License

[MIT](LICENSE)
