# graphdblite — Design Document

> Embedded graph database with Cypher support. SQLite-grade simplicity, graph-native performance.

## Positioning

"SQLite but for graphs." Lightweight, embeddable, single-file, multi-process-safe.
Not competing with Neo4j/TigerGraph on billion-edge workloads. Filling the gap
left by Kùzu (acquired by Apple, archived Oct 2025).

First target user: symtext (code analysis graph across multiple projects/Claude instances).

## Architecture

```
┌──────────────────────────────────────┐
│           graphdblite                │
│  ┌────────────┐  ┌────────────────┐  │
│  │  Cypher     │  │  Typed Rust    │  │
│  │  Parser     │  │  API           │  │
│  │  (pest/     │  │  (get_neighbors│  │
│  │   LALRPOP)  │  │   find_nodes,  │  │
│  │             │  │   create, etc) │  │
│  └──────┬──────┘  └───────┬────────┘  │
│         └────────┬────────┘           │
│          ┌───────┴────────┐           │
│          │  Logical IR     │           │
│          │  (Scan, Expand, │           │
│          │   Filter, Join, │           │
│          │   Aggregate,    │           │
│          │   Project)      │           │
│          └───────┬─────────┘           │
│          ┌───────┴─────────┐           │
│          │  Executor        │           │
│          │  (iterator model,│           │
│          │   BFS/DFS,       │           │
│          │   hash join)     │           │
│          └───────┬──────────┘           │
│          ┌───────┴──────────┐           │
│          │  Graph Storage    │           │
│          │  (adjacency blobs,│          │
│          │   node records,   │          │
│          │   secondary idx)  │          │
│          └───────┬───────────┘          │
└──────────────────┼───────────────────┘
            ┌──────┴───────────┐
            │  SQLite (KV mode) │
            │  WITHOUT ROWID    │
            │  tables, WAL,     │
            │  advisory locks   │
            └──────────────────┘
```

### Key design principle

> Every piece of state is owned by a specific `Database` instance. Zero module-level,
> static, or singleton state. Zero global mutexes. Two `Database` handles in the same
> process must have no shared mutable state.

This prevents the cross-DB contention bug that LadyBugDB (Kùzu fork) exhibits.

## Decisions

### 1. Storage — SQLite as KV engine

SQLite is used as a B-tree + WAL engine, **not** as a relational database. No JOINs,
no SQL query planning. Graph-native data structures stored as opaque binary blobs in
`WITHOUT ROWID` tables (pure B-trees).

**Why SQLite:**
- Multi-process write safety is battle-tested (15+ years in WAL mode)
- Crash recovery, fsync correctness, cross-platform portability — all solved
- The hardest 20% of database engineering (by risk), delegated to the most tested
  software in existence

**Why not raw SQL tables:**
- Impedance mismatch: graph traversals become JOINs, variable-length paths become
  recursive CTEs
- SQLite's query planner has no concept of graph adjacency
- We want graph-native storage layout with SQLite handling only bytes-on-disk

### 2. Language — Rust

- Memory safety in transaction/concurrency code (borrow checker prevents the bug classes
  that corrupt databases)
- Matches the ecosystem (Kùzu, CozoDB, Grafeo, Oxigraph, IndraDB — all Rust)
- `rusqlite` crate is mature for SQLite interop
- Python/Go/JS bindings via FFI/PyO3/napi-rs added later

### 3. Scale target — Small, medium ceiling

- **Designed for:** <1M nodes, <5M edges (single project symtext index)
- **Ceiling:** 1M–50M nodes, 10M–200M edges (org-wide, mid-size knowledge graph)
- **Explicitly out of scope:** 50M+ nodes, 1B+ edges (social networks, large analytics)

### 4. Durability — SQLite WAL default

- `PRAGMA synchronous=NORMAL` (SQLite WAL default): fsync at checkpoint, not every commit
- Never corrupt on crash; tiny window of uncheckpointed transaction loss on power failure
- `synchronous=FULL` available as opt-in config flag for zero-loss guarantees
- Either mode is imperceptible at target write rates (<100 tps)

### 5. Query language — Usable Cypher subset

v0.1 supports:

```cypher
-- Pattern matching
MATCH (a:Label)-[:EDGE_TYPE]->(b:Label)
MATCH (a)-[:TYPE*1..N]->(b)              -- variable-length paths

-- Filtering and projection
WHERE a.property = value
RETURN a.name, count(*) AS cnt
ORDER BY cnt DESC
LIMIT 10

-- Mutations
CREATE (n:Label {key: value})
CREATE (a)-[:TYPE]->(b)
DELETE n
SET n.property = value
MERGE (n:Label {key: value})
ON CREATE SET n.prop = val
```

**Not in v0.1:** subqueries, `CALL` procedures, `UNWIND`, `EXISTS{}` patterns,
list comprehensions, `CASE` expressions, `OPTIONAL MATCH`, shortest-path functions.

**Future languages:** The Logical IR is language-agnostic. Additional parsers (GQL,
SPARQL, Gremlin) compile to the same IR without touching storage or execution.

### 6. Multi-process concurrency — SQLite handles it

No custom lock code. SQLite's WAL mode provides exactly the protocol we need:

- **Read:** `BEGIN` — lock-free, instant, MVCC snapshot
- **Write:** `BEGIN IMMEDIATE` — acquires write lock, blocks up to `busy_timeout`
- **Commit:** `COMMIT` — fsync per durability setting, releases write lock
- **Crash:** SQLite detects stale lock, next writer rolls back automatically
- **Timeout:** returns `SQLITE_BUSY` as a graphdblite error; caller decides retry

Default `busy_timeout`: 5000ms (configurable).

Locks are per-database-file (SQLite's native behavior). Opening DB A has zero
effect on DB B in the same process.

## KV Schema

```
Table: nodes
  Key:   node_id (u64, big-endian)
  Value: msgpack/bincode { label: str, properties: map }

Table: node_idx_{label}_{property}
  Key:   property_value + node_id
  Value: (empty — key-only index)

Table: adj_out
  Key:   src_node_id (u64) + edge_label (str)
  Value: packed array of dst_node_ids (sorted, varint-encoded)

Table: adj_in
  Key:   dst_node_id (u64) + edge_label (str)
  Value: packed array of src_node_ids (sorted, varint-encoded)

Table: edge_props
  Key:   src_node_id + edge_label + dst_node_id
  Value: msgpack/bincode { properties: map }
```

All tables are `WITHOUT ROWID` (pure B-tree, no rowid overhead).

### Write path

- **Insert node:** 1 put (nodes) + N puts (indexes). Cheap.
- **Insert edge:** 2 reads (adj_out, adj_in) + 2 writes (modified adjacency blobs)
  + 1 optional write (edge_props). Read-modify-write is the tax for fast reads.
- **Delete node:** cascading — must read all adjacency lists and update neighbors.
  Cost proportional to degree.
- **Bulk operations:** batch in one SQLite transaction, single fsync on commit.

### Read path

- **Find node by property:** index lookup → O(log n)
- **Get neighbors:** single adjacency blob fetch → O(log n) for lookup, O(degree) to decode
- **Variable-length path:** iterative expansion, one adjacency fetch per hop per node
- **Full label scan:** sequential B-tree scan of nodes table with label filter

## Logical IR

Language-agnostic intermediate representation. All query languages compile to this.

```
Operators:
  Scan(label, filter?)          — enumerate nodes by label
  IndexLookup(label, prop, val) — point lookup via secondary index
  Expand(direction, edge_label, min_hops, max_hops) — traverse edges
  Filter(expression)            — predicate evaluation
  HashJoin(left, right, keys)   — join two streams
  Aggregate(group_keys, accumulators) — count, collect, sum, avg, min, max
  Sort(keys, direction)         — ORDER BY
  Limit(count)                  — LIMIT
  Project(columns)              — RETURN
  Create(pattern)               — node/edge creation
  Delete(identifiers)           — node/edge deletion
  SetProperty(identifier, key, value) — SET
  Merge(pattern, on_create?, on_match?) — MERGE
```

Executor uses a pull-based iterator (Volcano) model. Each operator implements
`fn next() -> Option<Record>`.

## Implementation Phases

### Phase 1 — Storage layer (~2-3 weeks)
- Rust project setup, `rusqlite` integration
- KV abstraction over SQLite WITHOUT ROWID tables
- Node/edge CRUD operations
- Adjacency blob encoding (sorted, varint-compressed)
- Secondary index creation and maintenance
- Transaction wrapper (begin/commit/rollback)
- Multi-process smoke tests (spawn N processes, hammer writes)

### Phase 2 — Typed Rust API (~1-2 weeks)
- `Database::open(path)` / `Database::open_with_config(path, config)`
- `db.create_node(label, properties) -> NodeId`
- `db.create_edge(src, dst, label, properties)`
- `db.get_node(id) -> Node`
- `db.get_neighbors(id, label, direction) -> Vec<NodeId>`
- `db.find_nodes(label, property, value) -> Vec<NodeId>`
- `db.delete_node(id)` / `db.delete_edge(src, dst, label)`
- `db.set_property(id, key, value)`
- `db.traverse(start, label, direction, min_hops, max_hops) -> Vec<NodeId>`
- Transaction support: `db.begin_write()`, `db.begin_read()`

### Phase 3 — Cypher parser + planner (~4-6 weeks)
- Cypher lexer/parser using `pest` or `LALRPOP`
- AST → Logical IR compilation
- Rule-based planner (pattern → index lookup + expand + filter)
- Variable-length path → BFS/DFS operator
- Aggregation operator (count, collect, sum, avg, min, max)
- Sort + Limit operators

### Phase 4 — Executor + integration (~2-3 weeks)
- Volcano-model pull iterator execution engine
- Expression evaluator (comparisons, boolean logic, property access)
- MERGE semantics (match-or-create)
- End-to-end tests: Cypher string in → results out
- Error handling and user-facing error messages

### Phase 5 — Polish + bindings (~2-3 weeks)
- Python bindings (PyO3)
- CLI tool for ad-hoc queries
- Configuration (busy_timeout, synchronous mode, page cache size)
- Documentation
- Benchmarks against LadyBugDB/GraphQLite on symtext workload
- Packaging (crates.io, PyPI)

**Total estimated timeline: ~12-18 weeks**

## Open questions (to resolve during implementation)

1. **Serialization format:** msgpack vs bincode vs custom. Bincode is faster but
   Rust-specific; msgpack is language-neutral for future bindings.
2. **Node ID generation:** sequential u64 (simple, compact) vs UUID (globally unique,
   no coordination needed for multi-source ingestion)?
3. **Adjacency blob chunking:** at what degree should we split into chunks?
   Probably 10k+ edges per (node, label) pair — rare at target scale.
4. **Parser choice:** `pest` (PEG, easier to write) vs `LALRPOP` (LALR, better error
   messages, more traditional)?
5. **Cost-based optimizer:** not in v0.1, but the Logical IR should be designed to
   support one later (statistics collection, cardinality estimation).
