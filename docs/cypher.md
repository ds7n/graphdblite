# Cypher Reference

graphdblite supports a practical subset of the Cypher query language. This reference
covers all supported syntax with examples.

Comments use `// line` or `/* block */` syntax and may appear anywhere
whitespace may. Comment markers inside string literals or backtick-quoted
identifiers are literal text.

```cypher
/* active users */
MATCH (n:User) // only the label scan
RETURN n.name AS `user's name`
```

## Pattern matching

### Node patterns

```cypher
-- Match all nodes with a label
MATCH (n:Person) RETURN n.name

-- Match with property filter in pattern
MATCH (n:Person {name: 'Alice'}) RETURN n.age

-- Match without label (scans all nodes)
MATCH (n) RETURN n

-- Multi-label match (node must have both labels)
MATCH (n:Person:Employee) RETURN n.name
```

### Relationship patterns

```cypher
-- Directed relationship
MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name

-- Bracketless shorthand (matches any edge type)
MATCH (a:Person)-->(b) RETURN a.name, b.name

-- Incoming direction
MATCH (a:Person)<-[:KNOWS]-(b:Person) RETURN a.name, b.name

-- Undirected (matches either direction; one row per relationship, so a
-- pair connected both ways yields both relationships)
MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN a.name, b.name

-- With relationship variable
MATCH (a)-[r:KNOWS]->(b) RETURN a.name, b.name
```

### Variable-length paths

```cypher
-- Exactly 2 hops
MATCH (a:Person)-[:KNOWS*2]->(b:Person) RETURN a.name, b.name

-- Between 1 and 3 hops
MATCH (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN a.name, b.name

-- With a relationship property map, checked at every hop (Cypher
-- equality: {w: 1.0} matches a stored 1)
MATCH (a:Person)-[:KNOWS*1..3 {since: 2020}]->(b:Person) RETURN b.name
```

Within one `MATCH` pattern a relationship is used at most once, across
fixed and variable-length segments alike. Parallel relationships (several
`a-[:R]->b` edges) are distinct relationships: each yields its own row, and
as values they stay distinct (`count(DISTINCT r)`, `=`). `DELETE` and `SET`
on a relationship held as a value (`WITH collect(r) AS rs ... DELETE rs[0]`)
affect only that relationship.

### Multiple patterns

```cypher
-- Cross-product join
MATCH (a:Person), (b:Company) RETURN a.name, b.name

-- Connected patterns
MATCH (a:Person)-[:WORKS_AT]->(c:Company), (a)-[:KNOWS]->(b:Person)
RETURN a.name, b.name, c.name
```

### OPTIONAL MATCH

Returns `null` for unmatched variables instead of filtering the row.

```cypher
MATCH (a:Person)
OPTIONAL MATCH (a)-[:KNOWS]->(b:Person)
RETURN a.name, b.name
```

## Filtering

### WHERE clause

```cypher
MATCH (n:Person)
WHERE n.age > 25 AND n.name <> 'Bob'
RETURN n.name, n.age

-- OR conditions
WHERE n.age < 20 OR n.age > 60

-- NOT
WHERE NOT n.active = false
```

### String predicates

```cypher
WHERE n.name STARTS WITH 'Al'
WHERE n.name ENDS WITH 'son'
WHERE n.name CONTAINS 'li'
```

### Regex match (`=~`)

Returns `true` when the left-hand string matches the right-hand
[Rust regex](https://docs.rs/regex/) pattern *in full*. Patterns are
implicitly anchored — `'hello' =~ 'hello'` is true, but
`'hello world' =~ 'hello'` is false (use `'hello.*'`).

```cypher
MATCH (n:Person) WHERE n.name =~ '(?i)al.*' RETURN n
```

**Supported:** Character classes, anchors, alternation, quantifiers,
inline flags `(?i)` / `(?m)` / `(?s)` / `(?x)` / `(?u)`.

**Not supported:** Backreferences and lookaround (Rust's `regex` crate
is linear-time by construction — ReDoS-safe at the cost of these two
PCRE features). If you need them, restructure the query or open an
issue.

**Indexing:** `=~` predicates always run as a label scan with per-row
evaluation. Full-text (trigram) indexes do not accelerate regex —
combine with an indexed `=` or `CONTAINS` predicate to pre-filter when
selectivity matters.

**Null and type semantics:** Identical to `CONTAINS` / `STARTS WITH` /
`ENDS WITH`. `NULL =~ x` and `x =~ NULL` return `NULL`. Non-string
operands return `NULL`. An invalid regex pattern raises a runtime
`InvalidArgumentValue` error; the pattern is validated on its own before
anchoring, so an unbalanced pattern such as `a)|(b` is rejected rather
than reinterpreted.

### NULL checks

```cypher
WHERE n.email IS NULL
WHERE n.email IS NOT NULL
```

### EXISTS subqueries

Test whether a pattern exists without returning it.

```cypher
MATCH (a:Person)
WHERE EXISTS { (a)-[:KNOWS]->(b:Person) WHERE b.age > 30 }
RETURN a.name
```

The subquery sees the variables of the enclosing query, including in its
`WHERE`, and keeps that correlation through `RETURN`, `ORDER BY`, `SKIP`,
`LIMIT`, `DISTINCT` and `UNWIND`:

```cypher
MATCH (m:Movie)
WHERE EXISTS { MATCH (r:Review) WHERE r.movie_id = m.id RETURN r LIMIT 1 }
RETURN m.title
```

Branches can be combined with `UNION` / `UNION ALL`; the subquery is
true when any branch produces a row:

```cypher
MATCH (p:Person)
WHERE EXISTS { MATCH (p)-[:KNOWS]->() UNION MATCH (p)-[:FOLLOWS]->() }
RETURN p.name
```

`exists((a)-[:KNOWS]->())` is the pattern's boolean (the same as the bare
pattern predicate); `exists(n.prop)` tests that a property is not null.

## Projection

### RETURN

```cypher
-- Specific properties
RETURN a.name, a.age

-- Aliases
RETURN a.name AS person_name, count(*) AS friend_count

-- Expressions
RETURN a.age + 1 AS next_age
```

### ORDER BY

```cypher
RETURN n.name, n.age ORDER BY n.age DESC

-- Multiple sort keys
RETURN n.name, n.age ORDER BY n.age DESC, n.name ASC
```

### LIMIT

```cypher
RETURN n.name ORDER BY n.age DESC LIMIT 10
```

## Intermediate processing

### WITH

Pipe results through intermediate steps. Supports filtering and aggregation.

```cypher
-- Filter on aggregated values
MATCH (a:Person)-[:KNOWS]->(b)
WITH a, count(*) AS cnt
WHERE cnt > 3
RETURN a.name, cnt

-- Chain multiple WITH clauses
MATCH (a:Person)-[:KNOWS]->(b:Person)
WITH a, collect(b.name) AS friends
WITH a, friends, length(friends) AS cnt
WHERE cnt > 1
RETURN a.name, friends

-- Forward a variable into a new MATCH
MATCH (a:Person)
WITH a
MATCH (a)-->(b)
RETURN a.name, b.name
```

`WITH ... WHERE` filters **after** the projection, `ORDER BY`, `SKIP` and
`LIMIT` of that WITH, and tests the projected values (`WITH rand() AS r
WHERE r < 0.5` keeps exactly the rows whose returned `r` is below 0.5):

```cypher
-- Top 3 by score, then keep those above 10 (may return fewer than 3 rows)
MATCH (p:Player)
WITH p ORDER BY p.score DESC LIMIT 3
WHERE p.score > 10
RETURN p.name
```

`ORDER BY` and `WHERE` of a plain WITH may still read variables it does not
project (`WITH a.name AS name ORDER BY a.age WHERE a.age > 18`). After
`DISTINCT` or an aggregation only projected names are in scope, though a
projected expression may be repeated verbatim (`WITH DISTINCT a.name AS name
WHERE a.name = 'B'`); anything else is an `UndefinedVariable` error. The
same applies to a final `RETURN ... ORDER BY`: a variable an earlier WITH
dropped is out of scope.

### UNWIND

Expand a list into individual rows.

```cypher
-- Literal list
UNWIND [1, 2, 3] AS x RETURN x

-- Create nodes from a list
UNWIND ['Alice', 'Bob', 'Carol'] AS name
CREATE (n:Person {name: name})

-- Unwind within a MATCH pipeline
MATCH (n:Person)
WITH collect(n.name) AS names
UNWIND names AS name
RETURN name

-- Look up one node per list element (MATCH / OPTIONAL MATCH directly after UNWIND)
UNWIND $ids AS id
MATCH (n:Person {id: id})
RETURN n.name
```

`UNWIND` produces its rows one at a time (`UNWIND range(1, 5000000)` does
not build the list first). When the rows are returned, the query stops with
a `result set exceeded maximum of N rows` error as soon as row N+1 appears
(`Config::max_result_rows`); when an aggregate consumes them
(`UNWIND range(1, 5000000) AS x RETURN count(*)`) they are streamed and not
counted. Operators that must hold rows (joins, `ORDER BY`, `DISTINCT`,
`collect()`) and the lists a query builds share a per-query memory budget,
`Config::max_materialized_bytes` (default 1 GiB, 0 = unlimited); exceeding it
returns a size-limit error.

## Aggregations

All aggregation functions can be used in `RETURN` or `WITH` clauses. Rows are grouped
by the non-aggregated columns.

| Function | Description |
|----------|-------------|
| `count(*)` | Count all rows |
| `count(expr)` | Count non-NULL values |
| `sum(expr)` | Sum numeric values |
| `avg(expr)` | Average of numeric values |
| `min(expr)` | Minimum value |
| `max(expr)` | Maximum value |
| `collect(expr)` | Collect non-NULL values into a list |
| `percentileDisc(expr, p)` | The input value at percentile `p` (keeps its type) |
| `percentileCont(expr, p)` | Interpolated percentile `p` (a Float) |

Grouping keys, `DISTINCT`, `count(DISTINCT ...)` and `UNION` treat values
that are equal under `=` as one value: `1` and `1.0` (also inside lists and
maps) form a single group, represented by the first value seen. `sum()` of
Integers raises `NumberOutOfRange` on overflow, like `+`; once a Float is
summed the result is a Float. An aggregate may sit inside a larger
expression, including subscripts and `CASE` (`RETURN collect(x)[0]`,
`CASE WHEN count(*) > 1 THEN ... END`).

```cypher
MATCH (a:Person)-[:KNOWS]->(b:Person)
RETURN a.name, count(*) AS friends, collect(b.name) AS friend_names

MATCH (n:Person)
RETURN avg(n.age) AS avg_age, min(n.age) AS youngest, max(n.age) AS oldest
```

## Scalar functions

| Function | Description |
|----------|-------------|
| `length(x)` | Number of edges in a path, characters in a string, or items in a list |
| `size(x)` | Number of characters in a string (not UTF-8 bytes) or items in a list |
| `nodes(path)` | List of node IDs from a path |
| `round(x)` | Nearest integer as a Float, ties away from zero |
| `round(x, precision [, mode])` | Round to `precision` decimal places (negative: tens, hundreds, ...). `mode` is one of `UP`, `DOWN`, `CEILING`, `FLOOR`, `HALF_UP` (default), `HALF_DOWN`, `HALF_EVEN`; rounding uses the shortest decimal form, so `round(2.675, 2)` is `2.68` |
| `split(s, delim)` | Split a string; an empty `delim` splits into characters |
| `toString(x)` | Floats print like Neo4j: `2.5`, `1.0E20`, `1.5E-7`, `Infinity`, `NaN` |
| `toInteger(x)` | Truncates Floats; `NaN` gives `null` |

```cypher
MATCH p = shortestPath((a:Person)-[:KNOWS*]->(b:Person))
WHERE a.name = 'Alice' AND b.name = 'Dave'
RETURN length(p) AS hops, nodes(p) AS path_nodes
```

## List operations

### List literals

```cypher
RETURN [1, 2, 3] AS numbers
RETURN ['a', 'b', 'c'] AS letters
```

### List comprehensions

```cypher
-- Filter and transform a list
RETURN [x IN [1, 2, 3, 4, 5] WHERE x > 2 | x * 10] AS result
-- Returns [30, 40, 50]

-- Filter only (no transform)
RETURN [x IN [1, 2, 3, 4, 5] WHERE x > 3] AS big

-- Transform only (no filter)
RETURN [x IN [1, 2, 3] | x * x] AS squares
```

## Conditionals

### CASE expressions

```cypher
MATCH (n:Person)
RETURN n.name,
  CASE
    WHEN n.age >= 65 THEN 'senior'
    WHEN n.age >= 18 THEN 'adult'
    ELSE 'minor'
  END AS category
```

## Arithmetic

`+ - * / % ^` follow openCypher. Integer overflow raises `NumberOutOfRange`.
Integer division or modulo by zero (`1 / 0`, `1 % 0`) raises an
`ArithmeticError` with code `DivisionByZero`. As soon as one operand is a
Float the result follows IEEE 754: `1 / 0.0` is `Infinity`, `0 / 0.0` and
`1.5 % 0` are `NaN`. Comparisons between an Integer and a Float are exact,
so `9007199254740993 = 9007199254740992.0` is false.

## Temporal values

`date`, `localtime`, `time`, `localdatetime`, `datetime` and `duration`
accept an ISO 8601 string or a map. Without an argument the five instant
constructors return the current time in UTC; every call in one statement
sees the same instant.

- Map keys are checked: an unknown key (`date({yaer: 2024})`), a missing
  year (`date({})`), or a component that does not fit (`localtime({hour:
  4294967297})`) is an `InvalidArgumentValue` error.
- `datetime({epochSeconds: s})` and `datetime({epochMillis: ms})` build an
  instant from the Unix epoch, optionally with `nanosecond` and
  `timezone`:

```cypher
RETURN datetime({epochSeconds: 1700000000, timezone: 'Europe/Stockholm'})
-- 2023-11-14T23:13:20+01:00[Europe/Stockholm]
```

- Adding a duration to a datetime with a named zone shifts months and days
  in local time and seconds as elapsed time, then re-resolves the offset,
  so DST transitions are handled:
  `datetime('2024-03-10T00:30[America/New_York]') + duration({hours: 3})`
  is `2024-03-10T04:30-04:00[America/New_York]`.
- Results outside the representable range (about +-262,000 years, or
  duration components beyond 64 bits) raise `NumberOutOfRange`. Integer
  duration map components are exact (`duration({seconds: 20000000,
  nanoseconds: 1})` keeps the nanosecond). `-duration(...)` negates.

## Graph algorithms

### Path variables

Bind the matched path to a variable.

```cypher
-- Single-node path
MATCH p = (n:Person) RETURN p

-- Path through relationships
MATCH p = (a:Person)-[:KNOWS]->(b:Person) RETURN p, length(p)
```

### shortestPath

Find the shortest path between two nodes.

```cypher
MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Dave'})
MATCH p = shortestPath((a)-[:KNOWS*]->(b))
RETURN p, length(p) AS hops
```

Semantics (match Neo4j's):

- The relationship may be untyped (`-[*]->`, follows every type) or list
  several types (`-[:KNOWS|WORKS_WITH*]->`). Without `*`, it is a single hop.
- The minimum length must be 0 or 1; `*2..` and higher are rejected.
- A pair with no path inside the hop bounds produces no row. Use
  `OPTIONAL MATCH p = shortestPath(...)` to get a `null` path instead.
- When both endpoints are the same node, the zero-length path matches
  only with a minimum length of 0 (`*0..`); with `*` / `*1..` there is no
  row (Neo4j raises an error here by default).
- `allShortestPaths` returns one path per distinct set of relationships, so
  parallel relationships between the same nodes yield separate paths.
- With unbound endpoints, one search runs per node pair. All searches in a
  query share the `Config::max_traversal_work` edge-visit budget (default
  10,000,000); exceeding it returns a size-limit error. Bind or filter the
  endpoints on large graphs.

### allShortestPaths

Find all shortest paths (same minimum length) between two nodes.

```cypher
MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Dave'})
MATCH p = allShortestPaths((a)-[:KNOWS*]->(b))
RETURN p, length(p) AS hops
```

## Mutations

### CREATE

```cypher
-- Create a node
CREATE (n:Person {name: 'Alice', age: 30})

-- Create nodes and a relationship inline
CREATE (:Person {name: 'Alice'})-[:KNOWS]->(:Person {name: 'Bob'})

-- Create a relationship between existing nodes
MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
CREATE (a)-[:KNOWS {since: 2020}]->(b)

-- Create multiple nodes
CREATE (a:Person {name: 'Carol'}), (b:Person {name: 'Dave'})
```

### SET

```cypher
MATCH (n:Person {name: 'Alice'})
SET n.age = 31

-- Set multiple properties
MATCH (n:Person {name: 'Alice'})
SET n.age = 31, n.city = 'NYC'
```

Setting a property to `null` removes it. Property values must be scalars or
lists of scalars; maps are rejected. `SET n = value` replaces all properties
and `SET n += value` merges into them, where `value` is a map, a node or a
relationship (its properties are copied); anything else is a type error.
Each row sees the writes of earlier rows (`UNWIND [1,2,3] AS i SET n.c =
n.c + 1` adds 3). Setting a property on a node or relationship deleted
earlier in the query is an error. `Config::max_property_value_bytes` and
`Config::max_name_bytes` apply to Cypher writes as well as the typed API.

### DELETE

```cypher
-- Delete a node (must have no relationships)
MATCH (n:Person {name: 'Alice'})
DELETE n

-- Delete a node and all its relationships
MATCH (n:Person {name: 'Alice'})
DETACH DELETE n
```

`DELETE`, `SET` and `REMOVE` need a node, relationship or path (or null, which
is skipped). Any other value, such as `WITH n.k AS x DELETE x`, is a type error.

### MERGE

Match-or-create semantics. Works on both nodes and relationships.
Creates the pattern atomically if no match exists.

```cypher
-- Node upsert
MERGE (n:Person {name: 'Alice'})
ON CREATE SET n.created = true
ON MATCH SET n.seen = true

-- Edge upsert — replaces a manual "DELETE existing + CREATE new" workaround
MATCH (a:Fn {name: $src}), (b:Fn {name: $dst})
MERGE (a)-[r:CALLS]->(b)
ON CREATE SET r += $props
ON MATCH  SET r += $props
```

MERGE matches the **whole** pattern, with variables bound by earlier
clauses held fixed, and binds every match (one output row each, ON MATCH
applied to each). Only when nothing matches does it create the pattern: the
relationship plus every node of the pattern that is not already bound, with
the labels and properties written in the pattern. So, with an existing
`(:A {id: 1})` that has no `:R`:

```cypher
-- Creates a new :A {id: 1}, a :B {id: 2} and the :R between them
MERGE (a:A {id: 1})-[:R]->(b:B {id: 2})

-- Reuses the bound a; creates c (even if an unconnected :C {id: 3} exists)
MATCH (a:A {id: 1})
MERGE (a)-[:R]->(c:C {id: 3})

-- A repeated variable is one node: matches or creates a self-loop
MERGE (a:A {id: 1})-[:R]->(a)
```

Property values compare with Cypher equality (`{x: 1.0}` matches a stored
`1`). `MERGE (a)-[:R {x: 1}]->(b)` does not match a parallel
`(a)-[:R {x: 2}]->(b)`. Direction is significant; an undirected
`MERGE (a)-[:R]-(b)` matches either direction and creates `a->b`.

ON CREATE / ON MATCH items may set any variable in scope, not only the
merged one, and can read the merged node or relationship
(`ON MATCH SET r.count = r.count + 1`). Relationship endpoints must be
nodes: an endpoint bound to null or to a non-node value (such as an
integer) is an error.

## Query planning

### EXPLAIN

View the logical plan without executing the query. Shows operators and estimated
row counts.

```cypher
EXPLAIN MATCH (a:Person)-[:KNOWS]->(b:Person)
WHERE a.name = 'Alice'
RETURN a.name, b.name
```

The optimizer automatically selects index lookups when secondary indexes exist on
the filtered property.

### Secondary indexes (`CREATE INDEX` / `DROP INDEX`)

Create and drop btree secondary indexes via Cypher DDL or the Rust API.
A single property or a **composite** (multi-column) list is supported:

```cypher
CREATE INDEX ON :Person(name)
CREATE INDEX ON :Person(tenant_id, ext_id)
DROP INDEX ON :Person(tenant_id, ext_id)
```

```rust
let tx = db.begin_write()?;
tx.create_index("Person", "name")?;
tx.create_composite_index("Person", &["tenant_id", "ext_id"])?;
tx.commit()?;
```

A composite index is used only when **every** one of its columns is
constrained by an equality predicate, whether written inline
(`MATCH (p:Person {tenant_id: 1, ext_id: 'a'})`) or in the `WHERE`
clause (`WHERE p.tenant_id = 1 AND p.ext_id = 'a'`). Among fully covered
indexes the planner picks the widest; ties prefer lexical order. Nodes
missing any covered property are omitted from the index (no NULL
placeholder), so a query that constrains only a leading prefix
(`WHERE p.tenant_id = 1`) cannot use the composite index: it would miss
nodes without `ext_id`. Such a query uses a single-property index on
`tenant_id` if one exists, otherwise a label scan.

## Full-text indexes

Full-text indexes accelerate substring predicates (`CONTAINS`,
`STARTS WITH`, `ENDS WITH`) on opted-in `(label, property)` pairs.
Declare via the Rust API (`Database::create_fulltext_index(label,
property)`) or the Python binding's `WriteTransaction.create_fulltext_index`.
The Cypher query syntax does not change — the planner detects the
index and rewrites matching predicates to use it.

**Semantics:**

- **Case-sensitive.** Matches the openCypher spec exactly.
- **Case-insensitive indexes** (`create_fulltext_index_ci`) also serve the
  `toLower(n.prop) <op> toLower(<term>)` idiom. An index never changes
  results: a plain `CONTAINS` / `STARTS WITH` / `ENDS WITH` served by a CI
  index is still evaluated case-sensitively (FTS rows are candidates that
  are re-checked with the exact predicate).
- **String properties only.** Non-string values for an indexed
  property are silently skipped at index-write time.
- **Rewritable terms.** The search term is evaluated once, before the
  lookup, so a term that reads the scanned node itself
  (`n.t CONTAINS n.u`) or a non-string literal (`n.t CONTAINS 123`) is
  not rewritten; it runs as a label scan. A parameter or expression that
  evaluates to a non-string matches no rows (the predicate is `null`),
  with or without the index.
- **Term-length floor.** Search terms shorter than 3 codepoints
  bypass the index and fall through to a label scan with per-row
  evaluation. Same result set, no perf win.
- **Equality not accelerated.** `=` predicates use the regular
  `IndexLookup` path (or full scan if no regular index exists). A
  regular and fulltext index on the same `(label, property)` are
  both allowed; they serve different operator sets.

**Storage cost:** approximately 3× the size of indexed text. The
trigram tokenizer stores every 3-character substring; avoid
declaring fulltext indexes on very large text columns unless you
need substring search on them.

**Example (Rust):**

```rust
db.begin_write()?;
db.create_fulltext_index("Doc", "body")?;
db.commit()?;

// Queries are unchanged — the planner picks up the index.
let rows = db.execute(
    "MATCH (n:Doc) WHERE n.body CONTAINS 'foo' RETURN n.body"
)?;
```

### OR-chain optimization

A WHERE clause that is a chain of `OR`-connected text predicates
(`CONTAINS` / `STARTS WITH` / `ENDS WITH`) against indexed properties
of the same label plans as a `Union` of FTS lookups, with row dedup:

```cypher
MATCH (n:Doc) WHERE n.title CONTAINS 'x' OR n.body CONTAINS 'x'
RETURN n
```

A single non-FTS-eligible disjunct (e.g. `n.score = 5`) disables the
rewrite — the query falls back to a label scan. Mixed `(A OR B) AND C`
forms are not rewritten in this pass.

**Known limitations (v1):**

- The `CONTAINS` / `STARTS WITH` / `ENDS WITH` rewrite itself does not
  rank results. For BM25-ranked whole-token / phrase / prefix / boolean
  search, use the [`fts.search`](#call-ftssearchlabel-property-query)
  procedure over a word-tokenized index, and read per-row relevance with
  the [`score()`](#scorevariable) function.

## Introspection

### `CALL db.indexes()`

Returns one row per index in the database.

| column     | type   | values |
| ---------- | ------ | ------ |
| `label`    | STRING | label the index covers |
| `property` | STRING | property the index covers |
| `kind`     | STRING | one of `"btree"`, `"fulltext"`, `"fulltext_ci"`, `"fulltext_word"` (see below) |

The `kind` values are:

| `kind`           | index type |
| ---------------- | ---------- |
| `"btree"`        | secondary btree index (single- or multi-property) |
| `"fulltext"`     | trigram FTS5, case-sensitive |
| `"fulltext_ci"`  | trigram FTS5, case-insensitive |
| `"fulltext_word"`| `unicode61` word-tokenized FTS5 (for `fts.search`) |

```cypher
CALL db.indexes() YIELD label, property, kind RETURN *
```

A `(label, property)` pair carrying both a btree and a fulltext index
yields two rows. A **multi-property** index (composite btree or
multi-column FTS) emits **one row per covered property**, all sharing
the same `kind`.

### `CALL db.counts()`

Returns per-label node counts and per-edge-type relationship counts,
maintained incrementally by internal stats counters.

| column   | type    | values |
| -------- | ------- | ------ |
| `kind`   | STRING  | `"label"` or `"edge_type"` |
| `name`   | STRING  | the label or relationship type |
| `count`  | INTEGER | number of nodes with that label / edges of that type |

```cypher
CALL db.counts() YIELD kind, name, count
WHERE kind = 'label' RETURN name, count ORDER BY count DESC
```

### `CALL fts.search(label, property, query)`

BM25-ranked full-text search over a **word-tokenized** (`unicode61`)
fulltext index. Unlike the `CONTAINS`/`STARTS WITH`/`ENDS WITH` rewrite
(substring, unranked), this exposes FTS5 MATCH syntax — whole tokens,
phrases (`"..."`), prefix (`term*`), and boolean (`a OR b`) — and
returns results ordered by descending relevance.

| argument   | meaning |
| ---------- | ------- |
| `label`    | node label to search |
| `property` | a covered column name, or `'*'` to search all columns of the (single) FTS index on `label` |
| `query`    | an FTS5 MATCH expression |

A named `property` on a multi-property index restricts the search to that
column. The restriction is applied outside the query text, so column
filters or parentheses inside `query` (e.g. `x) OR title:(secret`) cannot
reach other columns; such a query matches nothing or is rejected as a
syntax error. Errors for invalid queries say what failed but never repeat
the query text.

Yields:

| column  | type  | values |
| ------- | ----- | ------ |
| `node`  | NODE  | the matching node |
| `score` | FLOAT | relevance (higher = better; negated BM25) |

```cypher
CALL fts.search('Doc', 'body', 'graph AND database')
YIELD node, score
RETURN node.title, score ORDER BY score DESC LIMIT 10
```

Create the backing index with
`Database::create_fulltext_index_word(label, property)` (single column)
or `create_fulltext_index_word_multi(label, properties)` (multi-column,
searchable via `property = '*'` or a specific column). `'*'` requires
exactly one FTS index on the label; with several, name a covered column
to disambiguate.

### `score(variable)`

Reads the BM25 relevance of an FTS-driven match for a node bound by an
FTS-rewritten `CONTAINS` / `STARTS WITH` / `ENDS WITH` predicate:

```cypher
MATCH (n:Doc) WHERE n.body CONTAINS 'graph'
RETURN n.title, score(n) AS relevance ORDER BY relevance DESC
```

Returns a `FLOAT` (higher = better) for FTS-driven scans. Returns
`NULL` for any variable not bound by an FTS lookup (plain label scan,
btree `IndexLookup`, or expand), and `0.0` for a scan that was
FTS-eligible but fell back (e.g. a term below the trigram floor). Note
`score()` is independent of the `fts.search` procedure, which yields its
own `score` column directly.

Other openCypher `db.*` introspection procedures
(`db.labels()`, `db.relationshipTypes()`, `db.propertyKeys()`,
`db.schema.visualization()`) are not yet implemented.
