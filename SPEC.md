# graphdblite

## Overview

An embedded graph database for applications that need graph queries without running a server. Uses SQLite as the storage engine (one file, zero deployment), speaks a substantial subset of openCypher, and exposes bindings for Rust, Python, Node.js, Go, and C. Think "SQLite for graphs" — the same single-file, multi-process, zero-config philosophy applied to property graph workloads.

## Architecture

### Storage: SQLite-as-KV

The database is a single SQLite file. Rather than mapping graph structures into relational tables, graphdblite uses SQLite as an ordered key-value store via four `WITHOUT ROWID` tables:

| Table | Key scheme | Value |
|-------|-----------|-------|
| `nodes` | `[node_id: 8B BE]` | MessagePack-serialized `NodeRecord` (labels + properties). Also has a `label` TEXT column for indexed label scans. |
| `adj_out` | `[src_id][label][dst_id]` | Varint-encoded adjacency list |
| `adj_in` | `[dst_id][label][src_id]` | Varint-encoded adjacency list (reverse index) |
| `edge_props` | `[src_id][dst_id][label]` | MessagePack-serialized edge properties |

A `metadata` table tracks schema version and the node ID counter. Secondary indexes are dynamically created per `(label, property)` pair as separate tables.

**Why SQLite-as-KV:** Avoids impedance mismatch between graph traversals and SQL joins. Prefix scans on byte-ordered keys give O(1) neighbor lookups. SQLite's WAL mode provides multi-process concurrent reads with single-writer semantics — no application-level locking needed.

### Concurrency model

- WAL journal mode, `SYNCHRONOUS=NORMAL` by default
- `ReadTransaction` uses `BEGIN DEFERRED` — snapshot isolation, concurrent with other readers and one writer
- `WriteTransaction` uses `BEGIN IMMEDIATE` — acquires write lock upfront to avoid upgrade deadlocks
- Each `Database` instance owns one SQLite connection. Multiple processes can open the same file safely via WAL. Multiple in-process handles are independent (no shared state, no global mutexes).

### Query pipeline

```
Cypher string
  → pest grammar (grammar.pest, 451 rules)
  → Parser (parser.rs → AST)
  → Planner (planner.rs → LogicalOp IR)
  → Executor (executor.rs — write ops)
    / Iterator (iter.rs — pull-based streaming for reads)
  → Vec<Record>
```

- **Parser:** Hand-rolled from pest PEG grammar. Handles the full openCypher expression language including temporal constructors, pattern comprehensions, list comprehensions, quantifier predicates, and EXISTS subqueries.
- **Planner:** Translates AST → `LogicalOp` IR. Applies index lookups when available, handles multi-clause statement threading, correlated subqueries.
- **Executor:** Two modes — pull-based `RecordIter` trait for read-only plans (streams without full materialization), push-based `exec()` for write operations.
- **Eval:** Expression evaluator (`eval.rs`) with three-valued null logic, NaN handling, openCypher type coercion rules.

### Module responsibilities

| Module | Purpose |
|--------|---------|
| `db.rs` | `Database` struct, connection init, transaction factory |
| `transaction.rs` | `ReadTransaction` / `WriteTransaction` with Cypher `query()` |
| `node.rs` | Node CRUD against the `nodes` KV table |
| `edge.rs` | Edge CRUD, adjacency lists, BFS traversal |
| `index.rs` | Secondary index creation, lookup, maintenance |
| `storage/kv.rs` | Raw get/put/delete/scan_prefix over SQLite KV tables |
| `storage/encoding.rs` | Varint encoding, sorted ID list operations |
| `types.rs` | `Value`, `Node`, `Edge`, `PathValue`, error types |
| `temporal.rs` | Cypher temporal types (Date, Time, DateTime, Duration) |
| `schema.rs` | DDL, schema versioning, migrations |
| `cypher/` | Grammar, parser, AST, IR, planner, executor, evaluator |
| `bin/cli.rs` | CLI with single-query (`-q`) and REPL modes |

### Language bindings

| Binding | Location | Interface |
|---------|----------|-----------|
| **Rust** | `src/` (core crate) | `Database`, `ReadTransaction`, `WriteTransaction` |
| **Python** | `bindings/python/` | PyO3-based, `PyDatabase` / `PyWriteTransaction` / `PyReadTransaction` |
| **Node.js** | `bindings/node/` | NAPI-RS, `Database` / `WriteTransaction` / `ReadTransaction` |
| **Go** | `bindings/go/` | CGo via FFI, `Database` / `WriteTransaction` / `ReadTransaction` |
| **C** | `bindings/ffi/` | cbindgen-generated header, `GraphDB` / `GraphResult` opaque handles |

All bindings expose: `open`, `open_memory`, `begin_read`, `begin_write`, `query`, `execute`, `commit`, `rollback`.

## Supported Features

### Cypher clauses
- `MATCH` — node patterns, relationship patterns (directed, undirected), multi-hop
- `OPTIONAL MATCH` — left outer join semantics with null-filling
- `WHERE` — full expression filtering
- `RETURN` / `RETURN DISTINCT` — projection with aliases, `RETURN *`
- `ORDER BY` (ASC/DESC/ASCENDING/DESCENDING), `SKIP`, `LIMIT`
- `WITH` / `WITH DISTINCT` — intermediate projection, aggregation, filtering
- `CREATE` — nodes and relationships, multi-pattern
- `MERGE` — with `ON CREATE SET` / `ON MATCH SET`
- `SET` — property assignment, label assignment, map merge (`+=`), map replace (`=`)
- `REMOVE` — property removal, label removal
- `DELETE` / `DETACH DELETE` — node and relationship deletion
- `UNWIND` — list expansion
- `UNION` / `UNION ALL`
- `EXPLAIN` — query plan visualization
- Multi-clause statements — arbitrary sequences of the above

### Expressions
- Arithmetic: `+`, `-`, `*`, `/`, `%`, `^` (exponentiation)
- Comparison: `=`, `<>`, `<`, `>`, `<=`, `>=`, chained comparisons
- Boolean: `AND`, `OR`, `XOR`, `NOT` (three-valued logic)
- String: `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `=~` (regex)
- Null: `IS NULL`, `IS NOT NULL`
- Lists: indexing `[n]`, slicing `[a..b]`, `IN`, list comprehensions `[x IN list WHERE pred | expr]`
- Pattern comprehensions: `[(n)-[:R]->(m) | m.prop]`
- Maps: literal `{k: v}`, property access `n.prop`, dynamic access `n['key']`, chained `n.a.b`
- `CASE WHEN ... THEN ... ELSE ... END` (simple and generic forms)
- Quantifier predicates: `any()`, `all()`, `none()`, `single()`
- `EXISTS { MATCH ... WHERE ... }` subqueries
- Parameters: `$param` substitution

### Functions
- **Aggregation:** `count`, `sum`, `avg`, `min`, `max`, `collect`, `percentileDisc`, `percentileCont`, `stDev`, `stDevP` — all support `DISTINCT`
- **Scalar:** `id`, `labels`, `type`, `keys`, `properties`, `length`, `size`, `head`, `last`, `tail`, `range`, `coalesce`, `toInteger`, `toFloat`, `toString`, `toBoolean`, `reverse`, `nodes`, `relationships`, `startNode`, `endNode`, `rand`, `abs`, `ceil`, `floor`, `round`, `sign`, `sqrt`, `log`, `log10`, `exp`, `e`, `pi`
- **String:** `trim`, `ltrim`, `rtrim`, `toUpper`, `toLower`, `replace`, `substring`, `split`, `left`, `right`
- **Temporal constructors:** `date()`, `localtime()`, `time()`, `localdatetime()`, `datetime()`, `duration()` — from strings, maps, epoch values, `realtime`/`statement`/`transaction` clocks
- **Temporal functions:** `date.truncate()`, `time.truncate()`, etc., `duration.between()`, `duration.inMonths/inDays/inSeconds()`
- **Temporal arithmetic:** date ± duration, duration ± duration, duration × number
- **Temporal accessors:** `.year`, `.month`, `.day`, `.hour`, `.minute`, `.second`, `.millisecond`, `.microsecond`, `.nanosecond`, `.timezone`, `.epochSeconds`, `.epochMillis`, etc.

### Indexes
- Secondary indexes on `(label, property)` pairs via `CREATE INDEX`
- Automatically used by the planner for equality lookups in WHERE clauses
- Backfilled on creation, maintained on writes

### Error model
- openCypher-aligned error taxonomy: `SyntaxError`, `TypeError`, `SemanticError`, `EntityNotFound`, `ArgumentError`, `ArithmeticError`, `ConstraintViolation`
- Each error carries a `QueryPhase` (Parse, SemanticAnalysis, Runtime)
- Compile-time validation: undefined variables, duplicate aliases, type mismatches, invalid aggregation nesting

### TCK conformance
- Full openCypher TCK vendored (220 feature files, 3639 scenarios)
- **97.4% pass rate** (3457/3639), 182 skiplisted — as of 2026-04-25
- Regenerate stats: `cargo test --test tck 2>&1 > /tmp/tck_output.txt && uv run tests/tck/analyze.py /tmp/tck_output.txt`
- Regenerate blocker analysis: `uv run tests/tck/analyze_blockers.py`

## Known Limitations

### Storage model
- **Single edge per (src, dst, label) triple** — the edge_props key is `[src][dst][label]` with no edge ID, so a second `CREATE` with the same triple silently overwrites the first. No multi-edges with the same type between the same node pair.
- **Monolithic adjacency BLOBs** — each `(node, label)` pair stores all neighbors in one delta-varint BLOB. The entire list is decoded on every lookup, and write amplification is O(degree) per edge insert. Hub nodes (>100K edges of one type) will degrade.
- **No schema constraints** — no uniqueness, existence, or property type constraints.
- **No full-text search** — no SQLite FTS integration.

### Query engine
- **Label scans fully materialize** — `MATCH (n:Label)` loads all matching nodes into memory before any downstream LIMIT can stop it. Multi-label queries (`[:A:B]`) fall back to `LIKE` scans that bypass the label index.
- **Blocking operators** — `ORDER BY`, `DISTINCT`, and aggregation materialize the full input before proceeding. No streaming sort or top-N optimization.
- **O(n²) dedup in places** — `DISTINCT`, `UNION`, and traversal result dedup use `Vec::contains` instead of `HashSet`.
- **No query cache** — every query is parsed and planned from scratch.
- **Result row limit** — default 100K rows per query (configurable, 0 = unlimited).

### Concurrency
- **Single-writer** — WAL allows concurrent readers but only one writer at a time (SQLite limitation).

### Scale
Designed for datasets in the **low millions of nodes** with moderate edge density. 10M nodes is achievable for indexed point-lookups; full label scans at that scale will be slow. 100M+ nodes would require rearchitecting the scan layer (streaming from SQLite instead of materializing), replacing linear-scan dedup with `HashSet`, and making adjacency lists appendable without full rewrite.

### TCK gaps (182 skiplisted scenarios)
The remaining TCK failures are not one big gap — they span several specific sub-features:
- Path binding in MERGE/CREATE (`p = (a)-[:R]->(b)`)
- Multi-hop CREATE patterns with complex direction chains
- Deleted entity access detection after DELETE
- Expression-selected targets for SET/REMOVE
- Variable-length relationship edge cases
- Error validation for additional error categories
- Temporal type edge cases

Regenerate blocker analysis: `uv run tests/tck/analyze_blockers.py`

## Non-Goals

- **Not a server.** No network protocol, no client-server architecture. Embedded library only.
- **Not a distributed database.** Single file, single machine. No replication.
- **Not an ORM.** Cypher queries in, records out. No object mapping layer.
- **Not for billion-node graphs.** Targets workloads that fit in SQLite (low millions of nodes). For larger scales, use a dedicated graph database.
