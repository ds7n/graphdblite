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
| `edge_props` | `[src_id][dst_id][label][0x00][seq]` | MessagePack-serialized edge properties. Sequence byte supports multiple parallel edges per `(src, dst, type)` triple. |

A `metadata` table tracks schema version and the node ID counter. Secondary indexes are dynamically created per `(label, property)` pair as separate tables. Every database file is stamped with SQLite `application_id = 0x4744424C` ("GDBL") and `user_version = 2` in the file header. `Database::open` rejects files whose `application_id` is set to a different value, files with `application_id = 0` that contain non-graphdblite tables, or tables named like graphdblite's (`nodes`, `adj_out`, `adj_in`, `edge_props`, `metadata`) without graphdblite's columns (catches "I pointed graphdblite at a foreign SQLite file" mistakes without modifying the file), files with triggers / views on graphdblite tables, and files whose `user_version` is newer than this build supports. Version 0/1 files are migrated on open (see `docs/sqlite-application-id.md`).

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
  → Parser (parser/ → AST)
  → Planner (planner/ → LogicalOp IR)
  → Executor (executor/ — write ops)
    / Iterator (iter.rs — pull-based streaming for reads)
  → Vec<Record>
```

- **Parser:** Hand-rolled from pest PEG grammar. Handles the full openCypher expression language including temporal constructors, pattern comprehensions, list comprehensions, quantifier predicates, and EXISTS subqueries.
- **Planner:** Translates AST → `LogicalOp` IR. Applies index lookups when available, handles multi-clause statement threading, correlated subqueries. Pushes `LIMIT N` caps into var-length `Expand` operators when the chain between them is row-preserving.
- **Executor:** Two modes — pull-based `RecordIter` trait for read-only plans (streams without full materialization), push-based `exec()` for write operations. Correlated execution (subqueries, EXISTS, multi-MATCH) lives in `executor/correlated.rs`.
- **Eval:** Expression evaluator (`eval/`) with three-valued null logic, NaN handling, openCypher type coercion rules.

### Module responsibilities

| Module | Purpose |
|--------|---------|
| `db.rs` | `Database` struct, connection init, stateful txn API (`begin_*`/`execute`/`commit`/`rollback`), typed-tx factory (`read_tx`/`write_tx`) |
| `transaction.rs` | Crate-internal `ReadTransaction` / `WriteTransaction` typed wrappers + `WriteTxGuard` / `ReadTxGuard` RAII guards returned by `Database::{read,write}_tx` |
| `node.rs` | Node CRUD against the `nodes` KV table |
| `edge.rs` | Edge CRUD, adjacency lists, BFS traversal |
| `index.rs` | Secondary index creation, lookup, maintenance |
| `storage/kv.rs` | Raw get/put/delete/scan_prefix over SQLite KV tables |
| `storage/encoding.rs` | Varint encoding, sorted ID list operations |
| `types.rs` | `Value`, `Node`, `Edge`, `PathValue`, error types |
| `temporal.rs` | Cypher temporal types (Date, Time, DateTime, Duration) |
| `schema.rs` | DDL, schema versioning, migrations |
| `cypher/` | Grammar, parser, AST, IR, planner, executor, evaluator |
| `cypher/parse_cache.rs` | Per-`Database` bounded-FIFO cache of parsed `Statement` ASTs (capacity 128). Parameter-agnostic — no DDL invalidation needed |
| `cypher/plan_cache.rs` | Per-`Database` bounded-FIFO cache of planned `LogicalOp` trees keyed on `(cypher, schema_epoch)` (capacity 128). DDL bumps `Database::schema_epoch: AtomicU64`, lazily invalidating prior plans. Param-keyed `IndexLookup` (`LookupKey::Param`) keeps plans reusable across `$param` values; CALL and SKIP/LIMIT-with-`$param` opt out via `is_plan_cacheable` |
| `bin/cli.rs` | CLI with single-query (`-q`) and REPL modes |

### Language bindings

| Binding | Location | Interface |
|---------|----------|-----------|
| **Rust** | `src/` (core crate) | `Database` (stateful: `begin_*`/`execute`/`commit`/`rollback`), `WriteTxGuard`/`ReadTxGuard` (RAII via `read_tx`/`write_tx`) |
| **Python** | `bindings/python/` | PyO3-based, `PyDatabase` / `PyWriteTransaction` / `PyReadTransaction` |
| **Node.js** | `bindings/node/` | NAPI-RS, `Database` / `WriteTransaction` / `ReadTransaction` |
| **Go** | `bindings/go/` | CGo via FFI, `Database` / `WriteTransaction` / `ReadTransaction` |
| **C** | `bindings/ffi/` | cbindgen-generated header, `GraphDB` / `GraphResult` opaque handles |

All language bindings (Python/Node/Go/C) drive the underlying database via the stateful API on `Database` (`begin_read`, `begin_write`, `execute`, `commit`, `rollback`). When no transaction is active, `execute*` auto-begins/auto-commits one — read-only plans use `BEGIN DEFERRED`, writes use `BEGIN IMMEDIATE`; on error the auto-tx is rolled back. The Rust core additionally exposes `WriteTxGuard`/`ReadTxGuard` via `read_tx`/`write_tx` for compile-time RAII safety.

Each binding ships a **conformance suite** (`docs/BINDING_CONFORMANCE.md`, scenarios `BC-01..BC-10`) covering transaction lifecycle, multi-process snapshot visibility, and resource hygiene. Write Cypher inside a read transaction is rejected by `cypher::execute_cypher` via `ExecContext::require_read_only`.

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
- String: `STARTS WITH`, `ENDS WITH`, `CONTAINS`
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
- openCypher-aligned error taxonomy: `SyntaxError`, `TypeError`, `SemanticError`, `EntityNotFound`, `ArgumentError`, `ArithmeticError`, `ConstraintViolation`, `ProcedureError`
- Each error carries `phase: QueryPhase`, `code: ErrorCode` (~30 structured openCypher codes), `message`, optional `hint`, optional `span` (source position)
- Display format: `Kind(Code) at <span|phase>: msg` with optional `hint:` line
- Parser and planner errors carry source spans (line:col): `Expr` is `{ kind: ExprKind, span: Span }`, with parser populating spans for both leaf and non-leaf expressions (literals, binary/unary ops, CASE, EXISTS, comprehensions, list/map literals, subscripts, etc.) and planner attaching them at scope-validation sites. `$param` references keep their source span — the planner treats `ExprKind::Parameter` as opaque and the executor resolves it at eval time, so the original span surfaces in any runtime type error.
- `UndefinedVariable` errors include `did you mean X?` hints from a Levenshtein search over the in-scope names
- Integer arithmetic uses checked operations — overflow raises `ArithmeticError(NumberOutOfRange)` rather than wrapping
- Integer division or modulo by zero raises `ArithmeticError(DivisionByZero)`; with a Float operand the result follows IEEE 754 (`Infinity` / `NaN`)
- Compile-time validation: undefined variables, duplicate aliases, type mismatches, invalid aggregation nesting

### TCK conformance
- Full openCypher TCK vendored (220 feature files, 3895 scenarios)
- **100% pass rate** (3895/3895), 2 skiplisted — as of 2026-05-07
- Regenerate blocker analysis: `uv run tests/tck/analyze_blockers.py`

## Known Limitations

### Storage model
- **Monolithic adjacency BLOBs** — each `(node, label)` pair stores all neighbors in one delta-varint BLOB. The entire list is decoded on every lookup, and write amplification is O(degree) per edge insert. Hub nodes (>100K edges of one type) will degrade.
- **No schema constraints** — no uniqueness, existence, or property type constraints.
- **No full-text search** — no SQLite FTS integration.

### Query engine
- **Label scans fully materialize** — `MATCH (n:Label)` loads all matching nodes into memory before any downstream LIMIT can stop it. Multi-label queries (`[:A:B]`) fall back to `LIKE` scans that bypass the label index.
- **Blocking operators** — `ORDER BY`, `DISTINCT`, and aggregation materialize the full input before proceeding. No streaming sort or top-N optimization.
- **O(n²) dedup in places** — `DISTINCT` and `UNION` use `Vec::contains` instead of `HashSet`.
- **Result row limit** — default 100K rows per query (configurable, 0 = unlimited).

### Concurrency
- **Single-writer** — WAL allows concurrent readers but only one writer at a time (SQLite limitation).

### Resource limits (defensive caps)
- Query input: 1 MiB (`MAX_QUERY_BYTES` in `parser/mod.rs`)
- Expression nesting depth: 256 (`MAX_EXPR_DEPTH`). This is the depth of the parsed expression tree, so it counts operator chains (`1 + 1 + ...`, `NOT NOT ...`, `a AND b AND ...`, `- - 1`, `m.a.b...`, `l[0][0]...`, nested `CASE`) as well as brackets and nested calls. Checked before pest runs (bracket, unary-minus and `CASE` nesting pre-scans), while folding each operator chain, and on every whole top-level expression. Exceeding it is `GraphError::SizeLimit { what: "expression nesting depth" }`. Evaluation has a backstop of 512 nested `eval_expr` calls per thread (summed across nested subqueries), reported as `"expression evaluation depth"`. Deep expressions need stack (about 15 KiB per level while parsing in a release build, more when unoptimized); `parse` and the execute entry points reserve it up front from the query's nesting depth and length via `stacker::maybe_grow`, so a 2 MiB thread stack suffices. A query is also limited to 1024 clauses, MATCH / MERGE pattern elements and SET items together (`MAX_STATEMENT_ITEMS`, `SizeLimit`), since the planner and executor recurse once per item.
- Variable-length / fixed-length hop count: 64 default, configurable via `Config::max_traversal_depth` (0 = unlimited). Enforced at plan validation, before execution.
- Variable-length traversal work: 10M edge visits default, configurable via `Config::max_traversal_work` (0 = unlimited)
- List size: 10M values per list (`MAX_LIST_LEN`), counting the elements of nested lists and the values of nested maps. Applies to every list a query builds: `range()`, list literals, list `+`, list and pattern comprehensions, and `collect()`. Exceeding it is `GraphError::SizeLimit { what: "list size" }` (`range()` keeps its `NumberOutOfRange` argument error).
- Result rows: 100K default (configurable via `Config::max_result_rows`, 0 = unlimited). Enforced incrementally on every execution path: the final result fails on row N+1, and operators that materialize rows (cartesian products and joins from comma patterns, separate `MATCH` clauses and `OPTIONAL MATCH`, and relationship expansions) fail as soon as they hold N+1 rows rather than after building everything; label scans on the materialized path and `ORDER BY` input are checked once loaded. `UNWIND` also stops at row N+1 when its rows are returned. Rows consumed by a streaming aggregate or by a write without `RETURN` (bulk `UNWIND ... CREATE`) are not counted against the row cap (the materialization budget still applies). Error: `result set exceeded maximum of N rows`.
- Materialized data: 1 GiB default (`Config::max_materialized_bytes`, 0 = unlimited). A per-query budget, in estimated bytes (96 per value plus keys and string contents), for everything a query keeps in memory: rows buffered by the operators that cannot stream (joins, `ORDER BY`, `DISTINCT`, the named fallback executor, a long `UNWIND` feeding them), the groups and collected values of aggregations, the final result, and the headroom a new list needs (`range()`, list `+`, comprehensions, `collect()`), so lists held by buffered rows cannot add up past it even though each stays under the per-list cap. Streaming operators are not counted: `count`/`sum`/`avg`/`min`/`max` over a long `UNWIND` or scan hold one accumulator per group, and `UNWIND range(a, b)` yields its values one at a time. Charges are cumulative while an operator runs and are given back when it returns its output, so the budget bounds live data, with real memory close to it (measured within about 1.2 times for wide rows and for lists), apart from one value being cloned while an expression reads it. Exceeding it is `GraphError::SizeLimit { what: "materialized query data in bytes" }`, raised as soon as the charge crosses the budget, not after the data is built.
- Subqueries (`EXISTS { ... }`, pattern predicates, pattern comprehensions) run under the enclosing query's limits: hop count, traversal work, row cap, and the per-query `shortestPath` work budget, which they share with the outer query.
- Newly-created DB files have mode `0o600` atomically (no umask race)

### Scale
Designed for datasets in the **low millions of nodes** with moderate edge density. 10M nodes is achievable for indexed point-lookups; full label scans at that scale will be slow. 100M+ nodes would require rearchitecting the scan layer (streaming from SQLite instead of materializing), replacing linear-scan dedup with `HashSet`, and making adjacency lists appendable without full rewrite.

### TCK gaps (2 skiplisted scenarios)
- Large durations requiring ±999999999 year dates (2 scenarios — chrono limitation, custom e2e tests cover the logic within chrono's range)

Regenerate blocker analysis: `uv run tests/tck/analyze_blockers.py`

## Non-Goals

- **Not a server.** No network protocol, no client-server architecture. Embedded library only.
- **Not a distributed database.** Single file, single machine. No replication.
- **Not an ORM.** Cypher queries in, records out. No object mapping layer.
- **Not for billion-node graphs.** Targets workloads that fit in SQLite (low millions of nodes). For larger scales, use a dedicated graph database.
