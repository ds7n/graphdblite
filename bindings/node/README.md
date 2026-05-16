# graphdblite

Embedded graph database with [Cypher](https://opencypher.org/) query support, backed by SQLite.

- **Embedded.** Single file, no server, no configuration — like SQLite.
- **Graph-first.** Cypher queries, adjacency-list storage, graph-aware query planning.
- **Multi-process safe.** Multiple processes can read and write concurrently. ACID via SQLite.
- **openCypher TCK conformance: 100%** (3895/3895 scenarios).

## Install

```bash
npm install graphdblite
```

Pre-built native addons are published for Linux (glibc + musl, x86_64 + aarch64), macOS (x86_64 + aarch64), and Windows (x86_64). npm resolves the right one for your platform automatically.

## Quick start

```js
const { Database } = require('graphdblite');

const db = new Database('my.db');

db.execute("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})");

const results = db.query('MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name');
for (const row of results) {
  console.log(row['a.name'], '→', row['b.name']);
}

db.close();
```

## Cypher coverage

```cypher
MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 25 RETURN a.name, b.name
MATCH (a)-[:KNOWS*1..3]->(b) RETURN b                  // variable-length paths
MATCH p = shortestPath((a)-[:KNOWS*]->(b)) RETURN p    // shortest path
MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true
```

Clauses, expressions, 50+ functions, full reference: [Cypher reference](https://github.com/ds7n/graphdblite/blob/main/docs/cypher.md).

## Documentation

- **Node.js API:** <https://github.com/ds7n/graphdblite/blob/main/docs/node.md>
- **Architecture:** <https://github.com/ds7n/graphdblite/blob/main/docs/architecture.md>
- **Repository:** <https://github.com/ds7n/graphdblite>
- **Issues:** <https://github.com/ds7n/graphdblite/issues>

## License

[MIT](https://github.com/ds7n/graphdblite/blob/main/LICENSE)
