# OPTIONAL MATCH, SET Extensions, & Aggregation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unlock ~100+ TCK scenarios by fixing OPTIONAL MATCH edge cases, adding SET label/map forms, per-function DISTINCT in aggregation, and percentileDisc/percentileCont.

**Architecture:** OPTIONAL MATCH infrastructure (grammar, AST, planner, executor) is already complete — the remaining failures are edge cases in the executor's `exec_left_outer_join` and property-access-on-null handling. SET extensions require a new `SetItem` enum to generalize beyond property-only assignments. Aggregation needs `distinct: bool` threaded from grammar → AST → IR → executor, plus two new `AggregateFunction` variants.

**Tech Stack:** Rust, pest (PEG parser), SQLite (via rusqlite), rmp-serde (MessagePack)

---

## File Structure

| File | Responsibility | Changes |
|------|---------------|---------|
| `src/cypher/grammar.pest` | PEG grammar | Add `set_item`, `set_label`, `set_map`, `set_map_merge`; add `distinct_keyword` inside `function_args`; add `percentileDisc`/`percentileCont` to `function_name` |
| `src/cypher/ast.rs` | AST types | Add `SetItem` enum; add `distinct: bool` to `FunctionCall` |
| `src/cypher/parser.rs` | Parse tree → AST | Parse new SET forms; extract DISTINCT from function args; parse new agg functions |
| `src/cypher/ir.rs` | Logical plan IR | Add `distinct: bool` to `AggregateExpr`; add `PercentileDisc`/`PercentileCont` to `AggregateFunction`; add `SetLabel`/`SetProperties` IR ops |
| `src/cypher/planner.rs` | AST → IR translation | Handle `SetItem` variants; thread DISTINCT; recognize new agg function names |
| `src/cypher/executor.rs` | IR → execution | Implement `SetLabel`/`SetProperties` execution; dedup values when `agg.distinct`; implement percentile math; fix null-property access for OPTIONAL MATCH |
| `src/node.rs` | Node storage | Add `add_node_label()`, `set_all_node_properties()` |
| `tests/tck/skiplist.txt` | TCK skip list | Remove entries as tests start passing |

---

### Task 1: Fix OPTIONAL MATCH — Unskip and diagnose

The OPTIONAL MATCH infrastructure (LeftOuterJoin, exec_left_outer_join, plan_optional_patterns) is fully implemented. The remaining 18+ skipped tests likely fail on edge cases. The first step is removing them from the skiplist and seeing which actually fail.

**Files:**
- Modify: `tests/tck/skiplist.txt`

- [ ] **Step 1: Remove OPTIONAL MATCH entries from skiplist**

Remove these lines from `tests/tck/skiplist.txt`:

```
Match7 - Optional match::[3] OPTIONAL MATCH and bound nodes
Match7 - Optional match::[7] MATCH with OPTIONAL MATCH in longer pattern
Match7 - Optional match::[9] Longer pattern with bound nodes
Match7 - Optional match::[11] Return two subgraphs with bound undirected relationship and optional relationship
Match7 - Optional match::[12] Variable length optional relationships
Match7 - Optional match::[16] Optionally matching named paths - null result
Match7 - Optional match::[17] Optionally matching named paths - existing result
Match7 - Optional match::[18] Named paths inside optional matches with node predicates
Match7 - Optional match::[19] Optionally matching named paths with single and variable length patterns
Match7 - Optional match::[20] Variable length optional relationships with bound nodes, no matches
Match7 - Optional match::[22] MATCH after OPTIONAL MATCH
Match7 - Optional match::[23] OPTIONAL MATCH with labels on the optional end node
Match7 - Optional match::[24] Optionally matching self-loops
Match7 - Optional match::[25] Optionally matching self-loops without matches
Match7 - Optional match::[26] Handling correlated optional matches; first does not match implies second does not match
Match7 - Optional match::[29] Satisfies the open world assumption, relationships between same nodes
Match7 - Optional match::[30] Satisfies the open world assumption, single relationship
Match7 - Optional match::[31] Satisfies the open world assumption, relationships between different nodes
MatchWhere6 - Filter optional matches::[1] Filter node with node label predicate on multi variables with multiple bindings after MATCH and OPTIONAL MATCH
MatchWhere6 - Filter optional matches::[2] Filter node with false node label predicate after OPTIONAL MATCH
MatchWhere6 - Filter optional matches::[3] Filter node with property predicate on multi variables with multiple bindings after OPTIONAL MATCH
MatchWhere6 - Filter optional matches::[4] Do not fail when predicates on optionally matched and missed nodes are invalid
MatchWhere6 - Filter optional matches::[5] Matching and optionally matching with unbound nodes and equality predicate in reverse direction
MatchWhere6 - Filter optional matches::[6] Join nodes on non-equality of properties – OPTIONAL MATCH and WHERE
MatchWhere6 - Filter optional matches::[7] Join nodes on non-equality of properties – OPTIONAL MATCH on two relationships and WHERE
MatchWhere6 - Filter optional matches::[8] Join nodes on non-equality of properties – Two OPTIONAL MATCH clauses and WHERE
Null1 - IS NULL validation::[2] Property null check on optional non-null node
Null2 - IS NOT NULL validation::[2] Property not null check on optional non-null node
```

- [ ] **Step 2: Run TCK tests and capture failures**

Run: `cargo test --test tck 2>&1 | tee /tmp/tck_optional.txt`

Expected: Some tests pass (already implemented correctly), some fail. Capture the exact failure output to diagnose each one.

- [ ] **Step 3: Categorize failures**

Review the output. Group failures by root cause:
- Property access on null node (e.g., `null.name` should return null, not error)
- Named path materialization when optional pattern doesn't match
- Variable-length paths in optional patterns
- Chained OPTIONAL MATCH with null propagation

Record the categories and which tests fall into each. This drives tasks 2–4.

- [ ] **Step 4: Commit**

```bash
git add tests/tck/skiplist.txt
git commit -m "chore: unskip OPTIONAL MATCH TCK tests for diagnosis"
```

---

### Task 2: Fix property access on null values

When an OPTIONAL MATCH produces null for a node variable, expressions like `n.name` should evaluate to `null` rather than erroring. This is the most common OPTIONAL MATCH failure pattern.

**Files:**
- Modify: `src/cypher/eval.rs` (property access evaluation)
- Modify: `src/cypher/executor.rs` (record property lookup)

- [ ] **Step 1: Read eval.rs to find property access handling**

Read `src/cypher/eval.rs` and find where `Expr::Property(var, prop)` is evaluated. Check whether it handles the case where the variable's value is `Value::Null` in the record.

- [ ] **Step 2: Fix eval_expr for Property on null**

In `src/cypher/eval.rs`, in the `Expr::Property(var, prop)` match arm, when the variable resolves to `Value::Null`, return `Value::Null` instead of looking up the flattened key.

The current code likely does something like:
```rust
Expr::Property(var, prop) => {
    let key = format!("{var}.{prop}");
    Ok(rec.get(&key).cloned().unwrap_or(Value::Null))
}
```

This already returns Null for missing keys, but the issue may be in how node/edge compound values interact. Check if the variable itself is bound to `Value::Null` (from OPTIONAL MATCH no-match) and return early:

```rust
Expr::Property(var, prop) => {
    // If the variable is NULL (e.g. from unmatched OPTIONAL MATCH), property access is NULL.
    if matches!(rec.get(var), Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let key = format!("{var}.{prop}");
    Ok(rec.get(&key).cloned().unwrap_or(Value::Null))
}
```

- [ ] **Step 3: Run TCK tests**

Run: `cargo test --test tck 2>&1 | grep -E "FAIL|failed|✘" | head -20`

Expected: Fewer failures than step 2 of task 1.

- [ ] **Step 4: Commit**

```bash
git add src/cypher/eval.rs
git commit -m "fix: return null for property access on null OPTIONAL MATCH variables"
```

---

### Task 3: Fix LeftOuterJoin null-filling for flattened properties

When OPTIONAL MATCH doesn't match, the executor sets `alias → Null` for the optional variable. But downstream operators may also try to access `alias.prop`, `alias.__labels`, `alias.__src`, etc. These flattened keys need to be null-filled too.

**Files:**
- Modify: `src/cypher/executor.rs:1739-1775` (`exec_left_outer_join`)

- [ ] **Step 1: Read exec_left_outer_join**

Read `src/cypher/executor.rs` lines 1739-1775 to understand current null-filling.

- [ ] **Step 2: Extend null-filling to cover common flattened keys**

The current code only sets `rec.set(alias, Value::Null)`. It needs to also propagate null to `alias.__labels` so that `labels(n)` returns null when n is unmatched. And for relationship aliases, null-fill `alias.__src`, `alias.__dst`, `alias.__type`.

In `exec_left_outer_join`, in the `right_records.is_empty()` branch, after setting the alias to null, also check if the right plan would have produced flattened keys. A simpler approach: just set the common metadata keys to null:

```rust
if right_records.is_empty() {
    let mut rec = l_rec.clone();
    for alias in optional_aliases {
        rec.set(alias.clone(), Value::Null);
        // Null-fill flattened metadata keys so downstream property access
        // and functions (labels(), type()) return null rather than missing.
        rec.set(format!("{alias}.__labels"), Value::Null);
        rec.set(format!("{alias}.__src"), Value::Null);
        rec.set(format!("{alias}.__dst"), Value::Null);
        rec.set(format!("{alias}.__type"), Value::Null);
    }
    results.push(rec);
}
```

- [ ] **Step 3: Run TCK tests**

Run: `cargo test --test tck 2>&1 | grep -c "passed"` and check for regressions.

- [ ] **Step 4: Commit**

```bash
git add src/cypher/executor.rs
git commit -m "fix: null-fill flattened property keys for unmatched OPTIONAL MATCH"
```

---

### Task 4: Fix remaining OPTIONAL MATCH edge cases

After tasks 2 and 3, re-run the TCK and fix any remaining OPTIONAL MATCH failures. Common remaining issues:

**Files:**
- Modify: Various files depending on what still fails

- [ ] **Step 1: Run TCK and identify remaining failures**

Run: `cargo test --test tck 2>&1 | tee /tmp/tck_optional2.txt`

Check which Match7/MatchWhere6/Null1/Null2 tests still fail.

- [ ] **Step 2: Fix failures iteratively**

For each remaining failure, diagnose and fix. Common patterns:
- **Named paths**: `MaterializePath` needs to handle null node/rel aliases (return null path)
- **WHERE on optional vars**: predicates involving null vars should evaluate to false, not error
- **Chained OPTIONAL MATCH**: second OPTIONAL MATCH on null input should produce null output
- **Variable-length in optional**: the var-length expansion may need to handle bound-to-null source node

Fix each issue in the relevant file.

- [ ] **Step 3: Re-add any tests that genuinely can't pass yet to skiplist**

If any tests require features not yet implemented (e.g., CALL procedures, pattern comprehensions), re-add only those to the skiplist with a comment explaining the blocker.

- [ ] **Step 4: Run full test suite**

Run: `cargo test 2>&1`

Expected: All unit tests pass, TCK regressions = 0.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "fix: resolve remaining OPTIONAL MATCH edge cases"
```

---

### Task 5: Add `add_node_label()` to node storage

**Files:**
- Modify: `src/node.rs:125-141` (near `remove_node_label`)

- [ ] **Step 1: Write `add_node_label` function**

Add this function in `src/node.rs` after `remove_node_label`:

```rust
/// Add a label to an existing node. No-op if the label already exists.
pub fn add_node_label(conn: &Connection, id: NodeId, label: &str) -> Result<()> {
    validate_name(label)?;
    let data =
        kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord =
        rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
    if record.labels.contains(&label.to_string()) {
        return Ok(());
    }
    record.labels.push(label.to_string());
    record.labels.sort();
    let new_data =
        rmp_serde::to_vec(&record).map_err(|e| GraphError::Serialization(e.to_string()))?;
    let label_col = record.labels.join(":");
    put_node(conn, &id.to_be_bytes(), &label_col, &new_data)?;
    stats::increment_label_count(conn, label)?;
    Ok(())
}
```

- [ ] **Step 2: Write `set_all_node_properties` function**

Add this function in `src/node.rs` after `set_node_property`. This replaces ALL properties on a node (used by `SET n = {map}`):

```rust
/// Replace all properties on a node with the given map.
pub fn set_all_node_properties(conn: &Connection, id: NodeId, properties: Properties) -> Result<()> {
    for key in properties.keys() {
        validate_name(key)?;
    }
    let data =
        kv::get(conn, kv::TABLE_NODES, &id.to_be_bytes())?.ok_or(GraphError::NodeNotFound(id))?;
    let mut record: NodeRecord =
        rmp_serde::from_slice(&data).map_err(|e| GraphError::Serialization(e.to_string()))?;
    record.properties = properties;
    let new_data =
        rmp_serde::to_vec(&record).map_err(|e| GraphError::Serialization(e.to_string()))?;
    let label_col = record.labels.join(":");
    put_node(conn, &id.to_be_bytes(), &label_col, &new_data)?;
    Ok(())
}
```

- [ ] **Step 3: Run existing tests**

Run: `cargo test`

Expected: All existing tests still pass (new functions are unused so far).

- [ ] **Step 4: Commit**

```bash
git add src/node.rs
git commit -m "feat: add add_node_label and set_all_node_properties to node storage"
```

---

### Task 6: Extend SET grammar and AST for labels and maps

**Files:**
- Modify: `src/cypher/grammar.pest:113-124`
- Modify: `src/cypher/ast.rs:94-105, 276-282`

- [ ] **Step 1: Update grammar**

Replace the `assignment_list` and `assignment` rules in `src/cypher/grammar.pest` (lines 123-124):

```pest
assignment_list = { set_item ~ ("," ~ set_item)* }
set_item = { set_label | set_map_merge | set_map | assignment }
set_label = { ident ~ label_spec }
set_map = { ident ~ "=" ~ expr }
set_map_merge = { ident ~ "+=" ~ expr }
assignment = { property_access ~ "=" ~ expr }
```

Note: `set_map_merge` must come before `set_map` in `set_item` because PEG is ordered-choice and `+=` would otherwise match `+` as part of an expr after `=`. Also `set_label` must come before `assignment` because `n:Foo` would fail to parse as `property_access`.

**Important**: The `on_create_clause` and `on_match_clause` rules (line 150-151) still use `assignment_list` which now includes `set_item`. That's fine — MERGE ON CREATE/ON MATCH only use property assignments in practice.

- [ ] **Step 2: Add SetItem enum to AST**

In `src/cypher/ast.rs`, add the `SetItem` enum after the `Assignment` struct:

```rust
/// An item in a SET clause: property assignment, label, or map assignment.
#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// SET n.prop = value
    Property(Assignment),
    /// SET n:Label:Label2
    Label { variable: String, labels: Vec<String> },
    /// SET n = {map} (replace all properties)
    MapOverwrite { variable: String, value: Expr },
    /// SET n += {map} (merge properties)
    MapMerge { variable: String, value: Expr },
}
```

- [ ] **Step 3: Update SetStatement to use SetItem**

Change `SetStatement.assignments` from `Vec<Assignment>` to `Vec<SetItem>`:

```rust
pub struct SetStatement {
    pub patterns: Vec<Pattern>,
    pub optional_patterns: Vec<Vec<Pattern>>,
    pub where_clause: Option<Expr>,
    pub items: Vec<SetItem>,  // was: assignments: Vec<Assignment>
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
}
```

- [ ] **Step 4: Fix all compilation errors from the rename**

After changing `assignments` → `items` and `Vec<Assignment>` → `Vec<SetItem>`, fix references in:
- `src/cypher/parser.rs` (`parse_set` function)
- `src/cypher/planner.rs` (`plan_set` function)
- `src/cypher/executor.rs` (`exec_set_property` function)
- `src/cypher/ir.rs` (`SetProperty` variant)

For now, wrap existing `Assignment` values in `SetItem::Property(assignment)` to maintain current behavior while getting the code to compile.

- [ ] **Step 5: Run tests**

Run: `cargo test`

Expected: All tests pass — this is a refactor, no behavior change yet.

- [ ] **Step 6: Commit**

```bash
git add src/cypher/grammar.pest src/cypher/ast.rs src/cypher/parser.rs src/cypher/planner.rs src/cypher/executor.rs src/cypher/ir.rs
git commit -m "refactor: introduce SetItem enum for SET clause extensibility"
```

---

### Task 7: Parse SET label, map, and map-merge forms

**Files:**
- Modify: `src/cypher/parser.rs:406-446, 1055-1074`

- [ ] **Step 1: Update parse_set to handle set_item variants**

Replace the `parse_assignment_list` call in `parse_set` with a new `parse_set_item_list` that dispatches on the grammar rule:

```rust
fn parse_set_item_list(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Vec<SetItem>> {
    pair.into_inner()
        .filter(|p| p.as_rule() == Rule::set_item)
        .map(|p| {
            let inner = p.into_inner().next().unwrap();
            match inner.as_rule() {
                Rule::set_label => {
                    let mut parts = inner.into_inner();
                    let variable = parts.next().unwrap().as_str().to_string();
                    let label_spec = parts.next().unwrap();
                    let labels: Vec<String> = label_spec
                        .into_inner()
                        .map(|s| s.as_str().to_string())
                        .collect();
                    Ok(SetItem::Label { variable, labels })
                }
                Rule::set_map_merge => {
                    let mut parts = inner.into_inner();
                    let variable = parts.next().unwrap().as_str().to_string();
                    let value = parse_expr(parts.next().unwrap())?;
                    Ok(SetItem::MapMerge { variable, value })
                }
                Rule::set_map => {
                    let mut parts = inner.into_inner();
                    let variable = parts.next().unwrap().as_str().to_string();
                    let value = parse_expr(parts.next().unwrap())?;
                    Ok(SetItem::MapOverwrite { variable, value })
                }
                Rule::assignment => {
                    let mut children = inner.into_inner();
                    let prop_access = children.next().unwrap();
                    let mut prop_parts = prop_access.into_inner();
                    let variable = prop_parts.next().unwrap().as_str().to_string();
                    let property = prop_parts.next().unwrap().as_str().to_string();
                    let value = parse_expr(children.next().unwrap())?;
                    Ok(SetItem::Property(Assignment { variable, property, value }))
                }
                _ => Err(GraphError::parse("unexpected SET item rule")),
            }
        })
        .collect()
}
```

- [ ] **Step 2: Update parse_set to use the new parser**

In `parse_set`, change the `Rule::assignment_list` arm to:

```rust
Rule::assignment_list => items = parse_set_item_list(inner)?,
```

- [ ] **Step 3: Keep parse_assignment_list for MERGE ON CREATE/ON MATCH**

The existing `parse_assignment_list` is still used by MERGE's `on_create_clause` and `on_match_clause`. Keep it as-is — those only need property assignments.

- [ ] **Step 4: Run tests**

Run: `cargo test`

Expected: Parser can now parse `SET n:Foo`, `SET n = {map}`, `SET n += {map}` in addition to `SET n.prop = val`.

- [ ] **Step 5: Commit**

```bash
git add src/cypher/parser.rs
git commit -m "feat: parse SET label, map overwrite, and map merge forms"
```

---

### Task 8: Plan and execute SET label/map operations

**Files:**
- Modify: `src/cypher/ir.rs:119-123`
- Modify: `src/cypher/planner.rs:384-416`
- Modify: `src/cypher/executor.rs:1159-1211`

- [ ] **Step 1: Add new IR operators**

In `src/cypher/ir.rs`, replace the `SetProperty` variant with a more general one, or add new variants alongside it. The simplest approach: keep `SetProperty` for `Assignment` and add:

```rust
/// Add labels to nodes.
SetLabel {
    input: Box<LogicalOp>,
    variable: String,
    labels: Vec<String>,
},

/// Replace all properties on a node/edge with a map expression.
SetProperties {
    input: Box<LogicalOp>,
    variable: String,
    value: Expr,
    merge: bool, // false = overwrite (SET n = {}), true = merge (SET n += {})
},
```

- [ ] **Step 2: Update planner**

In `plan_set` in `src/cypher/planner.rs`, iterate over `stmt.items` and build a chain of IR operators:

```rust
fn plan_set(conn: &Connection, stmt: &SetStatement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_patterns(conn, &stmt.patterns)?;

    // Optional MATCH clauses.
    let mut bound_vars = collect_pattern_variables(&stmt.patterns);
    for opt_patterns in &stmt.optional_patterns {
        let (right, new_aliases) = plan_optional_patterns(conn, opt_patterns, &bound_vars)?;
        op = LogicalOp::LeftOuterJoin {
            input: Box::new(op),
            right: Box::new(right),
            optional_aliases: new_aliases.clone(),
        };
        bound_vars.extend(new_aliases);
    }

    if let Some(ref predicate) = stmt.where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    // Build one IR operator per SET item.
    for item in &stmt.items {
        match item {
            SetItem::Property(assignment) => {
                op = LogicalOp::SetProperty {
                    input: Box::new(op),
                    assignments: vec![assignment.clone()],
                };
            }
            SetItem::Label { variable, labels } => {
                op = LogicalOp::SetLabel {
                    input: Box::new(op),
                    variable: variable.clone(),
                    labels: labels.clone(),
                };
            }
            SetItem::MapOverwrite { variable, value } => {
                op = LogicalOp::SetProperties {
                    input: Box::new(op),
                    variable: variable.clone(),
                    value: value.clone(),
                    merge: false,
                };
            }
            SetItem::MapMerge { variable, value } => {
                op = LogicalOp::SetProperties {
                    input: Box::new(op),
                    variable: variable.clone(),
                    value: value.clone(),
                    merge: true,
                };
            }
        }
    }

    if let Some(ref rc) = stmt.return_clause {
        op = apply_return_projection(op, rc, &stmt.order_by, stmt.skip, stmt.limit)?;
    }

    Ok(op)
}
```

- [ ] **Step 3: Add exec dispatch for new IR operators**

In `src/cypher/executor.rs`, add match arms in the `exec` function for `SetLabel` and `SetProperties`. Also add the helper functions:

```rust
LogicalOp::SetLabel { input, variable, labels } => {
    exec_set_label(conn, input, variable, labels, ctx)
}
LogicalOp::SetProperties { input, variable, value, merge } => {
    exec_set_properties(conn, input, variable, value, *merge, ctx)
}
```

- [ ] **Step 4: Implement exec_set_label**

```rust
fn exec_set_label(
    conn: &Connection,
    input: &LogicalOp,
    variable: &str,
    labels: &[String],
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        if let Some(Value::I64(id)) = rec.get(variable) {
            let node_id = NodeId(*id as u64);
            for label in labels {
                node::add_node_label(conn, node_id, label)?;
            }
            // Update the labels in the record for downstream RETURN.
            let label_key = format!("{variable}.__labels");
            if let Some(Value::List(current_labels)) = rec.get(&label_key) {
                let mut updated = current_labels.clone();
                for label in labels {
                    let lv = Value::String(label.clone());
                    if !updated.contains(&lv) {
                        updated.push(lv);
                    }
                }
                updated.sort_by(|a, b| {
                    if let (Value::String(sa), Value::String(sb)) = (a, b) {
                        sa.cmp(sb)
                    } else {
                        std::cmp::Ordering::Equal
                    }
                });
                rec.set(label_key, Value::List(updated));
            }
        }
        // Skip if variable is Null (from OPTIONAL MATCH with no match).
    }
    Ok(records)
}
```

- [ ] **Step 5: Implement exec_set_properties**

```rust
fn exec_set_properties(
    conn: &Connection,
    input: &LogicalOp,
    variable: &str,
    value_expr: &Expr,
    merge: bool,
    ctx: &ExecContext,
) -> Result<Vec<Record>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        if let Some(Value::I64(id)) = rec.get(variable) {
            let node_id = NodeId(*id as u64);
            let map_val = eval_expr(value_expr, rec, conn)?;
            let new_props = match map_val {
                Value::Map(m) => m,
                _ => continue, // null or non-map — skip
            };

            let old = node::get_node(conn, node_id)?;

            let mut final_props = if merge {
                old.properties.clone()
            } else {
                Properties::new()
            };

            // Apply new properties (null values mean remove).
            for (k, v) in &new_props {
                match v {
                    Value::Null => { final_props.remove(k); }
                    _ => { final_props.insert(k.clone(), v.clone()); }
                }
            }

            // If overwrite mode, remove old properties not in the new map.
            // (Already handled by starting from empty Properties above.)

            node::set_all_node_properties(conn, node_id, final_props.clone())?;
            index::update_indexes_for_node(
                conn,
                node_id,
                old.labels.first().map(|s| s.as_str()).unwrap_or(""),
                Some(&old.properties),
                &final_props,
            )?;

            // Update record: remove old prop keys, add new ones.
            let prefix = format!("{variable}.");
            let old_keys: Vec<String> = rec.fields.keys()
                .filter(|k| k.starts_with(&prefix) && !k.contains("__"))
                .cloned()
                .collect();
            for k in old_keys {
                rec.set(k, Value::Null);
            }
            for (k, v) in &final_props {
                rec.set(format!("{variable}.{k}"), v.clone());
            }
        }
        // Skip if variable is Null (from OPTIONAL MATCH with no match).
    }
    Ok(records)
}
```

- [ ] **Step 6: Update is_bare_write and is_read_only for new ops**

Add `LogicalOp::SetLabel { .. }` and `LogicalOp::SetProperties { .. }` to the `is_bare_write` and `is_read_only` match arms in `executor.rs`.

- [ ] **Step 7: Run tests**

Run: `cargo test`

Expected: Existing tests pass. New SET forms are now executable.

- [ ] **Step 8: Commit**

```bash
git add src/cypher/ir.rs src/cypher/planner.rs src/cypher/executor.rs
git commit -m "feat: implement SET label, map overwrite, and map merge execution"
```

---

### Task 9: Unskip SET TCK tests

**Files:**
- Modify: `tests/tck/skiplist.txt`

- [ ] **Step 1: Remove SET3, SET4, SET5 entries from skiplist**

Remove these lines:
```
Set3 - Set a Label::[1] Add a single label to a node with no label
Set3 - Set a Label::[2] Adding multiple labels to a node with no label
Set3 - Set a Label::[3] Add a single label to a node with an existing label
Set3 - Set a Label::[4] Adding multiple labels to a node with an existing label
Set3 - Set a Label::[5] Ignore whitespace before colon 1
Set3 - Set a Label::[8] Ignore null when setting label
Set4 - Set all properties with a map::[1] Set multiple properties with a property map
Set4 - Set all properties with a map::[2] Non-existent values in a property map are removed with SET
Set4 - Set all properties with a map::[3] Null values in a property map are removed with SET
Set4 - Set all properties with a map::[4] All properties are removed if node is set to empty property map
Set4 - Set all properties with a map::[5] Ignore null when setting properties using an overriding map
Set5 - Set multiple properties with a map::[1] Ignore null when setting properties using an appending map
Set5 - Set multiple properties with a map::[2] Overwrite values when using +=
Set5 - Set multiple properties with a map::[3] Retain old values when using +=
Set5 - Set multiple properties with a map::[4] Explicit null values in a map remove old values
Set5 - Set multiple properties with a map::[5] Set an empty map when using += has no effect
```

Also remove Set6 entries that only depend on SET label/map (not on other missing features):
```
Set6 - Persistence of set clause side effects::[8] Limiting to zero results after adding a label on nodes affects the result set but not the side effects
Set6 - Persistence of set clause side effects::[9] Skipping all results after adding a label on nodes affects the result set but not the side effects
Set6 - Persistence of set clause side effects::[10] Skipping and limiting to a few results after adding a label on nodes affects the result set but not the side effects
Set6 - Persistence of set clause side effects::[11] Skipping zero result and limiting to all results after adding a label on nodes does not affect the result set nor the side effects
Set6 - Persistence of set clause side effects::[12] Filtering after adding a label on nodes affects the result set but not the side effects
Set6 - Persistence of set clause side effects::[13] Aggregating in `RETURN` after adding a label on nodes affects the result set but not the side effects
Set6 - Persistence of set clause side effects::[14] Aggregating in `WITH` after adding a label on nodes affects the result set but not the side effects
```

- [ ] **Step 2: Run TCK tests**

Run: `cargo test --test tck 2>&1 | tee /tmp/tck_set.txt`

Expected: Most Set3/Set4/Set5 tests pass. Fix any remaining issues.

- [ ] **Step 3: Fix any remaining failures**

Debug and fix issues. Common problems:
- Grammar ordering issues (PEG choice order)
- Map literal evaluation returning wrong Value type
- Side-effect counting for labels

- [ ] **Step 4: Run full test suite**

Run: `cargo test`

Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add tests/tck/skiplist.txt src/
git commit -m "feat: SET label and map forms pass TCK tests"
```

---

### Task 10: Add DISTINCT to aggregate function calls

**Files:**
- Modify: `src/cypher/grammar.pest:282-285`
- Modify: `src/cypher/ast.rs:307-308`
- Modify: `src/cypher/parser.rs:1543-1569`

- [ ] **Step 1: Update grammar for DISTINCT in function args**

In `src/cypher/grammar.pest`, modify the `function_args` rule to allow DISTINCT:

```pest
function_args = { star | distinct_keyword ~ expr_list | expr_list | "" }
```

This allows `count(DISTINCT x)` and `collect(DISTINCT x)`.

- [ ] **Step 2: Add distinct flag to FunctionCall AST**

In `src/cypher/ast.rs`, change the `FunctionCall` variant:

```rust
/// Function call: name(args), optionally with DISTINCT
FunctionCall { name: String, args: Vec<Expr>, distinct: bool },
```

- [ ] **Step 3: Fix all FunctionCall construction sites**

Search for all `Expr::FunctionCall {` construction sites and add `distinct: false`. Key locations:
- `src/cypher/parser.rs` (parse_function_call, parse_dotted_function_call)
- `src/cypher/eval.rs` (any constructed FunctionCall exprs)
- `src/cypher/planner.rs` (is_aggregate_fn, split_aggregates)

- [ ] **Step 4: Parse DISTINCT in function args**

In `parse_function_call` in `src/cypher/parser.rs`, detect the `distinct_keyword` rule and set the flag:

```rust
fn parse_function_call(pair: pest::iterators::Pair<Rule>) -> crate::types::Result<Expr> {
    let mut name = String::new();
    let mut args = Vec::new();
    let mut distinct = false;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::function_name => name = inner.as_str().to_lowercase(),
            Rule::function_args => {
                for arg in inner.into_inner() {
                    match arg.as_rule() {
                        Rule::star => args.push(Expr::Star),
                        Rule::distinct_keyword => distinct = true,
                        Rule::expr_list => {
                            for expr_pair in arg.into_inner() {
                                if expr_pair.as_rule() == Rule::expr {
                                    args.push(parse_expr(expr_pair)?);
                                }
                            }
                        }
                        Rule::expr => args.push(parse_expr(arg)?),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(Expr::FunctionCall { name, args, distinct })
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test`

Expected: All tests pass. DISTINCT is now parsed but not yet used in aggregation.

- [ ] **Step 6: Commit**

```bash
git add src/cypher/grammar.pest src/cypher/ast.rs src/cypher/parser.rs src/cypher/eval.rs src/cypher/planner.rs
git commit -m "feat: parse DISTINCT modifier in aggregate function calls"
```

---

### Task 11: Thread DISTINCT through IR and execute it

**Files:**
- Modify: `src/cypher/ir.rs:199-204`
- Modify: `src/cypher/planner.rs:1492-1525`
- Modify: `src/cypher/executor.rs:777-884`

- [ ] **Step 1: Add distinct to AggregateExpr**

In `src/cypher/ir.rs`:

```rust
pub struct AggregateExpr {
    pub function: AggregateFunction,
    pub input: Expr,
    pub alias: Option<String>,
    pub distinct: bool,
}
```

- [ ] **Step 2: Thread distinct in split_aggregates**

In `src/cypher/planner.rs`, in `split_aggregates`, extract the `distinct` flag from the `Expr::FunctionCall`:

```rust
Expr::FunctionCall { name, args, distinct } => {
    let function = match name.as_str() {
        "count" => Some(AggregateFunction::Count),
        // ... etc
    };
    if let Some(function) = function {
        let input = args.first().cloned().unwrap_or(Expr::Star);
        aggregates.push(AggregateExpr {
            function,
            input,
            alias: item.alias.clone(),
            distinct: *distinct,
        });
    } else {
        group_keys.push(item.expr.clone());
    }
}
```

- [ ] **Step 3: Implement DISTINCT deduplication in compute_aggregate**

In `src/cypher/executor.rs`, in `compute_aggregate`, when `agg.distinct` is true, collect and deduplicate values before aggregating:

```rust
fn compute_aggregate(agg: &AggregateExpr, records: &[Record], conn: &Connection) -> Result<Value> {
    // If DISTINCT, pre-collect and deduplicate non-null values.
    let deduped_records: Vec<Record>;
    let effective_records = if agg.distinct && !matches!(agg.input, Expr::Star) {
        let mut seen: Vec<Value> = Vec::new();
        let mut kept = Vec::new();
        for rec in records {
            let val = eval_expr(&agg.input, rec, conn)?;
            if matches!(val, Value::Null) {
                continue;
            }
            if !seen.contains(&val) {
                seen.push(val);
                kept.push(rec.clone());
            }
        }
        deduped_records = kept;
        &deduped_records
    } else {
        records
    };

    match agg.function {
        AggregateFunction::Count => {
            if matches!(agg.input, Expr::Star) {
                Ok(Value::I64(effective_records.len() as i64))
            } else {
                let count = effective_records
                    .iter()
                    .filter(|r| !matches!(eval_expr(&agg.input, r, conn), Ok(Value::Null)))
                    .count();
                Ok(Value::I64(count as i64))
            }
        }
        // ... rest of match arms use effective_records instead of records
```

Replace all `records` references within the match arms with `effective_records`.

- [ ] **Step 4: Run tests**

Run: `cargo test`

Expected: All existing tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/cypher/ir.rs src/cypher/planner.rs src/cypher/executor.rs
git commit -m "feat: implement DISTINCT deduplication in aggregate functions"
```

---

### Task 12: Add percentileDisc and percentileCont

**Files:**
- Modify: `src/cypher/grammar.pest:283, 310`
- Modify: `src/cypher/ir.rs:207-213`
- Modify: `src/cypher/planner.rs:1484-1525`
- Modify: `src/cypher/executor.rs:777-884`

- [ ] **Step 1: Add to grammar**

In `src/cypher/grammar.pest` line 283, add `^"percentileDisc"` and `^"percentileCont"` and `^"stDev"` and `^"stDevP"` to the `function_name` rule.

Also add them to the `keyword` rule (line 310) to prevent them from being parsed as identifiers.

- [ ] **Step 2: Add IR variants**

In `src/cypher/ir.rs`, add to `AggregateFunction`:

```rust
pub enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    PercentileDisc,
    PercentileCont,
    StDev,
    StDevP,
}
```

Also add a `percentile_arg` field to `AggregateExpr` for the second argument:

```rust
pub struct AggregateExpr {
    pub function: AggregateFunction,
    pub input: Expr,
    pub alias: Option<String>,
    pub distinct: bool,
    /// Second argument for percentile functions (the percentile value).
    pub extra_arg: Option<Expr>,
}
```

- [ ] **Step 3: Update split_aggregates**

In `src/cypher/planner.rs`, add matches for the new function names and extract the second argument:

```rust
"percentiledisc" => Some(AggregateFunction::PercentileDisc),
"percentilecont" => Some(AggregateFunction::PercentileCont),
"stdev" => Some(AggregateFunction::StDev),
"stdevp" => Some(AggregateFunction::StDevP),
```

For percentile functions, extract the second arg:
```rust
let extra_arg = if matches!(function, Some(AggregateFunction::PercentileDisc | AggregateFunction::PercentileCont)) {
    args.get(1).cloned()
} else {
    None
};
```

- [ ] **Step 4: Implement percentile computation**

In `src/cypher/executor.rs`, add match arms in `compute_aggregate`:

```rust
AggregateFunction::PercentileDisc => {
    let pct = match &agg.extra_arg {
        Some(expr) => match eval_expr(expr, effective_records.first().unwrap_or(&Record::new()), conn)? {
            Value::F64(p) if (0.0..=1.0).contains(&p) => p,
            Value::I64(p) if (0..=1).contains(&p) => p as f64,
            _ => return Err(GraphError::argument("NumberOutOfRange")),
        },
        None => return Err(GraphError::argument("percentileDisc requires two arguments")),
    };
    let mut vals: Vec<f64> = Vec::new();
    for rec in effective_records {
        match eval_expr(&agg.input, rec, conn)? {
            Value::I64(n) => vals.push(n as f64),
            Value::F64(n) => vals.push(n),
            _ => {}
        }
    }
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (pct * (vals.len() - 1) as f64).round() as usize;
    let idx = idx.min(vals.len() - 1);
    Ok(Value::F64(vals[idx]))
}

AggregateFunction::PercentileCont => {
    let pct = match &agg.extra_arg {
        Some(expr) => match eval_expr(expr, effective_records.first().unwrap_or(&Record::new()), conn)? {
            Value::F64(p) if (0.0..=1.0).contains(&p) => p,
            Value::I64(p) if (0..=1).contains(&p) => p as f64,
            _ => return Err(GraphError::argument("NumberOutOfRange")),
        },
        None => return Err(GraphError::argument("percentileCont requires two arguments")),
    };
    let mut vals: Vec<f64> = Vec::new();
    for rec in effective_records {
        match eval_expr(&agg.input, rec, conn)? {
            Value::I64(n) => vals.push(n as f64),
            Value::F64(n) => vals.push(n),
            _ => {}
        }
    }
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pos = pct * (vals.len() - 1) as f64;
    let lower = pos.floor() as usize;
    let upper = pos.ceil() as usize;
    if lower == upper {
        Ok(Value::F64(vals[lower]))
    } else {
        let frac = pos - lower as f64;
        Ok(Value::F64(vals[lower] * (1.0 - frac) + vals[upper] * frac))
    }
}

AggregateFunction::StDev => {
    let mut vals: Vec<f64> = Vec::new();
    for rec in effective_records {
        match eval_expr(&agg.input, rec, conn)? {
            Value::I64(n) => vals.push(n as f64),
            Value::F64(n) => vals.push(n),
            _ => {}
        }
    }
    if vals.len() < 2 {
        return Ok(Value::F64(0.0));
    }
    let mean = vals.iter().sum::<f64>() / vals.len() as f64;
    let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (vals.len() - 1) as f64;
    Ok(Value::F64(variance.sqrt()))
}

AggregateFunction::StDevP => {
    let mut vals: Vec<f64> = Vec::new();
    for rec in effective_records {
        match eval_expr(&agg.input, rec, conn)? {
            Value::I64(n) => vals.push(n as f64),
            Value::F64(n) => vals.push(n),
            _ => {}
        }
    }
    if vals.is_empty() {
        return Ok(Value::F64(0.0));
    }
    let mean = vals.iter().sum::<f64>() / vals.len() as f64;
    let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
    Ok(Value::F64(variance.sqrt()))
}
```

- [ ] **Step 5: Add GraphError::argument variant if missing**

Check if `GraphError` has an `argument` constructor. If not, add one that produces an error the TCK harness recognizes as `ArgumentError`. Check `src/types.rs` or wherever `GraphError` is defined.

- [ ] **Step 6: Run tests**

Run: `cargo test`

Expected: All existing tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/cypher/grammar.pest src/cypher/ir.rs src/cypher/planner.rs src/cypher/executor.rs
git commit -m "feat: add percentileDisc, percentileCont, stDev, stDevP aggregation functions"
```

---

### Task 13: Unskip aggregation TCK tests

**Files:**
- Modify: `tests/tck/skiplist.txt`

- [ ] **Step 1: Remove aggregation entries from skiplist**

Remove these lines:
```
Aggregation6 - Percentiles::[1] `percentileDisc()`
Aggregation6 - Percentiles::[2] `percentileCont()`
Aggregation6 - Percentiles::[3] `percentileCont()` failing on bad arguments
Aggregation6 - Percentiles::[4] `percentileDisc()` failing on bad arguments
Aggregation6 - Percentiles::[5] `percentileDisc()` failing in more involved query
Aggregation8 - DISTINCT::[1] Distinct on unbound node
Aggregation8 - DISTINCT::[2] Distinct on null
Aggregation8 - DISTINCT::[3] Collect distinct nulls
Aggregation8 - DISTINCT::[4] Collect distinct values mixed with nulls
Return5 - Implicit grouping with distinct::[1] DISTINCT inside aggregation should work with lists in maps
Return5 - Implicit grouping with distinct::[3] DISTINCT inside aggregation should work with nested lists in maps
Return5 - Implicit grouping with distinct::[4] DISTINCT inside aggregation should work with nested lists of maps in maps
```

- [ ] **Step 2: Run TCK tests**

Run: `cargo test --test tck 2>&1 | tee /tmp/tck_agg.txt`

Expected: Most aggregation tests pass. Fix any remaining issues.

- [ ] **Step 3: Fix any remaining failures**

Common issues:
- Percentile error tests needing specific error type recognition
- DISTINCT column naming (TCK expects `count(DISTINCT a)` as column header)
- `Aggregation6[5]` is complex (UNWIND + size + pattern comprehension) — may need to stay on skiplist

- [ ] **Step 4: Run full test suite**

Run: `cargo test`

Expected: All tests pass, 0 regressions.

- [ ] **Step 5: Commit**

```bash
git add tests/tck/skiplist.txt src/
git commit -m "feat: aggregation DISTINCT and percentile functions pass TCK tests"
```

---

### Task 14: Final cleanup and progress report

- [ ] **Step 1: Run full test suite**

Run: `cargo test 2>&1 | tail -20`

Confirm: 0 failures.

- [ ] **Step 2: Count TCK improvement**

Run: `cargo test --test tck 2>&1 | grep "scenarios"` and compare to current baseline (1413 passed, 920 skiplist).

- [ ] **Step 3: Run clippy and fmt**

Run: `cargo clippy --all-targets 2>&1 | head -30` and `cargo fmt --check`

Fix any warnings.

- [ ] **Step 4: Commit cleanup**

```bash
git add -A
git commit -m "style: clippy and fmt cleanup after OPTIONAL MATCH, SET, aggregation"
```
