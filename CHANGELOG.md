# Changelog

All notable changes to graphdblite are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/) once a stable release
ships.

## [Unreleased]

## [0.2.0] - 2026-10-27

### Changed

- **New limit `Config::max_materialized_bytes`** (default 1 GiB, 0 =
  unlimited; also a `Database` field): an approximate per-query budget for
  data a query keeps materialized. Queries that previously built more than
  about 1 GiB of intermediate rows or lists (for example a 10M-element
  list that is then buffered by `ORDER BY` or `WITH`, or
  `range(1, 10000000)` combined with another large list) now fail with
  `GraphError::SizeLimit { what: "materialized query data in bytes" }`;
  raise the setting to allow them. Under the limit nothing changes.
- A returned `UNWIND` now fails at row N+1 of `max_result_rows` while it is
  building rows, instead of after building all of them (same error).
- **Statement-level atomicity inside explicit transactions.** A statement
  that fails inside `begin_write` / `write_tx` (all bindings) is rolled back
  to a savepoint taken before it: its partial writes are undone and the
  transaction stays open. Previously the failed statement's writes stayed
  in the transaction and were committed by a later commit.
- **Paths in every binding are `{nodes, relationships}`** (Python dict,
  Node object, C/Go JSON and `graphdb_result_value_str`), with full node
  and relationship values; they were a bare list of node ids.
- **Node: integers outside +-(2^53 - 1) are returned as `BigInt`** instead
  of a rounded `number`.
- **`:memory:` opens an in-memory database** in `Database::open` and every
  binding; it used to create a file named `:memory:` (Go, Node, C) or an
  on-disk database (Python).
- **Property names starting with `__` are reserved** and rejected on write
  (`SemanticError(InvalidArgumentValue)`): they overwrote the `__id` /
  `__src` / `__type` / ... metadata the engine binds next to properties
  (wrong ids, reversed paths, wrong `type(r)`).
- Binding relationship values gain `__type` (`__label` stays as a
  deprecated alias); metadata keys win over same-named user properties.
- **Python errors:** each Cypher error kind has its own class
  (`CypherSyntaxError`, `CypherTypeError`, `CypherSemanticError`, ...), all
  subclasses of `ParseError` (alias `QueryError`), so `except ParseError`
  still works; transaction-state errors (nested begin, write in a read
  transaction, snapshot during a transaction) raise `TransactionError`
  instead of `RuntimeError`.
- **C:** a transaction belongs to the thread that began it; other threads'
  transaction calls fail (use the new `_shared` begin variants to opt out).
- C/Go JSON: floats keep a fractional part (`1.0`, was `1`) and NaN /
  infinities are `null` (was invalid JSON); `graphdb_result_value_str`
  prints floats as `1.0`.
- Go: `Commit` keeps the transaction open when it fails, so `Rollback`
  still works.

- **`Edge` gains a `seq: u64` field** (Rust API; part of equality and
  hashing). It is the parallel-edge sequence number, so relationship values
  now have identity: two parallel edges are distinct values even with equal
  properties. Code that builds `Edge` literals must set it.
  Python, Node and C (JSON) relationship values gain an `__edge_seq` key.

- **Behaviour change: a case-insensitive fulltext index no longer changes
  query results.** Plain `CONTAINS` / `STARTS WITH` / `ENDS WITH` are
  case-sensitive whether or not a `create_fulltext_index_ci` index exists,
  matching Neo4j. Previously they became case-insensitive only when the
  planner used the index (not for short terms, unindexed databases or query
  shapes it did not rewrite). For case-insensitive matching write
  `toLower(n.prop) CONTAINS toLower($q)`; the CI index accelerates that form.

- **On-disk schema version 2.** Index and fulltext tables use a new naming
  scheme (`$` separators, uppercase letters escaped as `^x`). Opening a
  0.1.x database migrates it once; 0.1.x builds cannot open a migrated
  file. See `docs/sqlite-application-id.md`.
- **`Database::open` no longer adopts foreign SQLite files.** A file with
  `application_id` 0 that contains anything other than graphdblite tables
  is rejected (`SQLITE_NOTADB`) and left untouched; previously it was
  stamped, given graphdblite tables and switched to WAL.
- **Typed Rust API.** `WriteTransaction::create_edge` returns the new
  edge's sequence number (`Result<u64>`, was `Result<()>`); new
  `delete_edge_at(src, dst, label, seq)` deletes one parallel edge.
  `create_edge` drops `Null` properties, and map / node / relationship /
  path property values are rejected as in Cypher.
- Error messages no longer echo user data: `fts.search` errors drop the
  query text, type errors name the value's type instead of printing it,
  and over-long names are reported by length only.

- **Behaviour change: resource limits now hold everywhere** (see the
  Security section). Queries that previously slipped past a limit now fail:
  results over `Config::max_result_rows` on the default execution path,
  joins of separate `MATCH` / `OPTIONAL MATCH` clauses and `ORDER BY` inputs
  over the row cap, subqueries with hop counts over
  `Config::max_traversal_depth`, and expressions whose operator chains are
  deeper than 256. Raise the `Config` limit (or set it to 0) if you relied on
  this. The cartesian-product row-cap error now reads `result set exceeded
  maximum of N rows` like every other row-cap error.

- **Behaviour change: MERGE follows openCypher.** A relationship MERGE
  matches the whole pattern with bound variables fixed and, if it does not
  match, creates every unbound part of it. Previously each endpoint was
  found-or-created on its own, so `MERGE (a:A {id: 1})-[:R]->(b:B {id: 2})`
  reused an existing `:A`; it now creates a new one unless the full pattern
  exists (bind `a` with `MATCH` first to reuse it). A standalone node MERGE
  returns every matching node, not only the first.
- **Behaviour change: CREATE / MERGE relationship endpoints must be live
  nodes.** An endpoint bound to an integer (`WITH 1 AS a CREATE
  (a)-[:R]->(b)`) is a `TypeError` (was: a relationship to whatever node had
  that id, or a dangling one); a null endpoint (from `OPTIONAL MATCH`) is an
  error (MERGE silently dropped the row, CREATE created a new node); a
  deleted node is `NodeNotFound`. `SET n = 'x'` / `SET n += 1` are
  `TypeError`s everywhere (MERGE wiped or copied properties instead).
- `CREATE (n) RETURN n` and `MERGE (n) RETURN n` return a node, not its
  integer id.

- **Behaviour change: integer division by zero is an error.** `1 / 0` and
  `1 % 0` raise an `ArithmeticError` with the new `ErrorCode::DivisionByZero`
  (they returned `null`). Once a Float is involved the result follows IEEE
  754: `1 / 0.0` and `1.0 / 0` are `Infinity`, `0 / 0.0` and `1.5 % 0` are
  `NaN` (`1.0 / 0` used to be `null`). Dividing a duration by zero errors.
- **Behaviour change: `1` and `1.0` are one value for `DISTINCT`, grouping
  keys, `count(DISTINCT ...)`, `collect(DISTINCT ...)` and `UNION`**,
  matching `=`; this applies inside lists and maps too (`[1]` / `[1.0]`).
  The first value seen is kept.
- **Behaviour change: stricter temporal maps.** Unknown keys
  (`date({yaer: 2024})`), a missing year (`date({})`,
  `localdatetime({hour: 1})`) and components that do not fit
  (`localtime({hour: 4294967297})`) raise `InvalidArgumentValue` instead
  of silently building year 0 or a wrapped value. `date()`, `datetime()`,
  ... without an argument return the current instant (UTC), fixed once
  per statement; they returned year 0.
- `toString` of a Float uses Neo4j's format (`1.0E20`, `1.5E-7`,
  `Infinity`, `NaN`) outside `[1e-3, 1e7)`.
- `GraphError::SizeLimit` messages read `<what> (<actual>) exceeds maximum
  of <limit>`; byte limits say so in `what` ("Cypher query size in bytes").
  Depth, list and work limits were wrongly reported in bytes.

### Added

- Query parameters for Node (`query(cypher, params?)` /
  `execute(cypher, params?)` on the database and both transaction
  classes), C (`graphdb_query_params`, `graphdb_execute_params`,
  `graphdb_tx_execute_params`, taking a JSON object) and Go
  (`QueryParams` / `ExecuteParams`).
- `Database::in_transaction()`.
- C: `graphdb_tx_begin_write_shared` / `graphdb_tx_begin_read_shared`.
- Go: `Type*` constants for every value type code (7-15), `ErrClosed` /
  `ErrTxFinished`, `ReadTransaction.Rollback`.
- Node and Python: `ReadTransaction.rollback()`.
- Python: `Database()` and `snapshot_to()` accept `os.PathLike`.
- Test-only `test-hooks` Cargo feature (core, Python, Node) used by the
  bindings' panic-containment tests.

### Fixed

- **A pattern predicate or `EXISTS` in `WITH ... WHERE` on a renamed
  variable was always true** (`MATCH (a:A) WITH a AS z WHERE (z)-[:R]->()`),
  and a scalar alias was invisible inside the `EXISTS` of the same `WITH`
  (`WITH m.k AS mk WHERE EXISTS { MATCH (n:L) WHERE n.k = mk }` raised
  `UndefinedVariable` or silently matched nothing). Such conjuncts now run
  on the projected row.
- Unaliased list comprehensions, pattern comprehensions, quantifiers,
  `CASE`, pattern predicates and `EXISTS` no longer all collide on the
  column name `_expr`; the column is named after the expression text.
- Temporal parse errors are `InvalidArgumentValue` query errors that
  describe the problem; they no longer read `serialization error ()` and no
  longer echo the input. `UNWIND` of a non-list and unknown timezones also
  report the type or problem instead of the value.
- `EXISTS { UNWIND ... MATCH ... }` (a leading `UNWIND`) is accepted.

- **Python:** a transaction object dropped without commit/rollback left
  its `Database` permanently "closed"; it now rolls back and releases the
  database. `db.close()` with an open transaction was ignored (a later
  `tx.commit()` persisted and revived the database); it now rolls back
  and the transaction raises `database is closed`.
- **Node:** `ReadTransaction` had no `rollback()`, so `withReadTx` (on a
  throwing callback) and `Symbol.dispose` leaked the read transaction and
  blocked the next `beginWrite`. A stale transaction object could also
  roll back a newer transaction when garbage-collected; transaction
  objects are now tied to the transaction they began.
- `Database::commit` / `rollback` left the handle believing a transaction
  was open when COMMIT/ROLLBACK itself failed, so every later `begin_*`
  failed; the state is now resynced from SQLite.
- Python: `__version__` is read from the package metadata (was hardcoded
  `0.1.1`); transaction `query` / `execute` release the GIL.
- C header: `graphdb_result_value_type` documents every code (0-15).
- `SET n:Label` / `MERGE ... SET n:Label` failed with "constraint failed"
  on a node with an FTS-indexed string property.
- `drop_fulltext_index("A", "b_data")` dropped the FTS5 shadow table of
  index `(A, b)` and corrupted it; only real FTS indexes are matched now,
  and an index on property `b_data` can coexist with one on `b`.
- Indexes (btree, composite and fulltext) on labels that differ only in
  case (`:Person` / `:person`) failed with a raw "table already exists"
  error.
- Legacy `_`-named index tables were mis-parsed for labels containing `_`.
- `CREATE (n:A:A)` stored the label twice and drifted the label counter.
- **Planner and parser correctness** (code review).
  - A composite index is used only when every column has an equality
    predicate. A leading-prefix lookup (`WHERE n.x = 1` with an index on
    `(x, z)`) silently dropped nodes that lack the trailing property.
  - Anonymous relationships in two named paths (`MATCH p = ()-[:R]->()
    MATCH q = ()-[:R]->()`) shared one internal alias, so separate MATCH
    clauses were wrongly joined and comma-separated patterns skipped
    relationship uniqueness.
  - `UNWIND ... MATCH ... RETURN` (and `OPTIONAL MATCH` directly after
    `UNWIND`) was a syntax error in read-only queries.
  - `/* ... */` block comments are supported, and a quote or bracket
    inside a backtick identifier or comment no longer trips the parser's
    pre-scans (``RETURN 1 AS `it's` `` failed as an unclosed string).
  - Fulltext rewrite: a term referencing the scanned node
    (`n.t CONTAINS n.u`) returned no rows with an FTS index, and a
    non-string term (`CONTAINS 123`, or a non-string `$param`) raised a
    TypeError instead of matching nothing. Both now give the same result
    as without the index.
  - A non-literal SKIP/LIMIT such as `LIMIT toInteger(rand() * 10) + 1`
    was frozen by the plan cache and returned the same row count on every
    run; any non-literal SKIP/LIMIT now bypasses the cache.
  - `WITH ... WHERE` now filters after the WITH's projection, `ORDER BY`,
    `SKIP` and `LIMIT`, as openCypher specifies. Previously a WHERE dropped
    the ORDER BY entirely and ran before SKIP/LIMIT
    (`WITH n ORDER BY n.v DESC LIMIT 1 WHERE n.v < 3` returned a row), and
    it re-evaluated aliased expressions instead of testing the projected
    value (`WITH rand() AS r WHERE r < 0.1` returned values above 0.1).
  - `ORDER BY` on a group-key alias of an aggregating WITH
    (`WITH x % 2 AS k, count(*) AS c ORDER BY k`) did not sort.
  - Scope errors are now reported instead of silently producing nulls:
    `WITH ... ORDER BY <undefined> WHERE ...`, a WITH WHERE that reads an
    undefined variable or one dropped by `DISTINCT` / aggregation, and
    `RETURN ... ORDER BY` on a variable an earlier WITH dropped
    (`WITH n.v AS v RETURN v ORDER BY n.w`) raise `UndefinedVariable`.

- **Relationship identity and the read engines** (code review 2026-10):
  - An undirected match (`(a)-[r]-(b)`) dropped the reverse edge of a pair
    connected both ways; every edge in either direction is now a row.
  - `OPTIONAL MATCH`, a second `MATCH`, pattern comprehensions and comma
    patterns returned one row for several parallel edges.
  - Relationship uniqueness within a pattern is enforced across fixed and
    variable-length segments in every engine, keyed on the directed edge
    (`(a)-[:R]->(b)-[:R]->(a)` over a mutual pair is a valid match); a
    `LIMIT` pushed into a variable-length expansion no longer drops rows
    that a later uniqueness or bound-endpoint check would keep.
  - `count(DISTINCT r)` and `r1 = r2` treated parallel edges as equal, and
    `DELETE x` on a relationship held as a value (`UNWIND relationships(p)`,
    `collect(r)`) deleted every parallel edge; it now deletes that edge only.
    `SET x.p = v` on a node or relationship held as a value (`WITH
    head(collect(r)) AS x`) is no longer silently dropped.
  - Index and fulltext lookups in a later `MATCH` ignored residual
    conditions that referenced earlier variables, ignored a variable already
    bound by an earlier clause, and compared `{k: 1.0}` with stored `1`
    strictly. `id(n) = 2.0` now matches node 2.
  - Pattern predicates were false after an aggregating `WITH`, ignored
    variable-length relationship property maps, and `exists((a)-->())` was
    always true.
  - `EXISTS { ... }` could not reference outer variables in its `WHERE`, and
    lost its correlation through `RETURN ... LIMIT` / `SKIP` / `DISTINCT` /
    `ORDER BY` / `UNWIND`.
  - `OPTIONAL MATCH p=(b)<-[:R*2]-(x)` rebuilt path nodes in the wrong
    order.
  - Variable-length relationship property maps compare with Cypher
    equality (`{w: 1.0}` matches a stored `1`).

- **`shortestPath` / `allShortestPaths` correctness** (GitHub issue #2).
  - An untyped relationship (`-[*]->`) found no paths; it now follows
    every relationship type, and path edges carry their real types.
  - Multi-type patterns (`-[:A|B*]->`) searched only the first type.
  - Pairs with no path returned a row with a `null` path; a plain `MATCH`
    now drops them (`OPTIONAL MATCH` still yields `null`).
  - A pair whose endpoints are the same node returned a zero-length path
    even with a minimum length of 1; it now matches only with `*0..`.
    Minimum lengths above 1 are rejected, as in Neo4j.
  - A `shortestPath` in its own `MATCH` ignored endpoints bound by an
    earlier clause and searched every node pair.
  - Endpoint labels / inline properties on a node that only appears
    inside `shortestPath(...)` were ignored in comma-joined patterns.
  - A relationship without `*` is now a single hop instead of unbounded.
  - Several `shortestPath` patterns with unnamed endpoints in one `MATCH`
    shared internal variable names and constrained each other.
  - `allShortestPaths` collapsed parallel relationships of the same type
    into one path; each is now a separate path.
  - Shortest-path searches had no work cap. They now share the
    `Config::max_traversal_work` budget per query and fail with a
    size-limit error instead of running unbounded.

Reported with fixes by @arupkamal (github.com/arupkamal/graphdblite);
ported here with regression tests in `tests/bug_regressions.rs`.

- **Label scans are exact.** `MATCH (n:person)` no longer returns `:Person`
  nodes, and `:A_B` no longer matches `:AxB` (the scan used SQL `LIKE`).
  This also affected index and fulltext backfills, MERGE and
  `find_nodes_by_label`.
- **Index lookups keep every label.** `MATCH (n:A:B {x: 1})` served by an
  index on `:A(x)` now requires `:B` too.
- **Index lookups use Cypher equality.** `{x: 1}` matches a stored `1.0` (and
  `0.0` matches `-0.0`), as an unindexed scan does; `{x: null}` never matches.
- **MERGE** honours relationship direction (`<-[:R]-`, undirected), keeps every
  label on the nodes it matches or creates, and applies `ON CREATE SET` to the
  relationship it just created rather than an existing parallel one.
- **MERGE `ON CREATE` / `ON MATCH SET`** can read the merged node
  (`ON MATCH SET n.visits = n.visits + 1` failed with "property access on
  Integer").
- **Variable-length relationship property maps** (`[:R*1..3 {w: $w}]`) no
  longer ignore parameters and variables.
- **DELETE / SET / REMOVE on a plain value** (`WITH n.k AS x DELETE x`) is a
  type error; it used to act on the node whose id equalled the value.
- **Fulltext lookups re-check every candidate.** The `toLower(...)` idiom on a
  case-insensitive index now works for `STARTS WITH`, `ENDS WITH` and short
  terms.
- **Plan cache** is keyed on SQLite's schema cookie too, so a plan cached
  before a rolled-back `CREATE INDEX` or another handle's `DROP INDEX` is not
  reused. A failed auto-commit now rolls back instead of leaving the
  transaction open.
- **Setting a property to null** through MERGE or
  `WriteTransaction::set_node_property` no longer leaves a null index entry.
- **Long multi-byte names** return a size-limit error instead of panicking.
- **Cypher writes validate properties.** Map values (and lists of maps) are
  rejected in `CREATE`, `MERGE` and `SET`, and `Config::max_property_value_bytes`
  / `max_name_bytes` now apply to Cypher writes, not only the typed API.
- **Python:** `Database("my.db")` with a bare file name no longer fails with
  "invalid path".

- **Writes and MERGE** (code review).
  - `MATCH (a) MERGE (a)-[:R]->(c:C {...})` was a silent no-op when an
    endpoint was a new variable; an anonymous endpoint errored.
  - Relationship MERGE ON CREATE / ON MATCH could not read the relationship
    (`ON MATCH SET r.n = r.n + 1` erased `n`).
  - ON CREATE / ON MATCH items were applied to the merged entity whatever
    variable they named (`SET a.flag` landed on `r`; `SET a:Lab` was dropped).
  - `MERGE (a:A {id: 1})-[:R]->(a)` linked to an unrelated node.
  - SET across rows read stale values (`UNWIND [1,2,3] AS i SET n.c =
    n.c + 1` added 1, not 3), for nodes and relationships.
  - `RETURN properties(n)` after `CREATE` / `MERGE` (also after `WITH n`)
    failed with a type error.
  - `SET` on a relationship deleted earlier in the query recreated a phantom
    property row; it is now an `EdgeNotFound` error.
  - `SET b = a` / `SET b += a` copying from a node or relationship failed.
  - `REMOVE n:A RETURN n` returned the node with its old labels.
  - `SET x = map`, `SET x += map`, `SET x:L`, `REMOVE x.p` and `REMOVE x:L`
    on a node or relationship held as a value (`WITH head(collect(n)) AS x`)
    were silently ignored.
  - `DELETE` ignored storage errors.

- **Expression evaluation** (code review).
  - `size()` / `length()` of a string counted UTF-8 bytes
    (`size('日本')` was 6); they count characters.
  - `round(x, precision)` ignored the precision. It now rounds HALF_UP on
    the decimal value (`round(2.675, 2)` = 2.68), and `round(x, precision,
    mode)` accepts `UP`, `DOWN`, `CEILING`, `FLOOR`, `HALF_UP`,
    `HALF_DOWN`, `HALF_EVEN`.
  - A list comprehension or quantifier variable named like an outer node
    or relationship read the outer entity's properties
    (`MATCH (x) RETURN [x IN [{name: 'i'}] | x.name]`).
  - `[x IN null | x]` is `null` (was `[]`); `split('abc', '')` is
    `['a', 'b', 'c']` (had empty strings at both ends);
    `substring(s, 1, null)` is `null`; `toInteger(NaN)` is `null` (was 0);
    `toFloat` no longer accepts Rust-only spellings like `'inf'`.
  - `9007199254740993 = 9007199254740992.0` was true: Integer/Float
    comparison is now exact beyond 2^53.
  - `-duration(...)` raised a type error.
  - `'xb' =~ 'a)|(b'` matched: a pattern is validated on its own before it
    is anchored. `=~` also recompiled its pattern for every row when the
    query ran without an explicit transaction.
  - `sum()` of Integers wrapped on overflow; it raises `NumberOutOfRange`
    like `+`. `percentileDisc` returns an element of its input (an Integer
    stays an Integer). Errors in grouping keys and `ORDER BY` keys were
    turned into `null`; they now fail the query.
  - An aggregate inside a subscript, slice, property access or `CASE`
    (`RETURN collect(x)[0]`, `WITH collect(r)[0] AS r`) was not treated as
    an aggregation and returned one row per input row.
  - `UNION` / `UNION ALL` inside `EXISTS { ... }` was a syntax error.
  - Parsing was quadratic in the query length (a 40,000-element list
    literal took ~21 s); it is linear.
- **Temporal arithmetic and construction** (code review).
  - Out-of-range results panicked (debug) or wrapped (release):
    `date(...) + duration({days: 100000000})`, Duration `+` / `-` / `*` /
    negation overflow, huge components in `duration('PT9223372036854775807H')`,
    `dayOfQuarter`, `millisecond` in time maps, `datetime.fromepoch` past
    year 2262. They raise `NumberOutOfRange`; a month shift past the
    calendar range (`+ duration('P300000Y')`) no longer returns the date
    unchanged, and `fromepoch` no longer falls back to 1970.
  - `duration.inSeconds` / `duration.inDays` overflowed for spans over
    ~292 years.
  - `duration({seconds: 20000000, nanoseconds: 1})` lost the nanosecond and
    `duration({hours: 3000000})` saturated: integer map components are
    exact.
  - Adding a duration to a datetime with a named zone kept the old UTC
    offset across a DST change
    (`datetime('2024-03-10T00:30[America/New_York]') + duration({hours: 3})`
    is `04:30-04:00`). Months and days shift the local date-time, seconds
    shift the instant, and the offset is re-resolved.
  - `datetime({epochSeconds: ...})` / `datetime({epochMillis: ...})` gave
    year 0; both are supported (with `nanosecond` and `timezone`).

### Security

- **Stack overflow on small thread stacks.** Nested expressions within the
  256-level limit (`abs(abs(...))`, `[[...]]`, nested `CASE`, `[x IN ...]`)
  overflowed a 2 MiB thread stack in release builds and aborted the
  process. Parsing, planning and execution now reserve stack from the
  query's nesting depth and length (`stacker`), growing onto a heap segment
  when the thread's stack is small.
- A query with more than 1024 clauses, MATCH / MERGE pattern elements and
  `SET` items now fails with `SizeLimit` instead of overflowing the stack in the
  planner and executor (`WITH 1 AS x ` x 5000 aborted the process).
- **Node and Python:** parameters nested deeper than 128 levels (or
  self-referencing in Python) crashed the process in the recursive value
  conversion; they now raise a typed error.

- **Queries could use memory far beyond the configured limits.** Rows were
  built in full before the row cap fired, so
  `MATCH (n:N) UNWIND range(1, 50000) AS x RETURN x` over 40 nodes reached
  3.9 GB before returning the cap error, `RETURN count(*)` over the same
  rows 2.9 GB, `UNWIND range(1, 5000000) AS x RETURN x` 5.5 GB, and 17
  chained `MATCH` clauses aborted the process. `UNWIND` now yields rows
  lazily (`UNWIND range(...)` does not build the list) and stops at row
  N+1, `count`/`sum`/`avg`/`min`/`max` aggregate incrementally in
  O(groups) memory, `DISTINCT` streams with a hash set (it was quadratic),
  and a new per-query budget, `Config::max_materialized_bytes`, bounds what
  the remaining materializing operators, aggregation state, `collect()`,
  and the lists built by `range()`, `+` and comprehensions may hold. The
  10M-element list cap was per list, so
  `UNWIND range(1,8) AS i WITH i, range(1,5000000) AS l ...` reached
  4.2 GB; lists now also count against the query budget.
- **A foreign SQLite file could be switched to WAL and half-initialized.**
  A file with `application_id = 0` whose tables only share graphdblite's
  names (`CREATE TABLE nodes(id, title)`) passed the name-only identity
  check, was put into WAL mode, and then failed schema setup. The core
  tables' columns, types and primary key are now checked before any write;
  a lookalike is refused with the same `SQLITE_NOTADB` error as other
  foreign files, leaving the file byte-for-byte unchanged.

- **C: `graphdb_result_value_str` use-after-free.** The header said the
  string lived until the next call on the same result, but it was one
  thread-local buffer shared by all results, so holding two strings read
  freed memory. Strings are now stored per result and live until
  `graphdb_result_free`.
- **Go: finalizer during a C call.** Methods did not keep the `Database`
  alive across the cgo call, so the GC finalizer could `graphdb_close` it
  mid-call (`Open(p).Query(q)`). Every method now uses
  `runtime.KeepAlive`.
- **Go: error message from the wrong thread.** The C error string is
  thread-local but was read in a separate cgo call, after which the
  goroutine could have moved threads (another call's error, or freed
  memory). Calls and error reads now run on one locked OS thread.
- **C: cross-thread transaction capture.** A transaction begun on one
  thread silently absorbed another thread's `graphdb_tx_execute` calls
  (and lost them on rollback); such calls are now rejected.
- `fts.search(label, property, query)` on a multi-property index could be
  escaped with query text such as `x) OR title:(secret`, matching other
  columns. The column scope is now bound outside the user query.
- Untrusted database files: connections run with `trusted_schema` off and
  `SQLITE_DBCONFIG_DEFENSIVE` on, and files with triggers on (or views
  named like) graphdblite tables are rejected, so SQL stored in a file no
  longer runs during library writes.
- FTS table names and column lists read from the file are validated and
  quoted before use; a crafted table name could previously inject SQL into
  index maintenance.
- Corrupt or crafted adjacency blobs (over-long varints, overflowing
  deltas) now return a `Serialization` error instead of panicking (debug)
  or producing wrapped node ids (release).
- `snapshot_to` creates the target atomically with mode `0o600` (it was
  created under the umask and chmod-ed afterwards, ignoring errors) and
  refuses an existing target without a check-then-create race.

- **No more panics on non-ASCII temporal strings.** `time('1é1')`,
  `date('202é101')` and similar sliced inside a multi-byte character and
  panicked (a process abort in the Node binding). Temporal parsers now
  reject non-ASCII input with a parse error.
- **Panic containment in every binding.** C: every export catches panics,
  takes the lock outside the catch (no more permanently poisoned handle) and
  rolls back the interrupted transaction. Node: all methods use
  `catch_unwind`, so a panic is a JS exception instead of a process abort.
  Python: a panic raises `GraphDBError` instead of `PanicException`, which
  escaped `except Exception`.
- **No stack overflow on flat operator chains.** `RETURN 1+1+...+1` (a few
  hundred terms), `NOT NOT ... true`, `- - - 1`, `m.a.a...` or nested `CASE`
  aborted the process. The 256 expression-depth limit now counts real tree
  depth; inline pattern property maps build balanced predicates; evaluation
  has a recursion backstop.
- **Subqueries no longer escape limits.** `EXISTS { ... }`, pattern
  predicates and pattern comprehensions ran with no hop cap, no row cap and
  a fixed traversal-work budget; they now use the query's `Config` limits.
- **Row cap enforced on the default execution path and incrementally.**
  `UNWIND range(1, 150000) AS x RETURN x` returned all rows under the 100K
  default, and joins of separate `MATCH` clauses built millions of rows
  (~13 GB for a 4-way join of 40 nodes) without ever checking the cap.
- **One list-size cap for every list.** Only `range()` was capped, so
  `range(1,10000000) + range(1,10000000) + ...` could exhaust memory and
  abort. All list construction now shares the 10M-value cap.

## [0.1.2] - 2026-07-08

### Added

- **Composite (multi-column) secondary indexes.** New
  `Database::create_composite_index(label, &[props])` /
  `drop_composite_index` plus matching `WriteTransaction` methods, with
  parity in the Python, Node, Go, and C/FFI bindings. Cypher DDL
  extended: `CREATE INDEX ON :Label(prop1, prop2, …)` and
  `DROP INDEX ON :Label(prop1, prop2, …)`. The planner picks the
  index with the longest leftmost-prefix matched by equality predicates
  — both inline pattern properties (`MATCH (p:Person {tenant_id: 1,
  ext_id: 'a'})`) and WHERE-clause conjuncts (`MATCH (p:Person) WHERE
  p.tenant_id = 1 AND p.ext_id = 'a'`) drive the same lookup. Tie-breaks
  prefer the smaller total index width, then lex on joined property
  names. Storage uses `node_idx$<label>$<p1>$…$<pN>` tables with
  msgpack-concat tuple keys (`$` is forbidden by `validate_name`, so
  the separator is unambiguous); single-prop indexes keep their
  existing `node_idx_<label>_<prop>` naming for backward compatibility
  — both APIs produce the same on-disk table for `N=1`. `db.indexes()`
  emits one row per covered property (`kind = "btree"`), matching the
  multi-prop FTS convention. Nodes missing any covered property are
  skipped from index entries (no `NULL` placeholder).

### Fixed

- **Sort now orders all value types correctly.** The pull-based `ORDER BY`
  paths (named- and slot-iterator) used a stripped comparator that returned
  "equal" for booleans, temporals (`Date`/`Time`/`DateTime`), `Duration`, and
  lists, silently leaving those columns in insertion order. Both paths now use
  the canonical comparator (NaN sorts last, lists compared element-wise).
- **`MERGE … ON CREATE/ON MATCH SET`** with a map (`SET n += {…}` / `SET n =
  {…}`) or label (`SET n:Label`) now maintains secondary and FTS indexes. These
  arms previously wrote node storage directly, so an index-served query could
  miss a MERGE-updated node or return a stale one.
- **`SET n:Label` / `REMOVE n:Label`** now re-index the node. Index/FTS
  maintenance is keyed per-label rather than only on the sorted-first "primary"
  label, so adding or removing a label no longer strands index entries on a
  multi-label node. (All `update_indexes_for_node` / `remove_indexes_for_node`
  callers now pass the full label set.)
- **Secondary-index and FTS OR-chain dedup no longer duplicates FTS rows.** A
  node matching two disjuncts of an OR-chain rewritten to `Union(FullTextLookup,
  …)` receives a different per-branch BM25 `__fts_score`; UNION dedup now ignores
  that synthetic key so the node collapses to one row.
- **Index and FTS table names are collision-free across underscores.**
  Single-property secondary-index tables now use the unambiguous
  `node_idx$<label>$<prop>` scheme and single-property fulltext tables use
  `node_fts$<label>$<prop>` (legacy `_`-delimited tables still resolve for
  existing databases), so `(A_b, c)` and `(A, b_c)` no longer collide onto one
  physical table. The index-lookup executor also re-verifies label membership
  defensively.
- **Contradictory equality predicates on an indexed property** (`n.x = 1 AND n.x
  = 2`) return zero rows again. The index-pushdown pass previously folded only
  the last value into the lookup and dropped the other conjunct, returning wrong
  rows; conflicting same-property equalities now stay in the residual filter.
- **`substring` on multibyte strings no longer panics.** It is now
  character-indexed per the Cypher spec (was byte-indexed, panicking at a
  non-char boundary, e.g. `substring('é', 1)`).
- **C FFI entry points no longer abort the host on a query panic.** Query and
  execute paths run untrusted Cypher under `catch_unwind`; a panic in the core
  now returns `-1` + `graphdb_last_error` instead of unwinding across the
  `extern "C"` boundary (undefined behavior / process abort).
- **`DELETE` of a duplicate-bound parallel edge** no longer under-counts the
  edge-type stats counter. `delete_single_edge` only decrements when a row was
  actually removed, so `db.counts()` and planner cardinality stay accurate.
- **Cypher `DELETE` now cleans up secondary indexes.** `exec_delete`
  previously called `storage::node::delete_node` directly, bypassing
  the index- and FTS-maintenance path that
  `WriteTransaction::delete_node` performs. The bug was latent for
  single-property btree indexes (the executor silently fetched deleted
  nodes back through stale entries) and surfaced as `NodeNotFound`
  errors against composite indexes once the planner reliably picked
  them for follow-up lookups. `exec_delete` now invokes
  `index::remove_indexes_for_node` and `fts::remove_fts_for_node`
  before removing each node, matching the typed-transaction path.
- **OR-chain rewrite across FTS indexes.** `WHERE A OR B OR …` where
  every disjunct is a text predicate (`CONTAINS` / `STARTS WITH` /
  `ENDS WITH`) against an FTS-indexed property of the same label now
  plans as `Union(FullTextLookup, FullTextLookup, …)` with row dedup.
  Mixed AND/OR and a single non-FTS-eligible disjunct still fall back
  to a label scan.
- **Full-text indexes.** New `Database::create_fulltext_index(label,
  property)` and `Database::drop_fulltext_index(...)` plus matching
  `WriteTransaction` methods. Python binding gains parity
  (`WriteTransaction.create_fulltext_index` /
  `.drop_fulltext_index`). Backed by SQLite FTS5 with the trigram
  tokenizer (`case_sensitive 1`). The planner transparently rewrites
  `CONTAINS`, `STARTS WITH`, and `ENDS WITH` predicates on indexed
  `(label, property)` pairs to a `FullTextLookup` operator, with a
  position post-filter for the two anchored ops. Term-length floor
  (`<3` codepoints) falls back to a scan. See `docs/cypher.md` for
  storage-cost trade-offs. Node, Go, and C bindings do not yet
  expose this surface; they will gain it alongside a future
  index-DDL parity project.
- **Regex match operator (`=~`).** OpenCypher full-match semantics
  backed by the Rust `regex` crate. Per-query compiled-regex cache,
  NULL propagation, ReDoS-safe by construction. See
  `docs/cypher.md` for the full feature/limitation matrix.
- **Index DDL parity across Node / Go / C/FFI bindings.** Each
  binding's `WriteTransaction` wrapper now exposes
  `create_index` / `drop_index` / `create_fulltext_index` /
  `drop_fulltext_index` (case-converted per language idiom).
  Closes BC-11 in `docs/BINDING_CONFORMANCE.md`. Python's surface
  is unchanged.
- **Index introspection via `CALL db.indexes()`.** New built-in
  procedure returns `(label, property, kind)` rows for every secondary
  and fulltext index in the database. No new grammar — reuses the
  existing CALL surface.

### Internal

- **pyo3 0.22 → 0.28 upgrade.** Python binding migrated to the newer
  `IntoPyObject` API. No public Python API change. The
  `#![allow(clippy::useless_conversion)]` module-level workaround is
  gone. Follow-on API churn folded in: `PyDict::new_bound` →
  `PyDict::new`, `Python::allow_threads` → `Python::detach`,
  `Bound::downcast` → `cast`, and an `unsendable` pyclass attribute on
  the three wrapper classes (rusqlite's `Connection` is `!Send`).

## [0.1.1] - 2026-05-23

### Performance

- **`WHERE id(n) = expr` now plans as an O(1) `IdLookup`** instead of a
  full node-table scan + filter. Critical fix for downstream batch
  helpers that use `UNWIND $rows AS row MATCH (a) WHERE id(a) = row.s`
  patterns: a 200-node graph with a 500-edge UNWIND batch dropped from
  ~4 s to <0.2 s, and large-scale reproductions (1176 edges) that
  previously cost ~67 s / 16 GB RSS are now milliseconds at sane RSS.
  Applies to both the non-correlated form and the correlated form
  inside `CorrelatedJoin` (predicate-pushdown into the join's right
  side). Empty-label `MATCH (n)` only; labeled `MATCH (n:Foo) WHERE
  id(n) = ...` still uses the scan path.

### Added

- `Database::snapshot_to(path)` — write a consistent, single-file snapshot of
  the database via `VACUUM INTO`. Produces a self-contained SQLite file (no
  `-wal` / `-shm` sidecars), defragmented and compacted. Rejects when a
  transaction is active or the destination path already exists. Exposed
  in every binding (Python `snapshot_to`, Node `snapshotTo`, C
  `graphdb_snapshot_to`, Go `SnapshotTo`).

### Python bindings

- PEP 561 type information: ship a `py.typed` marker plus a complete
  `__init__.pyi` covering `Database`, `WriteTransaction`,
  `ReadTransaction`, the exception hierarchy, batch APIs, and the
  optional `params` argument on `query` / `execute`. Downstream callers
  can drop their `# type: ignore[import-untyped]` workarounds. Also
  re-exports the transaction classes and exceptions from the top-level
  `graphdblite` package.

### Documentation

- Document the `MERGE (a)-[:R]->(b)` edge-upsert pattern with
  `ON CREATE SET r += $props` / `ON MATCH SET r += $props` in the
  Cypher reference (`docs/cypher.md`) and the Python / Node guides.
  Regression tests in `tests/e2e_query_tests.rs` cover the upsert
  flow and inline-prop discrimination of parallel edges. No behavior
  change — just confirms and documents what already worked.

## [0.1.0] - 2026-05-16

First public release.

### Core

- Embedded graph database backed by SQLite (WAL mode; multi-process safe).
- ACID semantics, crash-safe — provided by the SQLite engine.
- Single-file storage; zero configuration.

### Query language

- Cypher: parse → plan → execute pipeline with parse cache and plan cache.
- **openCypher TCK conformance: 100%** (3895/3895 scenarios).
- 50+ functions across string, list, map, math, temporal, predicate, and aggregation categories.
- Variable-length paths, shortest path, all-shortest-paths.
- Pattern comprehensions, list comprehensions, quantifier predicates.
- CALL procedures with YIELD (registry-based; internal use today).
- EXPLAIN for plan inspection.

### Types

- Full Cypher temporal types: Date, Time, LocalTime, DateTime, LocalDateTime, Duration.
- Named-timezone DateTime survives storage round-trips.
- Node and edge property values: i64, f64, bool, string, list, map, path, plus all temporal types.

### Transactions

- Two coexisting APIs on `Database`:
  - Stateful: `begin_write` / `begin_read` / `execute` / `commit` / `rollback`.
  - RAII guards: `write_tx()` / `read_tx()` returning `WriteTxGuard` / `ReadTxGuard`.
- Auto-tx on bare `Database::execute` calls (chooses read or write mode by query class).
- Read-only enforcement: write Cypher inside a read transaction is rejected.

### Storage

- Adjacency-list edge storage; traversals are direct key lookups.
- Parallel edges per `(src, dst, type)` triple, tracked by `__edge_seq`.
- Per-label secondary indexes; index-aware planner picks `IndexLookup` over `Scan` when applicable.

### Language bindings

- Rust (this crate).
- Python — `pip install graphdblite`, full PyPI wheel matrix (glibc + musl Linux, macOS, Windows).
- Node.js — `npm install graphdblite`, native addons for all 7 supported platforms.
- Go — `go get github.com/ds7n/graphdblite/bindings/go`.
- C / FFI — `libgraphdblite_ffi.{so,a,dylib,dll}` + `graphdblite.h` on the GitHub release page.
- Binding conformance contract (`BC-01..BC-10`) documented in `docs/BINDING_CONFORMANCE.md`.

### Public API surface

- Locked down ahead of `1.0.0`: only `graphdblite::*` is public; internal modules are `pub(crate)`. See `STABILITY.md`.
- `Record` exposes accessors only (no public field).
- API surface verifiable via `cargo public-api --simplified`.
