# Node.js API

## Installation

### Prebuilt binaries (no Rust required)

Download the `.node` binary for your platform from the
[latest dev build](https://github.com/ds7n/graphdblite/releases/tag/dev-latest)
and install the package directly:

```bash
npm install graphdblite
```

### From source

Requires a Rust toolchain ([rustup.rs](https://rustup.rs)) and
[napi-rs CLI](https://napi.rs/docs/introduction/getting-started).

```bash
# Clone and build
git clone https://github.com/ds7n/graphdblite.git
cd graphdblite/bindings/node
npm install
npm run build
```

## Getting started

```js
const { Database } = require('graphdblite');

// Open or create a database file
const db = new Database('my.db');

// Write data with execute()
db.execute("CREATE (n:Person {name: 'Alice', age: 30})");
db.execute("CREATE (n:Person {name: 'Bob', age: 25})");
db.execute(`
  MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
  CREATE (a)-[:KNOWS]->(b)
`);

// Read data with query()
const results = db.query('MATCH (n:Person) RETURN n.name, n.age');
for (const row of results) {
  console.log(row['n.name'], row['n.age']);
}

db.close();
```

## Database class

### Constructor

```js
const db = new Database(path);
```

- `path` — file path to the database (created if it doesn't exist)

Uses a default busy timeout of 5000 ms when waiting for a write lock.

### Factory methods

```js
// In-memory database (useful for testing)
const db = Database.openMemory();

// File database with a custom busy timeout (milliseconds)
const db = Database.openWithTimeout(path, busyTimeoutMs);
```

### query() vs execute()

| Method | Use for | Transaction type |
|--------|---------|-----------------|
| `db.query(cypher)` | Read-only queries (`MATCH ... RETURN`) | Read snapshot |
| `db.execute(cypher)` | Writes (`CREATE`, `SET`, `DELETE`, `MERGE`) | Read-write |

Both return an array of objects — each object is one result row with column names as keys.

```js
// query() for reads
const results = db.query('MATCH (n:Person) RETURN n.name, n.age');
// [{ 'n.name': 'Alice', 'n.age': 30 }, { 'n.name': 'Bob', 'n.age': 25 }]

// execute() for writes — returns results if the query has a RETURN clause
db.execute("CREATE (n:Person {name: 'Carol', age: 28})");
```

### close()

```js
db.close();
```

Releases the database connection. Any further calls on the instance will throw.

## Transactions

For multi-statement atomicity, use explicit transactions.

### WriteTransaction

```js
const tx = db.beginWrite();
try {
  tx.execute("CREATE (n:Person {name: 'Eve'})");
  tx.execute("CREATE (n:Person {name: 'Frank'})");
  tx.commit();
} catch (err) {
  tx.rollback();
  throw err;
}
```

| Method | Description |
|--------|-------------|
| `tx.execute(cypher)` | Run a write (or read) query within the transaction |
| `tx.query(cypher)` | Alias for `execute()` — same connection, same transaction |
| `tx.commit()` | Commit and close the transaction |
| `tx.rollback()` | Abort and close the transaction |

After `commit()` or `rollback()` the transaction object is finished — further calls throw.

### ReadTransaction

```js
const tx = db.beginRead();
try {
  const a = tx.query('MATCH (n:Person) RETURN n.name');
  const b = tx.query('MATCH ()-[r:KNOWS]->() RETURN count(r) AS total');
  tx.commit();
} catch (err) {
  tx.commit(); // release the snapshot even on error
}
```

| Method | Description |
|--------|-------------|
| `tx.query(cypher)` | Run a read-only query within the transaction |
| `tx.commit()` | Release the read snapshot |

## Examples

### Create and query a social graph

```js
const db = Database.openMemory();

for (const [name, age] of [['Alice', 30], ['Bob', 25], ['Carol', 28], ['Dave', 35]]) {
  db.execute(`CREATE (n:Person {name: '${name}', age: ${age}})`);
}

db.execute(`
  MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
  CREATE (a)-[:KNOWS]->(b)
`);
db.execute(`
  MATCH (a:Person {name: 'Bob'}), (b:Person {name: 'Carol'})
  CREATE (a)-[:KNOWS]->(b)
`);
db.execute(`
  MATCH (a:Person {name: 'Carol'}), (b:Person {name: 'Dave'})
  CREATE (a)-[:KNOWS]->(b)
`);

// Friends-of-friends
const fof = db.query(`
  MATCH (a:Person {name: 'Alice'})-[:KNOWS*2]->(fof:Person)
  RETURN fof.name
`);
```

### Aggregation

```js
const results = db.query(`
  MATCH (a:Person)-[:KNOWS]->(b:Person)
  RETURN a.name, count(*) AS friends, collect(b.name) AS friend_names
  ORDER BY friends DESC
`);
```

### Shortest path

```js
const results = db.query(`
  MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Dave'})
  MATCH p = shortestPath((a)-[:KNOWS*]->(b))
  RETURN length(p) AS hops
`);
```

### MERGE (upsert)

`MERGE` works on both nodes and relationships. The pattern is matched
atomically; if nothing matches it is created. Pair with `ON CREATE` /
`ON MATCH` to set different properties on the create vs. match branches.

```js
// Node upsert
db.execute(`
  MERGE (n:Person {name: 'Alice'})
  ON CREATE SET n.created = true
  ON MATCH SET n.seen = true
`);

// Edge upsert — replaces a manual "DELETE existing + CREATE new" workaround
db.execute(`
  MATCH (a:Fn {name: 'foo'}), (b:Fn {name: 'bar'})
  MERGE (a)-[r:CALLS]->(b)
  ON CREATE SET r += {lineno: 10, col: 5}
  ON MATCH  SET r += {lineno: 10, col: 5}
`);
```

Edge identity: `MERGE (a)-[:R]->(b)` finds an existing `(a)-[:R]->(b)`
edge if any exists. `MERGE (a)-[:R {x: 1}]->(b)` is constrained by the
inline properties — a parallel `(a)-[:R {x: 2}]->(b)` will not be
matched, and a new edge is created. Direction is significant.

### Bulk insert with UNWIND

```js
db.execute(`
  UNWIND ['Eve', 'Frank', 'Grace'] AS name
  CREATE (n:Person {name: name})
`);
```

## Error handling

All methods throw a `Error` on failure (parse errors, constraint violations, etc.).

```js
try {
  db.query('MATCH (n:Invalid Syntax');
} catch (err) {
  console.error('Query failed:', err.message);
}
```

For transactions, always rollback (write) or commit (read) in the error path to release
the connection:

```js
const tx = db.beginWrite();
try {
  tx.execute("CREATE (n:Person {name: 'Alice'})");
  tx.commit();
} catch (err) {
  tx.rollback();
  throw err;
}
```

## Rust API

For lower-level access (direct node/edge CRUD, index management, traversal), use the
Rust API. See [rust.md](rust.md) for the full reference.
