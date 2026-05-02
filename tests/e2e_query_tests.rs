use std::collections::HashMap;

use graphdblite::{Database, NodeId, Value};

/// Extract the sequence of node IDs from a `Value::Path` for assertion convenience.
fn path_ids(val: &Value) -> Vec<u64> {
    match val {
        Value::Path(p) => p.nodes.iter().map(|n| n.id.0).collect(),
        _ => panic!("expected Value::Path, got {val:?}"),
    }
}

/// Extract the id of a single `Value::Node`.
fn node_id(val: &Value) -> u64 {
    match val {
        Value::Node(n) => n.id.0,
        _ => panic!("expected Value::Node, got {val:?}"),
    }
}

/// Extract a property from a `Value::Node` for assertion convenience.
fn node_prop<'a>(val: &'a Value, key: &str) -> &'a Value {
    match val {
        Value::Node(n) => n
            .properties
            .get(key)
            .unwrap_or_else(|| panic!("node missing property '{key}': {val:?}")),
        _ => panic!("expected Value::Node, got {val:?}"),
    }
}

/// Extract the label of a `Value::Node`.
fn node_label(val: &Value) -> &str {
    match val {
        Value::Node(n) => n.labels.first().map(|s| s.as_str()).unwrap_or(""),
        _ => panic!("expected Value::Node, got {val:?}"),
    }
}

/// Helper: set up a small social graph for testing.
fn setup_social_graph() -> Database {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.query("CREATE (c:Person {name: 'Charlie', age: 35})")
            .unwrap();
        tx.query("CREATE (d:Company {name: 'Acme'})").unwrap();
        tx.commit().unwrap();
    }
    {
        // Create edges via the typed API since CREATE with edges needs existing nodes.
        let tx = db.begin_write().unwrap();
        tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
            .unwrap();
        tx.create_edge(NodeId(2), NodeId(3), "KNOWS", HashMap::new())
            .unwrap();
        tx.create_edge(NodeId(1), NodeId(4), "WORKS_AT", HashMap::new())
            .unwrap();
        tx.commit().unwrap();
    }
    db
}

#[test]
fn e2e_match_all_by_label() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(results.len(), 3);
    tx.commit().unwrap();
}

#[test]
fn e2e_match_with_where_filter() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WHERE n.age > 28 RETURN n.name")
        .unwrap();
    // Alice (30) and Charlie (35) match.
    assert_eq!(results.len(), 2);
    tx.commit().unwrap();
}

#[test]
fn e2e_match_with_string_filter() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WHERE n.name = 'Bob' RETURN n.name")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("n.name"), Some(&Value::String("Bob".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_match_with_relationship() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name")
        .unwrap();
    // Alice->Bob, Bob->Charlie
    assert_eq!(results.len(), 2);
    tx.commit().unwrap();
}

#[test]
fn e2e_match_variable_length_path() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // Alice knows Bob (1 hop), and Bob knows Charlie (2 hops from Alice).
    let results = tx
        .query("MATCH (a:Person {name: 'Alice'})-[:KNOWS*1..2]->(b) RETURN b.name")
        .unwrap();
    assert_eq!(results.len(), 2); // Bob + Charlie
    tx.commit().unwrap();
}

#[test]
fn e2e_count_aggregate() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx.query("MATCH (n:Person) RETURN count(*) AS cnt").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("cnt"), Some(&Value::I64(3)));
    tx.commit().unwrap();
}

#[test]
fn e2e_order_by_and_limit() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) RETURN n.name ORDER BY n.name LIMIT 2")
        .unwrap();
    assert_eq!(results.len(), 2);
    // Alphabetical: Alice, Bob
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[1].get("n.name"), Some(&Value::String("Bob".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_order_by_desc() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) RETURN n.age ORDER BY n.age DESC LIMIT 1")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(35)));
    tx.commit().unwrap();
}

#[test]
fn e2e_create_node() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Person {name: 'Dave', age: 40})")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (n:Person) WHERE n.name = 'Dave' RETURN n.age")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].get("n.age"), Some(&Value::I64(40)));
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_create_edge() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("a.name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(results[0].get("b.name"), Some(&Value::String("Bob".into())));
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_delete_node() {
    let mut db = setup_social_graph();
    {
        let tx = db.begin_write().unwrap();
        tx.query("MATCH (n:Person) WHERE n.name = 'Charlie' DETACH DELETE n")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(results.len(), 2); // Alice and Bob remain
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_set_property() {
    let mut db = setup_social_graph();
    {
        let tx = db.begin_write().unwrap();
        tx.query("MATCH (n:Person) WHERE n.name = 'Alice' SET n.age = 31")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n.age")
            .unwrap();
        assert_eq!(results[0].get("n.age"), Some(&Value::I64(31)));
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_merge_creates_when_not_found() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.source = 'created'")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n.source")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("n.source"),
            Some(&Value::String("created".into()))
        );
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_merge_matches_when_found() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'}) ON MATCH SET n.seen = true")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n.seen")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].get("n.seen"), Some(&Value::Bool(true)));
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_merge_idempotent() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'})").unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'})").unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx.query("MATCH (n:Person) RETURN count(*) AS cnt").unwrap();
        assert_eq!(results[0].get("cnt"), Some(&Value::I64(1))); // only 1 Alice
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_return_star() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WHERE n.name = 'Alice' RETURN *")
        .unwrap();
    assert_eq!(results.len(), 1);
    // `RETURN *` yields one compound column per bound variable.
    let n = results[0].get("n").expect("expected bound variable 'n'");
    assert_eq!(node_label(n), "Person");
    assert_eq!(node_prop(n, "name"), &Value::String("Alice".into()));
    assert_eq!(node_prop(n, "age"), &Value::I64(30));
    // Flat variants must NOT appear at the output layer.
    assert!(results[0].get("n.name").is_none());
    assert!(results[0].get("n.__id").is_none());
    assert!(results[0].get("n.__label").is_none());
    tx.commit().unwrap();
}

#[test]
fn e2e_return_bare_variable() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WHERE n.name = 'Bob' RETURN n")
        .unwrap();
    assert_eq!(results.len(), 1);
    // `RETURN n` yields a single compound Value::Node column.
    let n = results[0].get("n").expect("expected column 'n'");
    assert_eq!(node_label(n), "Person");
    assert_eq!(node_prop(n, "name"), &Value::String("Bob".into()));
    assert_eq!(node_prop(n, "age"), &Value::I64(25));
    // Flat aliases must NOT appear at the output layer.
    assert!(results[0].get("n.name").is_none());
    assert!(results[0].get("n.__id").is_none());
    assert!(results[0].get("n.__label").is_none());
    tx.commit().unwrap();
}

/// Phase 1 acceptance: `RETURN n` produces a single compound `Value::Node`
/// column carrying the node's id, label, and full property map.
#[test]
fn e2e_return_node_is_compound_value() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n")
        .unwrap();
    assert_eq!(results.len(), 1);
    let n = results[0].get("n").expect("expected column 'n'");
    match n {
        Value::Node(node) => {
            assert_eq!(node.labels, vec!["Person".to_string()]);
            assert_eq!(
                node.properties.get("name"),
                Some(&Value::String("Alice".into()))
            );
            assert_eq!(node.properties.get("age"), Some(&Value::I64(30)));
            assert!(node.id.0 > 0);
        }
        other => panic!("expected Value::Node, got {other:?}"),
    }
    tx.commit().unwrap();
}

#[test]
fn e2e_no_internal_fields_in_property_return() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n.name, n.age")
        .unwrap();
    assert_eq!(results.len(), 1);
    // Only requested fields should be present.
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(30)));
    assert!(results[0].get("n.__id").is_none());
    assert!(results[0].get("n.__label").is_none());
    assert!(results[0].get("n").is_none());
    tx.commit().unwrap();
}

#[test]
fn e2e_return_star_with_relationship() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person) RETURN *")
        .unwrap();
    assert_eq!(results.len(), 1); // Alice->Bob
                                  // Both variables should appear as compound columns.
    let a = results[0].get("a").expect("expected column 'a'");
    let b = results[0].get("b").expect("expected column 'b'");
    assert_eq!(node_prop(a, "name"), &Value::String("Alice".into()));
    assert_eq!(node_prop(b, "name"), &Value::String("Bob".into()));
    // No flat aliases at output layer.
    assert!(results[0].get("a.name").is_none());
    assert!(results[0].get("a.__id").is_none());
    assert!(results[0].get("b.__label").is_none());
    tx.commit().unwrap();
}

#[test]
fn e2e_match_create_edge_between_existing_nodes() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        tx.query(
            "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("a.name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(results[0].get("b.name"), Some(&Value::String("Bob".into())));
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_match_create_bidirectional_edges() {
    let mut db = setup_social_graph();
    // setup_social_graph creates Alice->Bob and Bob->Charlie via KNOWS.
    // Make the reverse edges too.
    {
        let tx = db.begin_write().unwrap();
        tx.query("MATCH (a:Person)-[:KNOWS]->(b:Person) CREATE (b)-[:KNOWS]->(a)")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name")
            .unwrap();
        // Original: Alice->Bob, Bob->Charlie. New: Bob->Alice, Charlie->Bob.
        assert_eq!(results.len(), 4);
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_match_create_with_new_node() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        tx.query(
            "MATCH (a:Person {name: 'Alice'}) CREATE (a)-[:WORKS_AT]->(c:Company {name: 'Acme'})",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (p:Person)-[:WORKS_AT]->(c:Company) RETURN p.name, c.name")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("p.name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(
            results[0].get("c.name"),
            Some(&Value::String("Acme".into()))
        );
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_is_null() {
    let mut db = setup_social_graph();
    // Company node has no 'age' property → age IS NULL.
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n) WHERE n.age IS NULL RETURN n.name")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Acme".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_collect_aggregate() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) RETURN collect(n.name) AS names")
        .unwrap();
    assert_eq!(results.len(), 1);
    match results[0].get("names") {
        Some(Value::List(items)) => {
            assert_eq!(items.len(), 3);
            // All three names should be present (order not guaranteed).
            let mut names: Vec<String> = items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    _ => panic!("expected string"),
                })
                .collect();
            names.sort();
            assert_eq!(names, vec!["Alice", "Bob", "Charlie"]);
        }
        other => panic!("expected Value::List, got {other:?}"),
    }
    tx.commit().unwrap();
}

#[test]
fn e2e_grouped_count_aggregate() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', dept: 'eng'})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', dept: 'eng'})")
            .unwrap();
        tx.query("CREATE (c:Person {name: 'Charlie', dept: 'sales'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) RETURN n.dept, count(*) AS cnt ORDER BY n.dept")
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get("n.dept"), Some(&Value::String("eng".into())));
    assert_eq!(results[0].get("cnt"), Some(&Value::I64(2)));
    assert_eq!(
        results[1].get("n.dept"),
        Some(&Value::String("sales".into()))
    );
    assert_eq!(results[1].get("cnt"), Some(&Value::I64(1)));
    tx.commit().unwrap();
}

#[test]
fn e2e_grouped_collect_aggregate() {
    let mut db = setup_social_graph();
    // Alice->Bob via KNOWS, Bob->Charlie via KNOWS
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, collect(b.name) AS friends ORDER BY a.name")
        .unwrap();
    assert_eq!(results.len(), 2);
    // Alice knows Bob.
    assert_eq!(
        results[0].get("a.name"),
        Some(&Value::String("Alice".into()))
    );
    match results[0].get("friends") {
        Some(Value::List(items)) => {
            assert_eq!(items, &vec![Value::String("Bob".into())]);
        }
        other => panic!("expected Value::List, got {other:?}"),
    }
    // Bob knows Charlie.
    assert_eq!(results[1].get("a.name"), Some(&Value::String("Bob".into())));
    match results[1].get("friends") {
        Some(Value::List(items)) => {
            assert_eq!(items, &vec![Value::String("Charlie".into())]);
        }
        other => panic!("expected Value::List, got {other:?}"),
    }
    tx.commit().unwrap();
}

#[test]
fn e2e_is_not_null() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n) WHERE n.age IS NOT NULL RETURN n.name ORDER BY n.name")
        .unwrap();
    // Alice, Bob, Charlie have age; Acme does not.
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_optional_match_with_results() {
    let mut db = setup_social_graph();
    // Alice WORKS_AT Acme. Query OPTIONAL MATCH for WORKS_AT.
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:Person) OPTIONAL MATCH (a)-[:WORKS_AT]->(c:Company) RETURN a.name, c.name ORDER BY a.name")
        .unwrap();
    // Alice has WORKS_AT → Acme; Bob and Charlie don't → null.
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0].get("a.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(
        results[0].get("c.name"),
        Some(&Value::String("Acme".into()))
    );
    assert_eq!(results[1].get("a.name"), Some(&Value::String("Bob".into())));
    assert_eq!(results[1].get("c.name"), Some(&Value::Null));
    assert_eq!(
        results[2].get("a.name"),
        Some(&Value::String("Charlie".into()))
    );
    assert_eq!(results[2].get("c.name"), Some(&Value::Null));
    tx.commit().unwrap();
}

#[test]
fn e2e_optional_match_all_matched() {
    let mut db = setup_social_graph();
    // Alice->Bob via KNOWS.
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:Person {name: 'Alice'}) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN a.name, b.name")
        .unwrap();
    // Alice knows Bob, so should get 1 row with both filled.
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("a.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[0].get("b.name"), Some(&Value::String("Bob".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_optional_match_no_matches() {
    let mut db = setup_social_graph();
    // Charlie has no outgoing WORKS_AT edges.
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:Person {name: 'Charlie'}) OPTIONAL MATCH (a)-[:WORKS_AT]->(c:Company) RETURN a.name, c.name")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("a.name"),
        Some(&Value::String("Charlie".into()))
    );
    assert_eq!(results[0].get("c.name"), Some(&Value::Null));
    tx.commit().unwrap();
}

// --- Index-aware query planning tests ---

#[test]
fn e2e_index_lookup_single_property() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.query("CREATE (c:Person {name: 'Charlie', age: 35})")
            .unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    // This should use IndexLookup instead of Scan+Filter.
    let results = tx
        .query("MATCH (n:Person {name: 'Alice'}) RETURN n.name, n.age")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(30)));
    tx.commit().unwrap();
}

#[test]
fn e2e_index_lookup_no_match() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Nobody'}) RETURN n.name")
        .unwrap();
    assert!(results.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_index_lookup_with_remaining_filter() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Alice', age: 25})")
            .unwrap();
        tx.query("CREATE (c:Person {name: 'Bob', age: 30})")
            .unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    // Index narrows to the two Alices, remaining filter picks age=30.
    let results = tx
        .query("MATCH (n:Person {name: 'Alice', age: 30}) RETURN n.name, n.age")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(30)));
    tx.commit().unwrap();
}

#[test]
fn e2e_index_lookup_in_relationship_pattern() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
        tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
            .unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    // Start node uses index lookup, then expand.
    let results = tx
        .query("MATCH (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person) RETURN a.name, b.name")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("a.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[0].get("b.name"), Some(&Value::String("Bob".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_no_index_falls_back_to_scan() {
    // Same query without an index — should still work via Scan+Filter.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Alice'}) RETURN n.name, n.age")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(30)));
    tx.commit().unwrap();
}

// --- WITH clause tests ---

#[test]
fn e2e_with_simple_projection() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WITH n.name AS name RETURN name ORDER BY name")
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].get("name"), Some(&Value::String("Alice".into())));
    assert_eq!(results[1].get("name"), Some(&Value::String("Bob".into())));
    assert_eq!(
        results[2].get("name"),
        Some(&Value::String("Charlie".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_with_where_filter() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WITH n.name AS name, n.age AS age WHERE age > 25 RETURN name ORDER BY name")
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get("name"), Some(&Value::String("Alice".into())));
    assert_eq!(
        results[1].get("name"),
        Some(&Value::String("Charlie".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_with_aggregation() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', dept: 'eng'})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', dept: 'eng'})")
            .unwrap();
        tx.query("CREATE (c:Person {name: 'Charlie', dept: 'sales'})")
            .unwrap();
        tx.query("CREATE (d:Person {name: 'Diana', dept: 'eng'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    // Aggregate in WITH, then filter on the aggregate result.
    let results = tx
        .query("MATCH (n:Person) WITH n.dept AS dept, count(*) AS cnt WHERE cnt > 1 RETURN dept, cnt ORDER BY dept")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("dept"), Some(&Value::String("eng".into())));
    assert_eq!(results[0].get("cnt"), Some(&Value::I64(3)));
    tx.commit().unwrap();
}

#[test]
fn e2e_with_passthrough_variable() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WITH n RETURN n.name ORDER BY n.name")
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[1].get("n.name"), Some(&Value::String("Bob".into())));
    assert_eq!(
        results[2].get("n.name"),
        Some(&Value::String("Charlie".into()))
    );
    tx.commit().unwrap();
}

/// Chained WITH: arithmetic on prior aggregate result.
#[test]
fn e2e_chained_with_arithmetic_on_aggregate() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (:Person {name: 'Alice', dept: 'eng'})")
        .unwrap();
    tx.query("CREATE (:Person {name: 'Bob', dept: 'eng'})")
        .unwrap();
    tx.query("CREATE (:Person {name: 'Charlie', dept: 'eng'})")
        .unwrap();
    tx.query("CREATE (:Person {name: 'Diana', dept: 'sales'})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (n:Person) \
             WITH n.dept AS dept, count(*) AS c \
             WITH dept, c, c * 2 AS doubled \
             WHERE doubled > 2 \
             RETURN dept, doubled ORDER BY dept",
        )
        .unwrap();
    // eng: count=3, doubled=6 > 2 ✓ ; sales: count=1, doubled=2, NOT > 2 ✗
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("dept"), Some(&Value::String("eng".into())));
    assert_eq!(results[0].get("doubled"), Some(&Value::I64(6)));
    tx.commit().unwrap();
}

/// Map literal in RETURN — basic structured row.
#[test]
fn e2e_map_literal_basic() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (a:Person {name: 'Alice'}) RETURN {name: a.name, age: a.age} AS info")
        .unwrap();
    assert_eq!(rows.len(), 1);
    let info = rows[0].get("info").unwrap();
    match info {
        Value::Map(m) => {
            assert_eq!(m.get("name"), Some(&Value::String("Alice".into())));
            assert_eq!(m.get("age"), Some(&Value::I64(30)));
        }
        other => panic!("expected Map, got {other:?}"),
    }
    tx.commit().unwrap();
}

/// collect() with a map literal — structured grouped results, the main symtext use case.
#[test]
fn e2e_collect_map_literal() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // Alice KNOWS Bob, Bob KNOWS Charlie from setup_social_graph.
    let rows = tx
        .query(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             RETURN a.name, collect({name: b.name, age: b.age}) AS friends \
             ORDER BY a.name",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    // Alice's friends: Bob (one KNOWS edge in setup).
    assert_eq!(
        rows[0].get("a.name").unwrap(),
        &Value::String("Alice".into())
    );
    let alice_friends = rows[0].get("friends").unwrap();
    match alice_friends {
        Value::List(items) => {
            assert_eq!(items.len(), 1);
            match &items[0] {
                Value::Map(m) => {
                    assert_eq!(m.get("name"), Some(&Value::String("Bob".into())));
                    assert_eq!(m.get("age"), Some(&Value::I64(25)));
                }
                other => panic!("expected Map in list, got {other:?}"),
            }
        }
        other => panic!("expected List, got {other:?}"),
    }
    tx.commit().unwrap();
}

/// Nested maps: {outer: {inner: value}}.
#[test]
fn e2e_map_literal_nested() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query(
            "MATCH (a:Person {name: 'Alice'}) \
             RETURN {person: {name: a.name, age: a.age}} AS wrapped",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    let wrapped = rows[0].get("wrapped").unwrap();
    match wrapped {
        Value::Map(outer) => match outer.get("person").unwrap() {
            Value::Map(inner) => {
                assert_eq!(inner.get("name"), Some(&Value::String("Alice".into())));
                assert_eq!(inner.get("age"), Some(&Value::I64(30)));
            }
            other => panic!("expected nested Map, got {other:?}"),
        },
        other => panic!("expected Map, got {other:?}"),
    }
    tx.commit().unwrap();
}

/// Empty map literal.
#[test]
fn e2e_map_literal_empty() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (a:Person {name: 'Alice'}) RETURN {} AS m")
        .unwrap();
    assert_eq!(rows.len(), 1);
    match rows[0].get("m").unwrap() {
        Value::Map(m) => assert!(m.is_empty()),
        other => panic!("expected Map, got {other:?}"),
    }
    tx.commit().unwrap();
}

/// Map equality in WHERE.
#[test]
fn e2e_map_literal_equality() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query(
            "MATCH (a:Person {name: 'Alice'}) \
             WHERE {x: 1, y: 2} = {y: 2, x: 1} \
             RETURN a.name",
        )
        .unwrap();
    // BTreeMap-based equality is key-order-independent.
    assert_eq!(rows.len(), 1);
    tx.commit().unwrap();
}

/// Chained WITH: aggregate on top of aggregate (count the groups).
#[test]
fn e2e_chained_with_aggregate_of_aggregate() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (:Person {dept: 'sales'})").unwrap();
    tx.query("CREATE (:Person {dept: 'ops'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (n:Person) \
             WITH n.dept AS dept, count(*) AS c \
             WITH count(*) AS group_count \
             RETURN group_count",
        )
        .unwrap();
    // 3 distinct departments.
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("group_count"), Some(&Value::I64(3)));
    tx.commit().unwrap();
}

// === CASE expression tests ===

#[test]
fn e2e_case_expression_with_else() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (n:Person) RETURN n.name, CASE WHEN n.age > 30 THEN 'senior' ELSE 'junior' END AS category ORDER BY n.name",
        )
        .unwrap();
    assert_eq!(results.len(), 3);
    // Alice (30) → junior, Bob (25) → junior, Charlie (35) → senior
    assert_eq!(
        results[0].get("category"),
        Some(&Value::String("junior".into()))
    );
    assert_eq!(
        results[1].get("category"),
        Some(&Value::String("junior".into()))
    );
    assert_eq!(
        results[2].get("category"),
        Some(&Value::String("senior".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_case_expression_multiple_when() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (n:Person) RETURN n.name, CASE WHEN n.age < 26 THEN 'young' WHEN n.age < 31 THEN 'mid' ELSE 'senior' END AS tier ORDER BY n.name",
        )
        .unwrap();
    assert_eq!(results.len(), 3);
    // Alice (30) → mid, Bob (25) → young, Charlie (35) → senior
    assert_eq!(results[0].get("tier"), Some(&Value::String("mid".into())));
    assert_eq!(results[1].get("tier"), Some(&Value::String("young".into())));
    assert_eq!(
        results[2].get("tier"),
        Some(&Value::String("senior".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_case_expression_no_else_returns_null() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (n:Person) WHERE n.name = 'Bob' RETURN CASE WHEN n.age > 30 THEN 'senior' END AS category",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    // Bob (25) → no WHEN matches, no ELSE → Null
    assert_eq!(results[0].get("category"), Some(&Value::Null));
    tx.commit().unwrap();
}

// === DETACH DELETE tests ===

#[test]
fn e2e_plain_delete_fails_on_node_with_edges() {
    let mut db = setup_social_graph();
    let tx = db.begin_write().unwrap();
    // Charlie has incoming KNOWS edge from Bob — plain DELETE should fail.
    let result = tx.query("MATCH (n:Person) WHERE n.name = 'Charlie' DELETE n");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("still has relationships"));
    tx.commit().unwrap();
}

#[test]
fn e2e_plain_delete_succeeds_on_isolated_node() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        // Alice has no edges — plain DELETE should work.
        tx.query("MATCH (n:Person) WHERE n.name = 'Alice' DELETE n")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(results.len(), 0);
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_detach_delete_cascades_edges() {
    let mut db = setup_social_graph();
    {
        let tx = db.begin_write().unwrap();
        // Bob has edges (Alice->Bob KNOWS, Bob->Charlie KNOWS) — DETACH DELETE cascades.
        tx.query("MATCH (n:Person) WHERE n.name = 'Bob' DETACH DELETE n")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_read().unwrap();
        let results = tx
            .query("MATCH (n:Person) RETURN n.name ORDER BY n.name")
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].get("n.name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(
            results[1].get("n.name"),
            Some(&Value::String("Charlie".into()))
        );
        // No KNOWS edges should remain.
        let edges = tx.query("MATCH (a)-[:KNOWS]->(b) RETURN a.name").unwrap();
        assert_eq!(edges.len(), 0);
        tx.commit().unwrap();
    }
}

// === Parse error message tests ===

#[test]
fn e2e_parse_error_is_human_readable() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let err = tx.query("GIBBERISH").unwrap_err();
    let msg = err.to_string();
    // Should not contain "serialization error" prefix.
    assert!(!msg.contains("serialization error"), "got: {msg}");
    // Should contain humanized rule name.
    assert!(msg.contains("Cypher statement"), "got: {msg}");
    // Should contain positional info.
    assert!(msg.contains("1:1"), "got: {msg}");
    tx.commit().unwrap();
}

#[test]
fn e2e_parse_error_missing_return_expression() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let err = tx.query("MATCH (n) RETURN").unwrap_err();
    let msg = err.to_string();
    assert!(!msg.contains("serialization error"), "got: {msg}");
    // Should mention expression-related expectation.
    assert!(
        msg.contains("expression")
            || msg.contains("CASE")
            || msg.contains("function")
            || msg.contains("condition"),
        "got: {msg}"
    );
    tx.commit().unwrap();
}

// --- String predicate tests ---

#[test]
fn e2e_starts_with() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Anna'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name STARTS WITH 'A' RETURN n.name ORDER BY n.name")
        .unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|r| r.get("n.name").unwrap().clone())
        .collect();
    assert_eq!(
        names,
        vec![Value::String("Alice".into()), Value::String("Anna".into())]
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_ends_with() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Grace'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name ENDS WITH 'ce' RETURN n.name ORDER BY n.name")
        .unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|r| r.get("n.name").unwrap().clone())
        .collect();
    assert_eq!(
        names,
        vec![Value::String("Alice".into()), Value::String("Grace".into())]
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_ends_with_no_match() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name ENDS WITH 'zzz' RETURN n.name")
        .unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_contains_string() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Lick'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name CONTAINS 'lic' RETURN n.name ORDER BY n.name")
        .unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|r| r.get("n.name").unwrap().clone())
        .collect();
    assert_eq!(names, vec![Value::String("Alice".into())]);
    tx.commit().unwrap();
}

#[test]
fn e2e_ends_with_null_property() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person)").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name ENDS WITH 'ce' RETURN n.name")
        .unwrap();
    // Node without name property should be filtered out (null ENDS WITH x = null = falsy).
    assert_eq!(rows.len(), 1);
    tx.commit().unwrap();
}

// --- Grouped aggregate tests ---

#[test]
fn e2e_grouped_aggregate_many_groups() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    // Create 100 distinct departments with 3 people each.
    for dept in 0..100 {
        for person in 0..3 {
            tx.query(&format!(
                "CREATE (n:Person {{dept: 'dept_{dept}', name: 'p{person}'}})"
            ))
            .unwrap();
        }
    }
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) RETURN n.dept, count(*) AS cnt")
        .unwrap();
    assert_eq!(rows.len(), 100);
    // Every group should have count 3.
    for row in &rows {
        assert_eq!(row.get("cnt").unwrap(), &Value::I64(3));
    }
    tx.commit().unwrap();
}

#[test]
fn e2e_grouped_aggregate_order_by_count() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'sales'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'sales'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'hr'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) RETURN n.dept, count(*) AS cnt ORDER BY cnt DESC")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get("n.dept").unwrap(), &Value::String("eng".into()));
    assert_eq!(rows[0].get("cnt").unwrap(), &Value::I64(3));
    assert_eq!(
        rows[1].get("n.dept").unwrap(),
        &Value::String("sales".into())
    );
    assert_eq!(rows[1].get("cnt").unwrap(), &Value::I64(2));
    assert_eq!(rows[2].get("n.dept").unwrap(), &Value::String("hr".into()));
    assert_eq!(rows[2].get("cnt").unwrap(), &Value::I64(1));
    tx.commit().unwrap();
}

#[test]
fn e2e_multiple_aggregates_in_return() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Person {dept: 'eng', age: 30})")
        .unwrap();
    tx.query("CREATE (n:Person {dept: 'eng', age: 40})")
        .unwrap();
    tx.query("CREATE (n:Person {dept: 'sales', age: 25})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) RETURN n.dept, count(*) AS cnt, sum(n.age) AS total, avg(n.age) AS average ORDER BY n.dept")
        .unwrap();
    assert_eq!(rows.len(), 2);
    // eng: count=2, sum=70, avg=35
    assert_eq!(rows[0].get("cnt").unwrap(), &Value::I64(2));
    assert_eq!(rows[0].get("total").unwrap(), &Value::I64(70));
    assert_eq!(rows[0].get("average").unwrap(), &Value::F64(35.0));
    // sales: count=1, sum=25, avg=25
    assert_eq!(rows[1].get("cnt").unwrap(), &Value::I64(1));
    assert_eq!(rows[1].get("total").unwrap(), &Value::I64(25));
    assert_eq!(rows[1].get("average").unwrap(), &Value::F64(25.0));
    tx.commit().unwrap();
}

// --- Edge case tests ---

#[test]
fn e2e_match_on_empty_database() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_match_no_label_on_empty_database() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx.query("MATCH (n) RETURN n").unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_count_on_empty_database() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx.query("MATCH (n:Person) RETURN count(*) AS cnt").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("cnt").unwrap(), &Value::I64(0));
    tx.commit().unwrap();
}

#[test]
fn e2e_where_on_missing_property() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob', email: 'bob@test.com'})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    // Alice has no email — comparison with missing prop should not match.
    let rows = tx
        .query("MATCH (n:Person) WHERE n.email = 'bob@test.com' RETURN n.name")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n.name").unwrap(), &Value::String("Bob".into()));
    tx.commit().unwrap();
}

#[test]
fn e2e_set_property_to_null_removes_it() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice', age: 30})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    tx.query("MATCH (n:Person) WHERE n.name = 'Alice' SET n.age = null")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n.age")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n.age").unwrap(), &Value::Null);
    tx.commit().unwrap();
}

#[test]
fn e2e_unicode_property_values() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Person {name: '日本語テスト'})")
        .unwrap();
    tx.query("CREATE (n:Person {name: 'émojis 🎉🚀'})").unwrap();
    tx.query("CREATE (n:Person {name: 'Ñoño'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) RETURN n.name ORDER BY n.name")
        .unwrap();
    assert_eq!(rows.len(), 3);

    // Verify we can filter on unicode too.
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name STARTS WITH 'Ñ' RETURN n.name")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("n.name").unwrap(),
        &Value::String("Ñoño".into())
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_large_dataset_smoke_test() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    for i in 0..1000 {
        tx.query(&format!("CREATE (n:Item {{id: {i}}})")).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx.query("MATCH (n:Item) RETURN count(*) AS cnt").unwrap();
    assert_eq!(rows[0].get("cnt").unwrap(), &Value::I64(1000));

    let rows = tx
        .query("MATCH (n:Item) WHERE n.id > 990 RETURN n.id ORDER BY n.id")
        .unwrap();
    assert_eq!(rows.len(), 9); // 991..999
    tx.commit().unwrap();
}

// --- Null handling in aggregation ---

#[test]
fn e2e_aggregate_with_null_group_keys() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person)").unwrap(); // no dept
    tx.query("CREATE (n:Person)").unwrap(); // no dept
    tx.query("CREATE (n:Person)").unwrap(); // no dept
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) RETURN n.dept, count(*) AS cnt ORDER BY cnt DESC")
        .unwrap();
    assert_eq!(rows.len(), 2);
    // Null group should have 3 nodes.
    let null_group = rows
        .iter()
        .find(|r| r.get("n.dept").unwrap() == &Value::Null)
        .unwrap();
    assert_eq!(null_group.get("cnt").unwrap(), &Value::I64(3));
    // eng group should have 2 nodes.
    let eng_group = rows
        .iter()
        .find(|r| r.get("n.dept").unwrap() == &Value::String("eng".into()))
        .unwrap();
    assert_eq!(eng_group.get("cnt").unwrap(), &Value::I64(2));
    tx.commit().unwrap();
}

#[test]
fn e2e_collect_on_empty_result() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Nonexistent) RETURN collect(n.name) AS names")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("names").unwrap(), &Value::List(vec![]));
    tx.commit().unwrap();
}

// --- Query combination tests ---

#[test]
fn e2e_with_where_return_chaining() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    for i in 1..=10 {
        tx.query(&format!("CREATE (n:Num {{val: {i}}})")).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Num) WITH n.val AS v WHERE v > 5 RETURN v ORDER BY v")
        .unwrap();
    assert_eq!(rows.len(), 5); // 6,7,8,9,10
    assert_eq!(rows[0].get("v").unwrap(), &Value::I64(6));
    assert_eq!(rows[4].get("v").unwrap(), &Value::I64(10));
    tx.commit().unwrap();
}

#[test]
fn e2e_optional_match_with_aggregation() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Charlie'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    // Only Alice knows people.
    tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(1), NodeId(3), "KNOWS", HashMap::new())
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name, count(b.name) AS friends ORDER BY a.name")
        .unwrap();
    assert_eq!(rows.len(), 3);
    // Alice knows 2 people.
    assert_eq!(
        rows[0].get("a.name").unwrap(),
        &Value::String("Alice".into())
    );
    assert_eq!(rows[0].get("friends").unwrap(), &Value::I64(2));
    // Bob and Charlie know 0 people.
    assert_eq!(rows[1].get("friends").unwrap(), &Value::I64(0));
    assert_eq!(rows[2].get("friends").unwrap(), &Value::I64(0));
    tx.commit().unwrap();
}

/// Regression: OPTIONAL MATCH where shared variable is the *destination* (not
/// source) of the optional pattern. Previously inflated count() because the
/// correlated expand didn't filter by the already-bound destination.
#[test]
fn e2e_optional_match_shared_destination_count() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    // Three functions; two callers.
    tx.query("CREATE (fn1:Function {name: 'main'})").unwrap();
    tx.query("CREATE (fn2:Function {name: 'helper'})").unwrap();
    tx.query("CREATE (fn3:Function {name: 'unused'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    // main is called by helper and unused; helper is called by main; unused is called by nobody.
    tx.create_edge(NodeId(2), NodeId(1), "CALLS", HashMap::new())
        .unwrap(); // helper -> main
    tx.create_edge(NodeId(3), NodeId(1), "CALLS", HashMap::new())
        .unwrap(); // unused -> main
    tx.create_edge(NodeId(1), NodeId(2), "CALLS", HashMap::new())
        .unwrap(); // main -> helper
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query(
            "MATCH (fn:Function) \
             OPTIONAL MATCH (caller:Function)-[:CALLS]->(fn) \
             RETURN fn.name, count(caller) AS caller_count \
             ORDER BY fn.name",
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    // helper: called by main → 1
    assert_eq!(
        rows[0].get("fn.name").unwrap(),
        &Value::String("helper".into())
    );
    assert_eq!(rows[0].get("caller_count").unwrap(), &Value::I64(1));
    // main: called by helper and unused → 2
    assert_eq!(
        rows[1].get("fn.name").unwrap(),
        &Value::String("main".into())
    );
    assert_eq!(rows[1].get("caller_count").unwrap(), &Value::I64(2));
    // unused: called by nobody → 0
    assert_eq!(
        rows[2].get("fn.name").unwrap(),
        &Value::String("unused".into())
    );
    assert_eq!(rows[2].get("caller_count").unwrap(), &Value::I64(0));
    tx.commit().unwrap();
}

/// Regression: dead-code detection pattern via OPTIONAL MATCH + WITH + WHERE.
#[test]
fn e2e_optional_match_dead_code_detection() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (fn1:Function {name: 'used'})").unwrap();
    tx.query("CREATE (fn2:Function {name: 'dead'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    tx.create_edge(NodeId(1), NodeId(1), "CALLS", HashMap::new())
        .unwrap(); // used calls itself (recursive)
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query(
            "MATCH (fn:Function) \
             OPTIONAL MATCH (caller:Function)-[:CALLS]->(fn) \
             WITH fn.name AS name, count(caller) AS c \
             WHERE c = 0 \
             RETURN name ORDER BY name",
        )
        .unwrap();
    // Only 'dead' has zero callers.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("name").unwrap(), &Value::String("dead".into()));
    tx.commit().unwrap();
}

/// Regression: WITH that projects a node reference should still allow
/// `node.prop` access in downstream clauses. Previously Aggregate threw
/// away the flattened `n.*` keys, leaving `n.name` returning Null.
#[test]
fn e2e_with_node_reference_preserves_properties() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
        .unwrap();
    tx.query("CREATE (b:Person {name: 'Bob', age: 25})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(1), NodeId(1), "KNOWS", HashMap::new())
        .unwrap(); // Alice self-loop so she has 2 KNOWS
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    // WITH a node reference + aggregate, then access .name on the node.
    let rows = tx
        .query(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             WITH a, count(b) AS c \
             RETURN a.name, c ORDER BY a.name",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("a.name").unwrap(),
        &Value::String("Alice".into())
    );
    assert_eq!(rows[0].get("c").unwrap(), &Value::I64(2));
    tx.commit().unwrap();
}

/// Regression: OPTIONAL MATCH with multiple edge types `[r:A|B]` should
/// match edges of any listed type in correlated execution, not just the
/// first type.
#[test]
fn e2e_optional_match_multiple_edge_types() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Charlie'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    // Alice KNOWS Bob; Alice FOLLOWS Charlie. No outgoing edges for Bob/Charlie.
    tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(1), NodeId(3), "FOLLOWS", HashMap::new())
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query(
            "MATCH (a:Person) \
             OPTIONAL MATCH (a)-[:KNOWS|FOLLOWS]->(b) \
             RETURN a.name, count(b) AS c ORDER BY a.name",
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    // Alice: 2 outgoing edges across KNOWS + FOLLOWS.
    assert_eq!(
        rows[0].get("a.name").unwrap(),
        &Value::String("Alice".into())
    );
    assert_eq!(rows[0].get("c").unwrap(), &Value::I64(2));
    // Bob, Charlie: 0 outgoing.
    assert_eq!(rows[1].get("c").unwrap(), &Value::I64(0));
    assert_eq!(rows[2].get("c").unwrap(), &Value::I64(0));
    tx.commit().unwrap();
}

#[test]
fn e2e_variable_length_path_with_where() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // Alice -> Bob -> Charlie via KNOWS, variable-length 1..2 hops.
    let rows = tx
        .query("MATCH (a:Person {name: 'Alice'})-[:KNOWS*1..2]->(b:Person) WHERE b.age > 30 RETURN b.name")
        .unwrap();
    // Only Charlie (age 35) should match via 2 hops.
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("b.name").unwrap(),
        &Value::String("Charlie".into())
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_merge_on_create_and_on_match_set() {
    let mut db = Database::open_memory().unwrap();
    // First MERGE — creates.
    {
        let tx = db.begin_write().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.updated = true")
            .unwrap();
        tx.commit().unwrap();
    }
    // Verify created.
    {
        let tx = db.begin_read().unwrap();
        let rows = tx
            .query("MATCH (n:Person {name: 'Alice'}) RETURN n.created, n.updated")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("n.created").unwrap(), &Value::Bool(true));
        assert_eq!(rows[0].get("n.updated").unwrap(), &Value::Null);
        tx.commit().unwrap();
    }
    // Second MERGE — matches.
    {
        let tx = db.begin_write().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.updated = true")
            .unwrap();
        tx.commit().unwrap();
    }
    // Verify updated.
    {
        let tx = db.begin_read().unwrap();
        let rows = tx
            .query("MATCH (n:Person {name: 'Alice'}) RETURN n.created, n.updated")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("n.created").unwrap(), &Value::Bool(true));
        assert_eq!(rows[0].get("n.updated").unwrap(), &Value::Bool(true));
        tx.commit().unwrap();
    }
}

// --- Error handling tests ---

#[test]
fn e2e_parse_error_has_position() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let err = tx.query("METCH (n) RETURN n").unwrap_err();
    let msg = err.to_string();
    // Should be human-readable, not a raw pest error.
    assert!(!msg.contains("serialization error"), "got: {msg}");
    // Should contain position info.
    assert!(msg.contains("1:"), "expected position info, got: {msg}");
    tx.commit().unwrap();
}

#[test]
fn e2e_unbound_variable_in_return() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    // 'x' is not bound by MATCH — should return null, not crash.
    let rows = tx.query("MATCH (n:Person) RETURN x.name");
    // Either returns nulls or errors — both are acceptable, just no panic.
    assert!(rows.is_ok() || rows.is_err());
    tx.commit().unwrap();
}

#[test]
fn e2e_delete_node_with_edges_fails() {
    let mut db = setup_social_graph();
    // Alice (NodeId 1) has edges — plain DELETE should fail.
    let tx = db.begin_write().unwrap();
    let err = tx
        .query("MATCH (n:Person {name: 'Alice'}) DELETE n")
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("still has relationships"),
        "expected DeleteConnectedNode error, got: {msg}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_detach_delete_node_with_edges_succeeds() {
    let mut db = setup_social_graph();
    let tx = db.begin_write().unwrap();
    tx.query("MATCH (n:Person {name: 'Alice'}) DETACH DELETE n")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person {name: 'Alice'}) RETURN n")
        .unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

// --- Numeric edge cases ---

#[test]
fn e2e_large_integers() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Num {val: 9223372036854775807})")
        .unwrap(); // i64::MAX
    tx.query("CREATE (n:Num {val: -9223372036854775808})")
        .unwrap(); // i64::MIN
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Num) RETURN n.val ORDER BY n.val")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("n.val").unwrap(), &Value::I64(i64::MIN));
    assert_eq!(rows[1].get("n.val").unwrap(), &Value::I64(i64::MAX));
    tx.commit().unwrap();
}

#[test]
fn e2e_integer_overflow_errors() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();

    // Each of these should error rather than panic or wrap silently.
    let cases = [
        "RETURN 9223372036854775807 + 1",
        "RETURN -9223372036854775808 - 1",
        "RETURN 9223372036854775807 * 2",
        "RETURN -9223372036854775808 / -1",
        "RETURN -9223372036854775808 % -1",
        "RETURN abs(-9223372036854775808)",
        "RETURN -(-9223372036854775808)",
    ];
    for q in cases {
        let err = tx
            .query(q)
            .expect_err(&format!("expected overflow for: {q}"));
        let msg = format!("{err}");
        assert!(
            msg.contains("overflow") || msg.contains("out of range"),
            "expected overflow error for {q}, got: {msg}"
        );
    }
    tx.commit().unwrap();
}

#[test]
fn e2e_division_by_zero_returns_null() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx.query("RETURN 1 / 0 AS x, 1 % 0 AS y").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("x").unwrap(), &Value::Null);
    assert_eq!(rows[0].get("y").unwrap(), &Value::Null);
    tx.commit().unwrap();
}

#[test]
fn e2e_null_propagation_arithmetic() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("RETURN null + 1 AS a, 1 - null AS b, null * 2 AS c, null / 2 AS d, null % 2 AS e")
        .unwrap();
    assert_eq!(rows.len(), 1);
    for col in ["a", "b", "c", "d", "e"] {
        assert_eq!(rows[0].get(col).unwrap(), &Value::Null, "col {col}");
    }
    tx.commit().unwrap();
}

#[test]
fn e2e_undefined_variable_carries_source_span() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    // Probe a few query shapes — at least one should reach the planner's
    // scope check and surface a line:col location.
    let queries = [
        "WITH 1 AS x RETURN unknown_var",
        "MATCH (n) WITH n AS y RETURN unknown_var",
        "MATCH (n) RETURN n.x ORDER BY zzz",
        "MATCH (n) WITH n.x AS p RETURN q",
    ];
    let mut saw_span = false;
    for q in queries {
        if let Err(e) = tx.query(q) {
            if format!("{e}").contains("at line ") {
                saw_span = true;
                break;
            }
        }
    }
    assert!(
        saw_span,
        "expected at least one undefined-variable error to carry a line:col span"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_float_property() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Num {val: 3.14159})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx.query("MATCH (n:Num) RETURN n.val").unwrap();
    assert_eq!(rows.len(), 1);
    if let Value::F64(v) = rows[0].get("n.val").unwrap() {
        #[allow(clippy::approx_constant)]
        let expected = 3.14159;
        assert!((v - expected).abs() < 1e-10);
    } else {
        panic!("expected F64");
    }
    tx.commit().unwrap();
}

#[test]
fn e2e_boolean_properties() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Flag {active: true, deleted: false})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Flag) WHERE n.active = true RETURN n.deleted")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n.deleted").unwrap(), &Value::Bool(false));
    tx.commit().unwrap();
}

// --- UNWIND tests ---

#[test]
fn e2e_unwind_list_literal() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx.query("UNWIND [1, 2, 3] AS x RETURN x").unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get("x").unwrap(), &Value::I64(1));
    assert_eq!(rows[1].get("x").unwrap(), &Value::I64(2));
    assert_eq!(rows[2].get("x").unwrap(), &Value::I64(3));
    tx.commit().unwrap();
}

#[test]
fn e2e_unwind_empty_list() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx.query("UNWIND [] AS x RETURN x").unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_unwind_string_list() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("UNWIND ['Alice', 'Bob', 'Charlie'] AS name RETURN name ORDER BY name")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get("name").unwrap(), &Value::String("Alice".into()));
    assert_eq!(rows[1].get("name").unwrap(), &Value::String("Bob".into()));
    assert_eq!(
        rows[2].get("name").unwrap(),
        &Value::String("Charlie".into())
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_unwind_with_where_filter() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("UNWIND [1, 2, 3, 4, 5] AS x WHERE x > 3 RETURN x")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("x").unwrap(), &Value::I64(4));
    assert_eq!(rows[1].get("x").unwrap(), &Value::I64(5));
    tx.commit().unwrap();
}

#[test]
fn e2e_unwind_create_nodes() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("UNWIND ['Alice', 'Bob', 'Charlie'] AS name CREATE (n:Person {name: name})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) RETURN n.name ORDER BY n.name")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].get("n.name").unwrap(),
        &Value::String("Alice".into())
    );
    assert_eq!(rows[1].get("n.name").unwrap(), &Value::String("Bob".into()));
    assert_eq!(
        rows[2].get("n.name").unwrap(),
        &Value::String("Charlie".into())
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_unwind_with_aggregation() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("UNWIND [1, 2, 3, 4, 5] AS x RETURN sum(x) AS total, count(*) AS cnt")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("total").unwrap(), &Value::I64(15));
    assert_eq!(rows[0].get("cnt").unwrap(), &Value::I64(5));
    tx.commit().unwrap();
}

#[test]
fn e2e_match_with_unwind() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice', tags: 'dev,lead'})")
        .unwrap();
    tx.commit().unwrap();

    // Use UNWIND within a MATCH via collect + UNWIND in WITH chain.
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) UNWIND [1, 2] AS x RETURN n.name, x ORDER BY x")
        .unwrap();
    // Each person x each unwind element = 1 * 2 = 2 rows.
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].get("n.name").unwrap(),
        &Value::String("Alice".into())
    );
    assert_eq!(rows[0].get("x").unwrap(), &Value::I64(1));
    assert_eq!(rows[1].get("x").unwrap(), &Value::I64(2));
    tx.commit().unwrap();
}

#[test]
fn e2e_list_literal_in_return() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice'})").unwrap();
    tx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("MATCH (n:Person) RETURN [1, 2, 3] AS nums")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("nums").unwrap(),
        &Value::List(vec![Value::I64(1), Value::I64(2), Value::I64(3)])
    );
    tx.commit().unwrap();
}

// === EXISTS subquery tests ===

#[test]
fn e2e_exists_simple_pattern() {
    // Alice KNOWS Bob, Bob KNOWS Charlie, Charlie knows nobody.
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WHERE EXISTS { (n)-[:KNOWS]->(:Person) } RETURN n.name ORDER BY n.name")
        .unwrap();
    // Alice -> Bob, Bob -> Charlie. Charlie has no outgoing KNOWS.
    let names: Vec<&str> = results
        .iter()
        .map(|r| match r.get("n.name").unwrap() {
            Value::String(s) => s.as_str(),
            _ => panic!(),
        })
        .collect();
    assert_eq!(names, vec!["Alice", "Bob"]);
    tx.commit().unwrap();
}

#[test]
fn e2e_exists_with_where_filter() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // Find people who know someone older than 30.
    let results = tx
        .query(
            "MATCH (n:Person) WHERE EXISTS { (n)-[:KNOWS]->(m:Person) WHERE m.age > 30 } RETURN n.name",
        )
        .unwrap();
    // Bob knows Charlie (age 35). Alice knows Bob (age 25) — doesn't match.
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name").unwrap(),
        &Value::String("Bob".to_string())
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_not_exists() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // Find people who do NOT know anyone.
    let results = tx
        .query("MATCH (n:Person) WHERE NOT EXISTS { (n)-[:KNOWS]->(:Person) } RETURN n.name")
        .unwrap();
    // Only Charlie has no outgoing KNOWS edges.
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name").unwrap(),
        &Value::String("Charlie".to_string())
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_exists_correlated_variable() {
    // Tests that the EXISTS subquery correctly correlates with the outer MATCH.
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // Find people who work at any company.
    let results = tx
        .query("MATCH (n:Person) WHERE EXISTS { (n)-[:WORKS_AT]->(:Company) } RETURN n.name")
        .unwrap();
    // Only Alice WORKS_AT Acme.
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name").unwrap(),
        &Value::String("Alice".to_string())
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_exists_with_property_match() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // Find people who know someone named 'Bob' (using WHERE in the subquery).
    let results = tx
        .query(
            "MATCH (n:Person) WHERE EXISTS { (n)-[:KNOWS]->(m:Person) WHERE m.name = 'Bob' } RETURN n.name",
        )
        .unwrap();
    // Only Alice knows Bob.
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name").unwrap(),
        &Value::String("Alice".to_string())
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_exists_combined_with_and() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // People older than 28 who also know someone.
    let results = tx
        .query(
            "MATCH (n:Person) WHERE n.age > 28 AND EXISTS { (n)-[:KNOWS]->(:Person) } RETURN n.name",
        )
        .unwrap();
    // Alice (30, knows Bob). Charlie (35, knows nobody) — fails EXISTS.
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name").unwrap(),
        &Value::String("Alice".to_string())
    );
    tx.commit().unwrap();
}

// === List comprehension tests ===

#[test]
fn e2e_list_comprehension_identity() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:X {name: 'a'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:X) RETURN [x IN [1, 2, 3] | x] AS nums")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("nums").unwrap(),
        &Value::List(vec![Value::I64(1), Value::I64(2), Value::I64(3)])
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_list_comprehension_with_filter() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:X {name: 'a'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:X) RETURN [x IN [1, 2, 3, 4, 5] WHERE x > 3] AS big")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("big").unwrap(),
        &Value::List(vec![Value::I64(4), Value::I64(5)])
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_list_comprehension_empty_input() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:X {name: 'a'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:X) RETURN [x IN [] | x] AS empty")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("empty").unwrap(), &Value::List(vec![]));
    tx.commit().unwrap();
}

#[test]
fn e2e_list_comprehension_filter_all() {
    // Filter removes all elements.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:X {name: 'a'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:X) RETURN [x IN [1, 2, 3] WHERE x > 100] AS empty_list")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("empty_list").unwrap(), &Value::List(vec![]));
    tx.commit().unwrap();
}

#[test]
fn e2e_list_comprehension_with_unwind_source() {
    // Use UNWIND to create a list, then comprehension to filter it.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (n:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.query("CREATE (n:Person {name: 'Charlie', age: 35})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    // Collect ages into a list, then filter with comprehension.
    let results = tx
        .query(
            "MATCH (n:Person) WITH collect(n.age) AS ages RETURN [a IN ages WHERE a > 28] AS old_ages",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    let old_ages = results[0].get("old_ages").unwrap();
    if let Value::List(items) = old_ages {
        assert_eq!(items.len(), 2); // 30 and 35
        assert!(items.contains(&Value::I64(30)));
        assert!(items.contains(&Value::I64(35)));
    } else {
        panic!("expected list, got {old_ages:?}");
    }
    tx.commit().unwrap();
}

// === shortestPath / allShortestPaths tests ===

/// Helper: build a graph with multiple paths for shortest path testing.
/// Graph: A -KNOWS-> B -KNOWS-> C -KNOWS-> D
///        A -KNOWS-> C (shortcut)
///        A -KNOWS-> D (direct)
fn setup_path_graph() -> Database {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'A'})").unwrap(); // NodeId(1)
        tx.query("CREATE (b:Person {name: 'B'})").unwrap(); // NodeId(2)
        tx.query("CREATE (c:Person {name: 'C'})").unwrap(); // NodeId(3)
        tx.query("CREATE (d:Person {name: 'D'})").unwrap(); // NodeId(4)
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        // Chain: A -> B -> C -> D
        tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
            .unwrap();
        tx.create_edge(NodeId(2), NodeId(3), "KNOWS", HashMap::new())
            .unwrap();
        tx.create_edge(NodeId(3), NodeId(4), "KNOWS", HashMap::new())
            .unwrap();
        // Shortcuts: A -> C, A -> D
        tx.create_edge(NodeId(1), NodeId(3), "KNOWS", HashMap::new())
            .unwrap();
        tx.create_edge(NodeId(1), NodeId(4), "KNOWS", HashMap::new())
            .unwrap();
        tx.commit().unwrap();
    }
    db
}

#[test]
fn e2e_shortest_path_direct() {
    // A -> D exists directly (1 hop). Also A->B->C->D (3 hops).
    let mut db = setup_path_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (a:Person {name: 'A'}), (d:Person {name: 'D'}), \
             p = shortestPath((a)-[:KNOWS*1..10]->(d)) \
             RETURN p",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    let path = results[0].get("p").unwrap();
    // Direct path: A(1) -> D(4)
    assert_eq!(path_ids(path), vec![1, 4]);
    tx.commit().unwrap();
}

#[test]
fn e2e_shortest_path_multi_hop() {
    // A -> B is 1 hop (direct).
    let mut db = setup_path_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (a:Person {name: 'A'}), (b:Person {name: 'B'}), \
             p = shortestPath((a)-[:KNOWS*1..10]->(b)) \
             RETURN p, length(p) AS len",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(path_ids(results[0].get("p").unwrap()), vec![1, 2]);
    assert_eq!(results[0].get("len").unwrap(), &Value::I64(1));
    tx.commit().unwrap();
}

#[test]
fn e2e_shortest_path_no_path() {
    // B -> A has no path (edges are directed A->B only).
    let mut db = setup_path_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (b:Person {name: 'B'}), (a:Person {name: 'A'}), \
             p = shortestPath((b)-[:KNOWS*1..10]->(a)) \
             RETURN p",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("p").unwrap(), &Value::Null);
    tx.commit().unwrap();
}

#[test]
fn e2e_shortest_path_length_function() {
    // A -> C: direct (1 hop) and A->B->C (2 hops). Shortest is 1.
    let mut db = setup_path_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (a:Person {name: 'A'}), (c:Person {name: 'C'}), \
             p = shortestPath((a)-[:KNOWS*1..10]->(c)) \
             RETURN length(p) AS len, nodes(p) AS node_ids",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("len").unwrap(), &Value::I64(1));
    // nodes(p) returns a list of full node values; verify their IDs.
    let node_ids_val = results[0].get("node_ids").unwrap();
    let ids: Vec<u64> = match node_ids_val {
        Value::List(items) => items.iter().map(node_id).collect(),
        _ => panic!("expected list, got {node_ids_val:?}"),
    };
    assert_eq!(ids, vec![1, 3]);
    tx.commit().unwrap();
}

#[test]
fn e2e_all_shortest_paths() {
    // A -> C: two paths of length 1 (A->C direct). Wait — there's only one
    // direct edge. Let me think about the graph.
    // A->C (1 hop): only one path of length 1.
    // So allShortestPaths should return just that one.
    let mut db = setup_path_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (a:Person {name: 'A'}), (c:Person {name: 'C'}), \
             p = allShortestPaths((a)-[:KNOWS*1..10]->(c)) \
             RETURN p",
        )
        .unwrap();
    // Only 1 shortest path: A->C (length 1).
    assert_eq!(results.len(), 1);
    assert_eq!(path_ids(results[0].get("p").unwrap()), vec![1, 3]);
    tx.commit().unwrap();
}

#[test]
fn e2e_all_shortest_paths_multiple() {
    // Build a diamond graph: A->B->D, A->C->D (both length 2).
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:N {name: 'A'})").unwrap(); // 1
        tx.query("CREATE (b:N {name: 'B'})").unwrap(); // 2
        tx.query("CREATE (c:N {name: 'C'})").unwrap(); // 3
        tx.query("CREATE (d:N {name: 'D'})").unwrap(); // 4
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        tx.create_edge(NodeId(1), NodeId(2), "E", HashMap::new())
            .unwrap();
        tx.create_edge(NodeId(1), NodeId(3), "E", HashMap::new())
            .unwrap();
        tx.create_edge(NodeId(2), NodeId(4), "E", HashMap::new())
            .unwrap();
        tx.create_edge(NodeId(3), NodeId(4), "E", HashMap::new())
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (a:N {name: 'A'}), (d:N {name: 'D'}), \
             p = allShortestPaths((a)-[:E*1..10]->(d)) \
             RETURN p",
        )
        .unwrap();
    // Two shortest paths of length 2: A->B->D and A->C->D.
    assert_eq!(results.len(), 2);
    let mut id_paths: Vec<Vec<u64>> = results
        .iter()
        .map(|r| path_ids(r.get("p").unwrap()))
        .collect();
    id_paths.sort();
    assert_eq!(id_paths[0], vec![1, 2, 4]);
    assert_eq!(id_paths[1], vec![1, 3, 4]);
    tx.commit().unwrap();
}

#[test]
fn e2e_shortest_path_respects_direction() {
    // A -> B exists, but B -> A does not.
    let mut db = setup_path_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (b:Person {name: 'B'}), (a:Person {name: 'A'}), \
             p = shortestPath((b)-[:KNOWS*1..10]->(a)) \
             RETURN p",
        )
        .unwrap();
    // No outgoing path from B to A.
    assert_eq!(results[0].get("p").unwrap(), &Value::Null);
    tx.commit().unwrap();
}

#[test]
fn e2e_shortest_path_same_node() {
    // Path from A to A should be a single-node path.
    let mut db = setup_path_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (a:Person {name: 'A'}), (a2:Person {name: 'A'}), \
             p = shortestPath((a)-[:KNOWS*1..10]->(a2)) \
             RETURN p, length(p) AS len",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(path_ids(results[0].get("p").unwrap()), vec![1]);
    assert_eq!(results[0].get("len").unwrap(), &Value::I64(0));
    tx.commit().unwrap();
}

// === Cost optimizer / EXPLAIN tests ===

#[test]
fn e2e_explain_returns_plan_not_data() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("EXPLAIN MATCH (n:Person) WHERE n.age > 30 RETURN n.name")
        .unwrap();
    // EXPLAIN returns a single record with a "plan" column.
    assert_eq!(results.len(), 1);
    let plan = results[0].get("plan").unwrap();
    if let Value::String(text) = plan {
        assert!(
            text.contains("Scan :Person"),
            "plan should show Scan: {text}"
        );
        assert!(text.contains("Filter"), "plan should show Filter: {text}");
        assert!(text.contains("Project"), "plan should show Project: {text}");
        assert!(text.contains("est."), "plan should show estimates: {text}");
    } else {
        panic!("expected string plan output, got {plan:?}");
    }
    tx.commit().unwrap();
}

#[test]
fn e2e_explain_shows_estimated_rows() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        // Create 10 Person nodes — stats should track this.
        for i in 0..10 {
            tx.query(&format!("CREATE (n:Person {{name: 'P{i}'}})"))
                .unwrap();
        }
        // Create 3 Company nodes.
        for i in 0..3 {
            tx.query(&format!("CREATE (n:Company {{name: 'C{i}'}})"))
                .unwrap();
        }
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx.query("EXPLAIN MATCH (n:Person) RETURN n.name").unwrap();
    let plan = match results[0].get("plan").unwrap() {
        Value::String(s) => s.clone(),
        other => panic!("expected string, got {other:?}"),
    };
    // Should show est. 10 rows for Person scan (from live stats).
    assert!(
        plan.contains("est. 10 rows"),
        "expected 10 rows estimate: {plan}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_stats_maintained_on_create_delete() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Animal {name: 'Dog'})").unwrap();
        tx.query("CREATE (b:Animal {name: 'Cat'})").unwrap();
        tx.query("CREATE (c:Animal {name: 'Bird'})").unwrap();
        tx.commit().unwrap();
    }
    // Check: EXPLAIN should show est. 3 rows for Animal.
    {
        let tx = db.begin_read().unwrap();
        let results = tx.query("EXPLAIN MATCH (n:Animal) RETURN n").unwrap();
        let plan = match results[0].get("plan").unwrap() {
            Value::String(s) => s.clone(),
            other => panic!("expected string, got {other:?}"),
        };
        assert!(
            plan.contains("est. 3 rows"),
            "expected 3 after creates: {plan}"
        );
        tx.commit().unwrap();
    }
    // Delete one Animal.
    {
        let tx = db.begin_write().unwrap();
        tx.query("MATCH (n:Animal {name: 'Bird'}) DELETE n")
            .unwrap();
        tx.commit().unwrap();
    }
    // Check: should now show est. 2 rows.
    {
        let tx = db.begin_read().unwrap();
        let results = tx.query("EXPLAIN MATCH (n:Animal) RETURN n").unwrap();
        let plan = match results[0].get("plan").unwrap() {
            Value::String(s) => s.clone(),
            other => panic!("expected string, got {other:?}"),
        };
        assert!(
            plan.contains("est. 2 rows"),
            "expected 2 after delete: {plan}"
        );
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_explain_cross_product() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("EXPLAIN MATCH (a:Person), (b:Company) RETURN a.name, b.name")
        .unwrap();
    let plan = match results[0].get("plan").unwrap() {
        Value::String(s) => s.clone(),
        other => panic!("expected string, got {other:?}"),
    };
    assert!(
        plan.contains("CrossProduct"),
        "should show CrossProduct: {plan}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_length_function_on_string() {
    // length() also works on strings and lists.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:X {name: 'hello'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:X) RETURN length(n.name) AS len")
        .unwrap();
    assert_eq!(results[0].get("len").unwrap(), &Value::I64(5));
    tx.commit().unwrap();
}

// ── Issue 1: Relationship properties in CREATE ────────────────────────

#[test]
fn e2e_create_edge_with_properties() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:File {name: 'main.py'})-[:IMPORTS {line_number: 1, alias: 'os'}]->(b:Module {name: 'os'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:File)-[r:IMPORTS]->(b:Module) RETURN r.line_number, r.alias")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("r.line_number").unwrap(), &Value::I64(1));
    assert_eq!(
        results[0].get("r.alias").unwrap(),
        &Value::String("os".to_string())
    );
    tx.commit().unwrap();
}

// ── Issue 2: SET on relationship properties ───────────────────────────

#[test]
fn e2e_set_relationship_property() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:File {name: 'main.py'})-[:IMPORTS {line_number: 1}]->(b:Module {name: 'os'})")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        tx.query("MATCH (a:File)-[r:IMPORTS]->(b:Module) SET r.line_number = 5")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:File)-[r:IMPORTS]->(b:Module) RETURN r.line_number")
        .unwrap();
    assert_eq!(results[0].get("r.line_number").unwrap(), &Value::I64(5));
    tx.commit().unwrap();
}

// ── Issue 3: RETURN relationship properties ───────────────────────────

#[test]
fn e2e_return_relationship_properties() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        tx.create_edge(
            NodeId(1),
            NodeId(2),
            "KNOWS",
            [("since".to_string(), Value::I64(2020))]
                .into_iter()
                .collect(),
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, r.since, b.name")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("r.since").unwrap(), &Value::I64(2020));
    tx.commit().unwrap();
}

// ── Issue 4: MATCH...MERGE ────────────────────────────────────────────

#[test]
fn e2e_match_merge_creates_edge() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:File {key: 'main.py'})").unwrap();
        tx.query("CREATE (b:Module {key: 'os'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        tx.query(
            "MATCH (a:File {key: 'main.py'}), (b:Module {key: 'os'}) MERGE (a)-[:IMPORTS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:File)-[:IMPORTS]->(b:Module) RETURN a.key, b.key")
        .unwrap();
    assert_eq!(results.len(), 1);
    tx.commit().unwrap();
}

#[test]
fn e2e_match_merge_idempotent_edge() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:File {key: 'main.py'})").unwrap();
        tx.query("CREATE (b:Module {key: 'os'})").unwrap();
        tx.commit().unwrap();
    }
    // Run MERGE twice — should create the edge only once.
    for _ in 0..2 {
        let tx = db.begin_write().unwrap();
        tx.query(
            "MATCH (a:File {key: 'main.py'}), (b:Module {key: 'os'}) MERGE (a)-[:IMPORTS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:File)-[:IMPORTS]->(b:Module) RETURN a.key")
        .unwrap();
    assert_eq!(results.len(), 1);
    tx.commit().unwrap();
}

// ── Issue 5: DELETE after OPTIONAL MATCH ──────────────────────────────

#[test]
fn e2e_delete_after_optional_match_with_edge() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:File {key: 'main.py'})-[:IMPORTS {line: 1}]->(b:Module {key: 'os'})")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        tx.query("MATCH (a:File {key: 'main.py'}) OPTIONAL MATCH (a)-[r:IMPORTS]->(b) DELETE r")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:File)-[:IMPORTS]->(b:Module) RETURN a.key")
        .unwrap();
    assert_eq!(results.len(), 0);
    // Nodes should still exist.
    let nodes = tx.query("MATCH (n) RETURN n").unwrap();
    assert_eq!(nodes.len(), 2);
    tx.commit().unwrap();
}

#[test]
fn e2e_delete_after_optional_match_no_edge() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:File {key: 'main.py'})").unwrap();
        tx.commit().unwrap();
    }
    // OPTIONAL MATCH finds nothing — DELETE should be a no-op.
    {
        let tx = db.begin_write().unwrap();
        tx.query("MATCH (a:File {key: 'main.py'}) OPTIONAL MATCH (a)-[r:IMPORTS]->(b) DELETE r")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let nodes = tx.query("MATCH (n:File) RETURN n.key").unwrap();
    assert_eq!(nodes.len(), 1);
    tx.commit().unwrap();
}

// ---------------------------------------------------------------------------
// Regression tests for bugs found during symtext migration
// ---------------------------------------------------------------------------

/// Bug 1: RETURN alias collision with MATCH variable name.
/// When a RETURN alias matches a MATCH variable, the alias should resolve to the
/// expression value, not the node's internal ID.
#[test]
fn regression_return_alias_collision_with_match_variable() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Function {key: 'f1', name: 'main'})")
            .unwrap();
        tx.query("CREATE (b:Function {key: 'f2', name: 'helper'})")
            .unwrap();
        tx.query(
            "MATCH (a:Function {key: 'f1'}), (b:Function {key: 'f2'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    // Alias "caller" collides with MATCH variable "caller" — should still return property value.
    let results = tx
        .query(
            "MATCH (caller:Function)-[:CALLS]->(tgt:Function {name: 'helper'}) \
             RETURN caller.name AS caller",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("caller"),
        Some(&Value::String("main".into()))
    );
    tx.commit().unwrap();
}

/// Bug 2: DELETE on matched relationship should only delete the matched edge,
/// not ALL edges of that type from the source.
#[test]
fn regression_delete_matched_relationship_preserves_others() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (f:File {key: 'f1'})").unwrap();
        tx.query("CREATE (m1:Module {key: 'm1'})").unwrap();
        tx.query("CREATE (m2:Module {key: 'm2'})").unwrap();
        tx.query("MATCH (a:File {key: 'f1'}), (b:Module {key: 'm1'}) CREATE (a)-[:IMP]->(b)")
            .unwrap();
        tx.query("MATCH (a:File {key: 'f1'}), (b:Module {key: 'm2'}) CREATE (a)-[:IMP]->(b)")
            .unwrap();

        // Delete only the m2 edge.
        tx.query("MATCH (a:File {key: 'f1'})-[old:IMP]->(b:Module {key: 'm2'}) DELETE old")
            .unwrap();

        // m1 edge should survive.
        let remaining = tx
            .query("MATCH (a:File)-[:IMP]->(b:Module) RETURN b.key")
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].get("b.key"), Some(&Value::String("m1".into())));
        tx.commit().unwrap();
    }
}

/// Bug 3: Target node filter in relationship MATCH pattern must be applied.
/// `(a)-[:TYPE]->(b {key: X})` should only match neighbors with that property.
#[test]
fn regression_target_node_filter_in_relationship_match() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (f:File {key: 'f1'})").unwrap();
        tx.query("CREATE (m1:Module {key: 'm1'})").unwrap();
        tx.query("CREATE (m2:Module {key: 'm2'})").unwrap();
        // Only create edge to m1.
        tx.query("MATCH (a:File {key: 'f1'}), (b:Module {key: 'm1'}) CREATE (a)-[:IMP]->(b)")
            .unwrap();

        // Query for edge to m2 (which doesn't exist) — should return empty.
        let check = tx
            .query(
                "MATCH (a:File {key: 'f1'})-[r:IMP]->(b:Module {key: 'm2'}) \
                 RETURN a.key AS src",
            )
            .unwrap();
        assert!(check.is_empty(), "expected no results, got {check:?}");

        // Query for edge to m1 — should return a match.
        let check = tx
            .query(
                "MATCH (a:File {key: 'f1'})-[r:IMP]->(b:Module {key: 'm1'}) \
                 RETURN a.key AS src",
            )
            .unwrap();
        assert_eq!(check.len(), 1);
        assert_eq!(check[0].get("src"), Some(&Value::String("f1".into())));
        tx.commit().unwrap();
    }
}

/// Bug 4: Open-ended variable-length path `*1..` should be parsed (not rejected).
#[test]
fn regression_open_ended_variable_length_path() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Class {name: 'Base'})").unwrap();
        tx.query("CREATE (b:Class {name: 'Mid'})").unwrap();
        tx.query("CREATE (c:Class {name: 'Leaf'})").unwrap();
        tx.query(
            "MATCH (a:Class {name: 'Mid'}), (b:Class {name: 'Base'}) CREATE (a)-[:INHERITS]->(b)",
        )
        .unwrap();
        tx.query(
            "MATCH (a:Class {name: 'Leaf'}), (b:Class {name: 'Mid'}) CREATE (a)-[:INHERITS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    // Open-ended range *1.. should now parse and find transitive ancestors.
    let results = tx
        .query("MATCH (a:Class {name: 'Leaf'})-[:INHERITS*1..]->(b:Class) RETURN b.name")
        .unwrap();
    let names: Vec<&str> = results
        .iter()
        .filter_map(|r| match r.get("b.name") {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"Mid"), "should find Mid ancestor");
    assert!(names.contains(&"Base"), "should find Base ancestor");
    tx.commit().unwrap();
}

// --- Scalar function tests ---

#[test]
fn e2e_tolower_toupper() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) WHERE toLower(n.name) = 'alice' RETURN toUpper(n.name) AS upper")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("upper"),
        Some(&Value::String("ALICE".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_tostring_tointeger_tofloat() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Alice'}) RETURN toString(n.age) AS s, toFloat(n.age) AS f")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("s"), Some(&Value::String("30".into())));
    assert_eq!(results[0].get("f"), Some(&Value::F64(30.0)));
    tx.commit().unwrap();
}

#[test]
fn e2e_coalesce() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Alice'}) RETURN coalesce(n.missing, n.name) AS val")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("val"), Some(&Value::String("Alice".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_substring() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Charlie'}) RETURN substring(n.name, 0, 4) AS sub")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("sub"), Some(&Value::String("Char".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_replace_function() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Alice'}) RETURN replace(n.name, 'ice', 'an') AS r")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("r"), Some(&Value::String("Alan".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_split_function() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Data {path: 'src/foo/bar.rs'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Data) RETURN split(n.path, '/') AS parts")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("parts"),
        Some(&Value::List(vec![
            Value::String("src".into()),
            Value::String("foo".into()),
            Value::String("bar.rs".into()),
        ]))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_trim_function() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Data {val: '  hello  '})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx.query("MATCH (n:Data) RETURN trim(n.val) AS t").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("t"), Some(&Value::String("hello".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_reverse_function() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Bob'}) RETURN reverse(n.name) AS r")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("r"), Some(&Value::String("boB".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_size_function() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Alice'}) RETURN size(n.name) AS s")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("s"), Some(&Value::I64(5)));
    tx.commit().unwrap();
}

#[test]
fn e2e_abs_function() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Data {val: -42})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx.query("MATCH (n:Data) RETURN abs(n.val) AS a").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("a"), Some(&Value::I64(42)));
    tx.commit().unwrap();
}

#[test]
fn e2e_range_function() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("UNWIND range(1, 5) AS i RETURN collect(i) AS nums")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("nums"),
        Some(&Value::List(vec![
            Value::I64(1),
            Value::I64(2),
            Value::I64(3),
            Value::I64(4),
            Value::I64(5),
        ]))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_head_tail_last() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "UNWIND [[1, 2, 3]] AS list RETURN head(list) AS h, last(list) AS l, tail(list) AS t",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("h"), Some(&Value::I64(1)));
    assert_eq!(results[0].get("l"), Some(&Value::I64(3)));
    assert_eq!(
        results[0].get("t"),
        Some(&Value::List(vec![Value::I64(2), Value::I64(3)]))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_id_as_property_name_still_works() {
    // Regression: adding id() as a function must not break {id: X} property maps.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Item {id: 42, name: 'widget'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx.query("MATCH (n:Item {id: 42}) RETURN n.name").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("widget".into()))
    );
    tx.commit().unwrap();
}

// ===========================================================================
// Standalone relationship MERGE tests
// ===========================================================================

#[test]
fn merge_relationship_creates_nodes_and_edge() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    // Both nodes should exist.
    let people = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(people.len(), 2);
    // The edge should exist.
    let edges = tx
        .query("MATCH (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'}) RETURN a.name, b.name")
        .unwrap();
    assert_eq!(edges.len(), 1);
    tx.commit().unwrap();
}

#[test]
fn merge_relationship_is_idempotent() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        // Run it again — should not create duplicates.
        tx.query("MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let people = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(people.len(), 2);
    tx.commit().unwrap();
}

#[test]
fn merge_relationship_reuses_existing_nodes() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.begin_write().unwrap();
        // Alice already exists — should reuse her.
        tx.query("MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let people = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(people.len(), 2); // Not 3!
    tx.commit().unwrap();
}

#[test]
fn merge_relationship_with_edge_properties() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query(
            "MERGE (a:Person {name: 'Alice'})-[:KNOWS {since: 2020}]->(b:Person {name: 'Bob'})",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (a:Person {name: 'Alice'})-[r:KNOWS]->(b) RETURN r.since")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("r.since"), Some(&Value::I64(2020)));
    tx.commit().unwrap();
}

// ===========================================================================
// Parameterized query tests
// ===========================================================================

#[test]
fn parameterized_match_with_literal() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (n:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let params: HashMap<String, Value> =
        [("name".to_string(), Value::String("Alice".into()))].into();
    let results = tx
        .query_with_params("MATCH (n:Person {name: $name}) RETURN n.age", Some(&params))
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(30)));
    tx.commit().unwrap();
}

#[test]
fn parameterized_create() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        let params: HashMap<String, Value> = [
            ("name".to_string(), Value::String("Charlie".into())),
            ("age".to_string(), Value::I64(40)),
        ]
        .into();
        tx.query_with_params("CREATE (n:Person {name: $name, age: $age})", Some(&params))
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person {name: 'Charlie'}) RETURN n.age")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(40)));
    tx.commit().unwrap();
}

#[test]
fn parameterized_where_clause() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (n:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (n:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let params: HashMap<String, Value> = [("min_age".to_string(), Value::I64(28))].into();
    let results = tx
        .query_with_params(
            "MATCH (n:Person) WHERE n.age > $min_age RETURN n.name",
            Some(&params),
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn parameterized_merge() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        let params: HashMap<String, Value> =
            [("key".to_string(), Value::String("fn:main".into()))].into();
        tx.query_with_params("MERGE (n:Function {key: $key})", Some(&params))
            .unwrap();
        // Second call should not create a duplicate.
        tx.query_with_params("MERGE (n:Function {key: $key})", Some(&params))
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx.query("MATCH (n:Function) RETURN n.key").unwrap();
    assert_eq!(results.len(), 1);
    tx.commit().unwrap();
}

#[test]
fn parameterized_missing_param_errors() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    let params: HashMap<String, Value> = HashMap::new();
    let result = tx.query_with_params("MATCH (n:Person {name: $name}) RETURN n", Some(&params));
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("missing parameter: $name"), "got: {err}");
    tx.rollback().unwrap();
}

#[test]
fn parameterized_with_index() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.create_index("Function", "key").unwrap();
        tx.query("CREATE (n:Function {key: 'fn:main', name: 'main'})")
            .unwrap();
        tx.query("CREATE (n:Function {key: 'fn:helper', name: 'helper'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let params: HashMap<String, Value> =
        [("key".to_string(), Value::String("fn:main".into()))].into();
    let results = tx
        .query_with_params(
            "MATCH (n:Function {key: $key}) RETURN n.name",
            Some(&params),
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("main".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_with_order_by() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (:Function {name: 'foo', path: 'a.py'})")
            .unwrap();
        tx.query("CREATE (:Function {name: 'bar', path: 'a.py'})")
            .unwrap();
        tx.query("CREATE (:Function {name: 'baz', path: 'b.py'})")
            .unwrap();
        // foo->bar, baz->bar (bar has 2 callers)
        tx.query(
            "MATCH (a:Function {name: 'foo'}), (b:Function {name: 'bar'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();
        tx.query(
            "MATCH (a:Function {name: 'baz'}), (b:Function {name: 'bar'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();
        // baz->foo (foo has 1 caller)
        tx.query(
            "MATCH (a:Function {name: 'baz'}), (b:Function {name: 'foo'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();
        // foo->baz (baz has 1 caller)
        tx.query(
            "MATCH (a:Function {name: 'foo'}), (b:Function {name: 'baz'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query(
            "MATCH (a:Function)-[:CALLS]->(b:Function) \
             WITH b.path AS path, b.name AS name, count(*) AS caller_count \
             ORDER BY path, caller_count DESC \
             RETURN path, COLLECT(name) AS top",
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get("path"), Some(&Value::String("a.py".into())));
    // bar has 2 callers, foo has 1 — bar should sort first
    assert_eq!(
        results[0].get("top"),
        Some(&Value::List(vec![
            Value::String("bar".into()),
            Value::String("foo".into()),
        ]))
    );
    assert_eq!(results[1].get("path"), Some(&Value::String("b.py".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_with_order_by_and_limit() {
    let mut db = setup_social_graph();
    let tx = db.begin_read().unwrap();
    // ORDER BY + LIMIT on WITH: keep only the youngest person
    let results = tx
        .query(
            "MATCH (n:Person) \
             WITH n.name AS name, n.age AS age \
             ORDER BY age ASC \
             LIMIT 1 \
             RETURN name, age",
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("name"), Some(&Value::String("Bob".into())));
    assert_eq!(results[0].get("age"), Some(&Value::I64(25)));
    tx.commit().unwrap();
}

#[test]
fn test_pattern_comprehension_nested_in_list_comprehension() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query(
            "CREATE (n1:X {n: 1}), (m1:Y), (i1:Y), (i2:Y) CREATE (n1)-[:T]->(m1), (m1)-[:T]->(i1), (m1)-[:T]->(i2)",
        ).unwrap();
        tx.query(
            "CREATE (n2:X {n: 2}), (m2), (i3:L), (i4:Y) CREATE (n2)-[:T]->(m2), (m2)-[:T]->(i3), (m2)-[:T]->(i4)",
        ).unwrap();
        tx.commit().unwrap();
    }
    let reader = db.begin_read().unwrap();

    let result = reader.query(
        "MATCH p = (n:X)-->() RETURN n.n AS nn, [x IN nodes(p) | size([(x)-->(:Y) | 1])] AS list",
    ).unwrap();

    for row in &result {
        if row.get("nn") == Some(&Value::I64(1)) {
            assert_eq!(
                row.get("list"),
                Some(&Value::List(vec![Value::I64(1), Value::I64(2)])),
                "For n=1, expected [1, 2]"
            );
        }
        if row.get("nn") == Some(&Value::I64(2)) {
            assert_eq!(
                row.get("list"),
                Some(&Value::List(vec![Value::I64(0), Value::I64(1)])),
                "For n=2, expected [0, 1]"
            );
        }
    }
}

#[test]
fn large_duration_between() {
    // Analogous to TCK Temporal10[9] but within chrono's year range.
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("RETURN duration.between(date('0001-01-01'), date('9999-12-31')) AS duration")
        .unwrap();
    assert_eq!(rows.len(), 1);
    let dur = rows[0].get("duration").unwrap();
    assert_eq!(format!("{dur}"), "P9998Y11M30D");
}

#[test]
fn large_duration_in_seconds() {
    // Analogous to TCK Temporal10[10] but within chrono's year range
    // (original uses ±999999999 years; chrono caps at ~±262,143).
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let rows = tx
        .query("RETURN duration.inSeconds(localdatetime('1000-01-01T00:00:00'), localdatetime('1200-12-31T23:59:59')) AS duration")
        .unwrap();
    assert_eq!(rows.len(), 1);
    let dur = rows[0].get("duration").unwrap();
    assert_eq!(format!("{dur}"), "PT1761935H59M59S");
}
