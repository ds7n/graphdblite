# Architecture

## Pipeline

```
Cypher query string
       │
       ▼
┌─────────────┐
│ Parser      │  pest PEG grammar → AST
│ (pest)      │
└──────┬──────┘
       │
       ▼
┌─────────────┐
│ Planner     │  AST → Logical IR (Scan, Expand, Filter, Join, etc.)
│             │  Index-aware: converts filters to IndexLookup when possible
└──────┬──────┘
       │
       ▼
┌─────────────┐
│ Cost        │  Cardinality estimation, index selection
│ Estimator   │  Uses label statistics when available
└──────┬──────┘
       │
       ▼
┌─────────────┐
│ Executor    │  Volcano pull-based iterator model
│             │  Each operator: fn next() → Option<Record>
└──────┬──────┘
       │
       ▼
┌─────────────┐
│ Graph       │  Adjacency blobs, node records, secondary indexes
│ Storage     │  msgpack encoding, sorted varint arrays
└──────┬──────┘
       │
       ▼
┌─────────────┐
│ SQLite      │  WAL mode, WITHOUT ROWID tables (pure B-trees)
│ (KV mode)   │  No SQL JOINs, no relational planning
└─────────────┘
```

## Storage design

SQLite is used as a B-tree + WAL engine, **not** as a relational database. Graph-native
data structures are stored as opaque binary blobs in `WITHOUT ROWID` tables.

### Tables

| Table | Key | Value |
|-------|-----|-------|
| `nodes` | `node_id` (u64, big-endian) | msgpack `{ label, properties }` |
| `node_idx_{label}_{property}` | `property_value + node_id` | (empty — key-only index) |
| `adj_out` | `src_node_id + edge_label` | sorted varint array of `dst_node_ids` |
| `adj_in` | `dst_node_id + edge_label` | sorted varint array of `src_node_ids` |
| `edge_props` | `src_node_id + edge_label + dst_node_id` | msgpack `{ properties }` |

### Write path

- **Insert node:** 1 put (nodes) + N puts (indexes)
- **Insert edge:** 2 reads + 2 writes (adjacency blobs, read-modify-write) + 1 optional write (edge_props)
- **Delete node:** cascading — reads all adjacency lists and updates neighbors. Cost proportional to degree

### Read path

- **Find node by property:** index lookup → O(log n)
- **Get neighbors:** single adjacency blob fetch → O(log n) lookup, O(degree) to decode
- **Variable-length path:** iterative expansion, one adjacency fetch per hop per node
- **Full label scan:** sequential B-tree scan of nodes table with label filter

## Concurrency

SQLite WAL mode handles multi-process reads and writes. No custom lock code.

- **Read:** `BEGIN` — lock-free, instant, MVCC snapshot isolation
- **Write:** `BEGIN IMMEDIATE` — acquires write lock, blocks up to `busy_timeout`
- **Commit:** fsync per durability setting, releases write lock
- **Crash:** SQLite detects stale lock, next writer rolls back automatically
- **Timeout:** returns `SQLITE_BUSY` as a `GraphError::Storage`; caller decides retry

Locks are per-database-file. Opening database A has zero effect on database B in the
same process.

### Key design principle

> Every piece of state is owned by a specific `Database` instance. Zero module-level,
> static, or singleton state. Zero global mutexes. Two `Database` handles in the same
> process have no shared mutable state.

## Indexing

Secondary property indexes enable O(log n) lookups by property value. Without an index,
property filters require a full label scan.

Indexes are created via the Rust API:

```rust
let tx = db.begin_write()?;
tx.create_index("Person", "name")?;
tx.commit()?;
```

The query planner automatically detects index-friendly `WHERE` filters and converts
`Scan + Filter` into `IndexLookup` when an index exists.

## Cost-based optimizer

The planner uses cardinality estimation to choose between scan strategies:

- **Label cardinality:** uses actual node count when statistics are available, otherwise defaults to 1000
- **Filter selectivity:** 0.3 (30% pass-through)
- **Expand fan-out:** 5.0 edges per node
- **Aggregation:** 50% cardinality reduction

Use `EXPLAIN` to inspect the logical plan:

```cypher
EXPLAIN MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = 'Alice' RETURN b.name
```

This shows the operator tree with estimated row counts, useful for understanding
which indexes are being used and how the planner structures the query.

## Logical IR

The intermediate representation is language-agnostic — designed so that future query
language parsers (GQL, SPARQL, Gremlin) can compile to the same IR without touching
storage or execution.

| Operator | Description |
|----------|-------------|
| `Scan(label, filter?)` | Enumerate nodes by label |
| `IndexLookup(label, prop, val)` | Point lookup via secondary index |
| `Expand(direction, edge_label, min_hops, max_hops)` | Traverse edges |
| `Filter(expression)` | Predicate evaluation |
| `HashJoin(left, right, keys)` | Join two streams |
| `Aggregate(group_keys, accumulators)` | count, collect, sum, avg, min, max |
| `Sort(keys, direction)` | ORDER BY |
| `Limit(count)` | LIMIT |
| `Project(columns)` | RETURN |
| `Create(pattern)` | Node/edge creation |
| `Delete(identifiers)` | Node/edge deletion |
| `SetProperty(identifier, key, value)` | SET |
| `Merge(pattern, on_create?, on_match?)` | MERGE |
