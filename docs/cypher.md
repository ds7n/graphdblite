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
the filtered property. Create indexes with the Rust API:

```rust
let tx = db.begin_write()?;
tx.create_index("Person", "name")?;
tx.commit()?;
```

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

**Known limitations (v1):**

- OR-chains across multiple FTS indexes do not rewrite
  (`WHERE n.title CONTAINS 'x' OR n.body CONTAINS 'x'` runs as a
  scan even when both columns are indexed). Workaround: use
  separate queries per field and union at the Cypher level.
- No phrase queries, no prefix-with-`*`, no ranking / BM25. Use
  the existing operators only.
