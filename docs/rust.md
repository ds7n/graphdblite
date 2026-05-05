# Rust API

## Installation

Add graphdblite as a git dependency in your `Cargo.toml`:

```toml
[dependencies]
graphdblite = { git = "https://github.com/ds7n/graphdblite.git" }
```

## Example

```rust
use graphdblite::{Database, Value};
use std::collections::HashMap;

fn main() -> graphdblite::Result<()> {
    let mut db = Database::open("my.db")?;

    // Write transaction — `WriteTxGuard` rolls back on drop if not committed.
    let mut tx = db.write_tx()?;
    tx.query("CREATE (a:Person {name: 'Alice', age: 30})")?;
    tx.query("CREATE (b:Person {name: 'Bob', age: 25})")?;
    tx.query("MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)")?;
    tx.create_index("Person", "name")?;
    tx.commit()?;

    // Read transaction
    let tx = db.read_tx()?;
    let results = tx.query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name")?;
    for record in &results {
        println!("{:?} knows {:?}", record.get("a.name"), record.get("b.name"));
    }
    tx.commit()?;

    Ok(())
}
```

## Database

```rust
// Open or create a database file
let mut db = Database::open("path.db")?;

// Open with custom configuration
let config = Config {
    busy_timeout_ms: 10_000,       // 10s write lock timeout
    synchronous: "FULL".into(),    // fsync every commit (zero-loss durability)
};
let mut db = Database::open_with_config("path.db", config)?;

// In-memory database (for testing)
let mut db = Database::open_memory()?;
```

### Config

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `busy_timeout_ms` | `u32` | `5000` | Milliseconds to wait for write lock |
| `synchronous` | `String` | `"NORMAL"` | SQLite sync mode (`"NORMAL"` or `"FULL"`) |

## Transactions

The Rust API exposes two transaction styles backed by the same engine.

### Recommended: RAII guards (`write_tx` / `read_tx`)

```rust
let tx = db.read_tx()?;   // ReadTxGuard — snapshot-isolated read
let mut tx = db.write_tx()?;  // WriteTxGuard — BEGIN IMMEDIATE
```

`WriteTxGuard` and `ReadTxGuard` deref to the typed transaction, so the
methods listed below in `ReadTransaction` / `WriteTransaction` are reachable
directly on the guard. Drop without `commit()` rolls back automatically (the
write guard also emits a `tracing::warn!`).

### Stateful API (primarily for binding implementations)

```rust
db.begin_write()?;                 // -> Result<()>
db.execute("CREATE (:Person {name: 'Alice'})")?;
db.commit()?;
```

`begin_write` / `begin_read` / `execute` / `execute_with_params` /
`commit` / `rollback` form the stateful lifecycle every language binding
consumes. Mixing the two styles on a single `Database` handle is rejected at
runtime (each style takes the txn slot exclusively).

When no transaction is active, `execute` / `execute_with_params`
auto-begin/auto-commit a transaction (read-only plans use `BEGIN
DEFERRED`, writes use `BEGIN IMMEDIATE`). On error the auto-txn is
rolled back. Multi-statement transactions still need explicit
`begin_*` + `commit`.

## ReadTransaction

Reachable via deref on `ReadTxGuard` and `WriteTxGuard`.

| Method | Description |
|--------|-------------|
| `query(cypher: &str) -> Result<Vec<Record>>` | Execute a Cypher query |
| `get_node(id: NodeId) -> Result<Node>` | Fetch a node by ID |
| `node_exists(id: NodeId) -> Result<bool>` | Check if a node exists |
| `get_neighbors(id: NodeId, label: &str, direction: Direction) -> Result<Vec<NodeId>>` | Get adjacent node IDs |
| `get_edge_properties(src: NodeId, dst: NodeId, label: &str) -> Result<Properties>` | Get properties on an edge |
| `find_nodes_by_label(label: &str) -> Result<Vec<Node>>` | Find all nodes with a label |
| `index_lookup(label: &str, property: &str, value: &Value) -> Result<Vec<NodeId>>` | O(log n) index lookup |
| `traverse(start: NodeId, label: &str, direction: Direction, min_hops: u32, max_hops: u32) -> Result<Vec<NodeId>>` | Variable-length traversal |
| `commit(self) -> Result<()>` | Commit and release transaction |

## WriteTransaction

Reachable via deref on `WriteTxGuard`. All `ReadTransaction` methods, plus:

| Method | Description |
|--------|-------------|
| `create_node(label: &str, properties: Properties) -> Result<NodeId>` | Create a node, returns its ID |
| `delete_node(id: NodeId) -> Result<()>` | Delete a node (must have no edges) |
| `set_node_property(id: NodeId, key: &str, value: Value) -> Result<()>` | Set a property on a node |
| `remove_node_property(id: NodeId, key: &str) -> Result<()>` | Remove a property from a node |
| `create_edge(src: NodeId, dst: NodeId, label: &str, properties: Properties) -> Result<()>` | Create a directed edge |
| `delete_edge(src: NodeId, dst: NodeId, label: &str) -> Result<()>` | Delete an edge |
| `create_index(label: &str, property: &str) -> Result<()>` | Create a secondary property index |
| `drop_index(label: &str, property: &str) -> Result<()>` | Drop a secondary property index |
| `commit(self) -> Result<()>` | Commit and release write lock |
| `rollback(self) -> Result<()>` | Explicitly rollback |

## Types

### Value

```rust
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
    List(Vec<Value>),
    Node(Node),
    Edge(Edge),
    Path(PathValue),
    Map(BTreeMap<String, Value>),
    Date(CypherDate),
    LocalTime(CypherLocalTime),
    Time(CypherTime),
    LocalDateTime(CypherLocalDateTime),
    DateTime(CypherDateTime),
    Duration(CypherDuration),
}
```

### Node / Edge / Path

```rust
pub struct NodeId(pub u64);

pub struct Node {
    pub id: NodeId,
    pub labels: Vec<String>,        // multi-label support
    pub properties: Properties,     // HashMap<String, Value>
}

pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub label: String,
    pub properties: Properties,
}

pub struct PathValue {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}
```

### Record

Query results are returned as `Vec<Record>`. Each record is a row of named values.

```rust
pub struct Record {
    pub fields: HashMap<String, Value>,
}

impl Record {
    pub fn get(&self, key: &str) -> Option<&Value>
    pub fn set(&mut self, key: String, value: Value)
}
```

## Error handling

All fallible methods return `Result<T>`, where the error type is `GraphError`:

```rust
pub enum GraphError {
    Storage(rusqlite::Error),          // SQLite errors
    Serialization(String),             // Encoding/decoding errors
    ParseError(String),                // Cypher parse errors
    NodeNotFound(NodeId),              // Node doesn't exist
    EdgeNotFound(NodeId, String, NodeId), // Edge doesn't exist
    HasEdges(NodeId),                  // Can't delete node with edges (use DETACH DELETE)
    Transaction(String),               // Transaction errors
    IndexAlreadyExists(String, String), // Index already exists
    IndexNotFound(String, String),     // Index doesn't exist
}
```
