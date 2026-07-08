# Cypher Reference

graphdblite supports a practical subset of the Cypher query language. This reference
covers all supported syntax with examples.

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

-- Undirected (matches either direction)
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
```

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
`InvalidArgumentValue` error.

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
```

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
| `nodes(path)` | List of node IDs from a path |

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

### DELETE

```cypher
-- Delete a node (must have no relationships)
MATCH (n:Person {name: 'Alice'})
DELETE n

-- Delete a node and all its relationships
MATCH (n:Person {name: 'Alice'})
DETACH DELETE n
```

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

Edge identity: `MERGE (a)-[:R]->(b)` finds an existing `(a)-[:R]->(b)`
edge if any exists, otherwise creates one. `MERGE (a)-[:R {x: 1}]->(b)`
is constrained by the inline properties — a parallel
`(a)-[:R {x: 2}]->(b)` edge will not be matched, and a new one is
created. Direction is significant.

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

A composite index uses **leftmost-prefix** matching: the planner picks
the index whose longest leading run of columns is covered by equality
predicates, whether written inline (`MATCH (p:Person {tenant_id: 1,
ext_id: 'a'})`) or in the `WHERE` clause (`WHERE p.tenant_id = 1 AND
p.ext_id = 'a'`). Ties prefer the narrower index, then lexical order.
Nodes missing any covered property are omitted from the index (no NULL
placeholder), so a composite index does not serve a query that filters
only on a non-leading column.

## Full-text indexes

Full-text indexes accelerate substring predicates (`CONTAINS`,
`STARTS WITH`, `ENDS WITH`) on opted-in `(label, property)` pairs.
Declare via the Rust API (`Database::create_fulltext_index(label,
property)`) or the Python binding's `WriteTransaction.create_fulltext_index`.
The Cypher query syntax does not change — the planner detects the
index and rewrites matching predicates to use it.

**Semantics:**

- **Case-sensitive.** Matches the openCypher spec exactly.
- **String properties only.** Non-string values for an indexed
  property are silently skipped at index-write time.
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
