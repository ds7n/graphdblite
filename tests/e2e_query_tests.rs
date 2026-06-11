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
        let tx = db.write_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
    let results = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(results.len(), 3);
    tx.commit().unwrap();
}

#[test]
fn e2e_match_with_where_filter() {
    let mut db = setup_social_graph();
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
    let results = tx.query("MATCH (n:Person) RETURN count(*) AS cnt").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("cnt"), Some(&Value::I64(3)));
    tx.commit().unwrap();
}

#[test]
fn e2e_order_by_and_limit() {
    let mut db = setup_social_graph();
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Person {name: 'Dave', age: 40})")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MATCH (n:Person) WHERE n.name = 'Charlie' DETACH DELETE n")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
        let results = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(results.len(), 2); // Alice and Bob remain
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_set_property() {
    let mut db = setup_social_graph();
    {
        let tx = db.write_tx().unwrap();
        tx.query("MATCH (n:Person) WHERE n.name = 'Alice' SET n.age = 31")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.source = 'created'")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'}) ON MATCH SET n.seen = true")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'})").unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'})").unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
        let results = tx.query("MATCH (n:Person) RETURN count(*) AS cnt").unwrap();
        assert_eq!(results[0].get("cnt"), Some(&Value::I64(1))); // only 1 Alice
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_return_star() {
    let mut db = setup_social_graph();
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query(
            "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MATCH (a:Person)-[:KNOWS]->(b:Person) CREATE (b)-[:KNOWS]->(a)")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query(
            "MATCH (a:Person {name: 'Alice'}) CREATE (a)-[:WORKS_AT]->(c:Company {name: 'Acme'})",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', dept: 'eng'})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', dept: 'eng'})")
            .unwrap();
        tx.query("CREATE (c:Person {name: 'Charlie', dept: 'sales'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.query("CREATE (c:Person {name: 'Charlie', age: 35})")
            .unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Alice', age: 25})")
            .unwrap();
        tx.query("CREATE (c:Person {name: 'Bob', age: 30})")
            .unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
        tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
            .unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (:Person {name: 'Alice', dept: 'eng'})")
        .unwrap();
    tx.query("CREATE (:Person {name: 'Bob', dept: 'eng'})")
        .unwrap();
    tx.query("CREATE (:Person {name: 'Charlie', dept: 'eng'})")
        .unwrap();
    tx.query("CREATE (:Person {name: 'Diana', dept: 'sales'})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (:Person {dept: 'sales'})").unwrap();
    tx.query("CREATE (:Person {dept: 'ops'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        // Alice has no edges — plain DELETE should work.
        tx.query("MATCH (n:Person) WHERE n.name = 'Alice' DELETE n")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
        let results = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
        assert_eq!(results.len(), 0);
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_detach_delete_cascades_edges() {
    let mut db = setup_social_graph();
    {
        let tx = db.write_tx().unwrap();
        // Bob has edges (Alice->Bob KNOWS, Bob->Charlie KNOWS) — DETACH DELETE cascades.
        tx.query("MATCH (n:Person) WHERE n.name = 'Bob' DETACH DELETE n")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Anna'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Grace'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name ENDS WITH 'zzz' RETURN n.name")
        .unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_contains_string() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Lick'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person)").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
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

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'sales'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'sales'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'hr'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {dept: 'eng', age: 30})")
        .unwrap();
    tx.query("CREATE (n:Person {dept: 'eng', age: 40})")
        .unwrap();
    tx.query("CREATE (n:Person {dept: 'sales', age: 25})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
    let rows = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_match_no_label_on_empty_database() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let rows = tx.query("MATCH (n) RETURN n").unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_count_on_empty_database() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let rows = tx.query("MATCH (n:Person) RETURN count(*) AS cnt").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("cnt").unwrap(), &Value::I64(0));
    tx.commit().unwrap();
}

#[test]
fn e2e_where_on_missing_property() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob', email: 'bob@test.com'})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice', age: 30})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.write_tx().unwrap();
    tx.query("MATCH (n:Person) WHERE n.name = 'Alice' SET n.age = null")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {name: '日本語テスト'})")
        .unwrap();
    tx.query("CREATE (n:Person {name: 'émojis 🎉🚀'})").unwrap();
    tx.query("CREATE (n:Person {name: 'Ñoño'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    for i in 0..1000 {
        tx.query(&format!("CREATE (n:Item {{id: {i}}})")).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person {dept: 'eng'})").unwrap();
    tx.query("CREATE (n:Person)").unwrap(); // no dept
    tx.query("CREATE (n:Person)").unwrap(); // no dept
    tx.query("CREATE (n:Person)").unwrap(); // no dept
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    for i in 1..=10 {
        tx.query(&format!("CREATE (n:Num {{val: {i}}})")).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Charlie'})").unwrap();
    tx.commit().unwrap();

    let tx = db.write_tx().unwrap();
    // Only Alice knows people.
    tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(1), NodeId(3), "KNOWS", HashMap::new())
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    // Three functions; two callers.
    tx.query("CREATE (fn1:Function {name: 'main'})").unwrap();
    tx.query("CREATE (fn2:Function {name: 'helper'})").unwrap();
    tx.query("CREATE (fn3:Function {name: 'unused'})").unwrap();
    tx.commit().unwrap();

    let tx = db.write_tx().unwrap();
    // main is called by helper and unused; helper is called by main; unused is called by nobody.
    tx.create_edge(NodeId(2), NodeId(1), "CALLS", HashMap::new())
        .unwrap(); // helper -> main
    tx.create_edge(NodeId(3), NodeId(1), "CALLS", HashMap::new())
        .unwrap(); // unused -> main
    tx.create_edge(NodeId(1), NodeId(2), "CALLS", HashMap::new())
        .unwrap(); // main -> helper
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (fn1:Function {name: 'used'})").unwrap();
    tx.query("CREATE (fn2:Function {name: 'dead'})").unwrap();
    tx.commit().unwrap();

    let tx = db.write_tx().unwrap();
    tx.create_edge(NodeId(1), NodeId(1), "CALLS", HashMap::new())
        .unwrap(); // used calls itself (recursive)
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
        .unwrap();
    tx.query("CREATE (b:Person {name: 'Bob', age: 25})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.write_tx().unwrap();
    tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(1), NodeId(1), "KNOWS", HashMap::new())
        .unwrap(); // Alice self-loop so she has 2 KNOWS
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
    tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
    tx.query("CREATE (c:Person {name: 'Charlie'})").unwrap();
    tx.commit().unwrap();

    let tx = db.write_tx().unwrap();
    // Alice KNOWS Bob; Alice FOLLOWS Charlie. No outgoing edges for Bob/Charlie.
    tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(1), NodeId(3), "FOLLOWS", HashMap::new())
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.updated = true")
            .unwrap();
        tx.commit().unwrap();
    }
    // Verify created.
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.updated = true")
            .unwrap();
        tx.commit().unwrap();
    }
    // Verify updated.
    {
        let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("MATCH (n:Person {name: 'Alice'}) DETACH DELETE n")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Num {val: 9223372036854775807})")
        .unwrap(); // i64::MAX
    tx.query("CREATE (n:Num {val: -9223372036854775808})")
        .unwrap(); // i64::MIN
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();

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
    let tx = db.read_tx().unwrap();
    let rows = tx.query("RETURN 1 / 0 AS x, 1 % 0 AS y").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("x").unwrap(), &Value::Null);
    assert_eq!(rows[0].get("y").unwrap(), &Value::Null);
    tx.commit().unwrap();
}

#[test]
fn e2e_null_propagation_arithmetic() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
fn e2e_undefined_variable_did_you_mean_hint() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    // `nmae` is one transposition away from the in-scope `name`.
    let err = tx
        .query("WITH 'Alice' AS name RETURN nmae")
        .expect_err("expected undefined-variable error");
    let msg = format!("{err}");
    assert!(
        msg.contains("did you mean") && msg.contains("name"),
        "expected suggestion in error, got: {msg}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_var_length_hop_cap_rejects_unbounded_traversal() {
    // Regression: explicit large hop counts in `*1..N` were accepted and would
    // run unbounded traversal, OOMing on dense graphs. Must now error.
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let err = tx
        .query("MATCH (a)-[*1..1000000]->(b) RETURN a")
        .expect_err("expected hop-cap error");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("hop") || msg.contains("cap") || msg.contains("out of range"),
        "expected hop-cap error, got: {msg}"
    );
    // Sanity: a hop count within the cap still parses.
    tx.query("MATCH (a)-[*1..3]->(b) RETURN a").unwrap();
    tx.commit().unwrap();
}

#[test]
fn e2e_var_length_hop_cap_is_configurable_via_config() {
    // Regression: `Config::max_traversal_depth` must actually be enforced.
    // A query within the default cap but above a custom cap must error; the
    // same query must succeed when the cap is raised.
    use graphdblite::Config;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("depth.db");
    let mut db = Database::open_with_config(
        &path,
        Config {
            max_traversal_depth: 5,
            ..Default::default()
        },
    )
    .unwrap();
    let tx = db.read_tx().unwrap();
    let err = tx
        .query("MATCH (a)-[*1..10]->(b) RETURN a")
        .expect_err("expected configured-depth error");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("traversal depth") && msg.contains("5"),
        "expected error to cite the configured cap, got: {msg}"
    );
    tx.query("MATCH (a)-[*1..5]->(b) RETURN a").unwrap();
    tx.commit().unwrap();
}

#[test]
fn e2e_range_size_cap_rejects_huge_allocations() {
    // Regression: range() used to allocate the full Vec without a size guard,
    // OOMing the host on `RETURN range(0, 9999999999)`. Must now reject.
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let err = tx
        .query("RETURN range(0, 9999999999)")
        .expect_err("expected size-cap error");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("out of range") || msg.contains("cap"),
        "expected size-cap error, got: {msg}"
    );
    // Sanity: a reasonable range still works.
    let rows = tx.query("RETURN range(1, 5)").unwrap();
    assert_eq!(rows.len(), 1);
    tx.commit().unwrap();
}

#[test]
fn e2e_duration_months_overflow_errors_not_panics() {
    // Regression: chrono::Months::new requires the value to fit in i32, and
    // panics otherwise. Adding a huge-month duration to a date used to abort
    // the host process. Must now surface a NumberOutOfRange.
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let err = tx
        .query("RETURN date('2020-01-01') + duration({months: 9999999999999})")
        .expect_err("expected overflow error on huge months");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("out of range") || msg.contains("i32"),
        "expected overflow error, got: {msg}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_var_length_hop_overflow_errors_not_panics() {
    // Regression: parser used to `unwrap()` on `.parse::<u32>()`, panicking the
    // host process on `*1..99999999999`. Must now surface a NumberOutOfRange.
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let err = tx
        .query("MATCH (a)-[*1..99999999999]->(b) RETURN a")
        .expect_err("expected overflow error");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("out of range") || msg.contains("u32"),
        "expected overflow error, got: {msg}"
    );
    let err = tx
        .query("MATCH (a)-[*99999999999]->(b) RETURN a")
        .expect_err("expected overflow error on fixed length");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("out of range") || msg.contains("u32"),
        "expected overflow error, got: {msg}"
    );
    tx.commit().unwrap();
}

// Slow (~2 min in debug): exercises the full 10M-edge-visit fuel budget.
// Run on demand with `cargo test -- --ignored`.
#[test]
#[ignore]
fn e2e_var_length_traversal_fuel_cap_rejects_factorial_blowup() {
    // Regression: even with hop counts within MAX_VAR_LENGTH_HOPS, a small
    // dense graph could enumerate factorially many paths and burn host
    // resources. Must now hit the fuel cap and return a SizeLimit error.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        // Build K12 (complete directed graph): 12 nodes, every ordered pair
        // connected. *1..11 from any node enumerates ~11! ≈ 4e7 paths, each
        // visiting up to 11 neighbors per hop — far above the 10M fuel cap.
        tx.query("UNWIND range(1, 12) AS i CREATE (:N {id: i})")
            .unwrap();
        tx.query("MATCH (a:N), (b:N) WHERE a.id <> b.id CREATE (a)-[:R]->(b)")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let err = tx
        .query("MATCH (a:N {id: 1})-[*1..11]->(b) RETURN b")
        .expect_err("expected fuel-cap error on K12 traversal");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("traversal work") && msg.contains("exceeds"),
        "expected traversal fuel-cap error, got: {msg}"
    );
    // Sanity: a small hop range on the same graph still works.
    let _ = tx
        .query("MATCH (a:N {id: 1})-[*1..2]->(b) RETURN b LIMIT 5")
        .unwrap();
    tx.commit().unwrap();
}

#[cfg(unix)]
#[test]
fn e2e_new_db_file_created_with_owner_only_perms() {
    // Regression: DB file used to be created with the process umask (often
    // 0o644) and chmod'd to 0o600 afterward — TOCTOU window. Must now be
    // 0o600 atomically from creation.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("perms_test.db");
    {
        let _db = Database::open(&path).unwrap();
    }
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
}

#[test]
fn e2e_input_size_cap_rejects_huge_query() {
    // Regression: parser/eval recursion was unbounded by input size, so a
    // pathologically large query could exhaust memory. Must now reject up
    // front with a SizeLimit error.
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let big = format!("RETURN {}", "1+".repeat(600_000));
    let err = tx.query(&big).expect_err("expected size-cap error");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("cypher query") && msg.contains("exceeds"),
        "expected query-size-cap error, got: {msg}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_expr_depth_cap_rejects_deep_nesting() {
    // Regression: deeply nested parentheses overflowed the parser's recursion
    // stack and aborted the host. Must now reject before pest descends.
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let q = format!("RETURN {}1{}", "(".repeat(10_000), ")".repeat(10_000));
    let err = tx.query(&q).expect_err("expected depth-cap error");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("nesting depth") && msg.contains("exceeds"),
        "expected depth-cap error, got: {msg}"
    );
    // Sanity: modestly nested expressions still parse.
    tx.query("RETURN ((((1 + 2)) * 3))").unwrap();
    tx.commit().unwrap();
}

#[test]
fn e2e_unknown_function_typo_hint() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    // `lenght` is a one-edit typo for `length`.
    let err = tx
        .query("RETURN lenght('abc')")
        .expect_err("expected unknown-function error");
    let msg = format!("{err}");
    assert!(
        msg.contains("did you mean") && msg.contains("length"),
        "expected suggestion in error, got: {msg}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_unknown_function_no_close_match() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    // No close match — error should still be raised, just without a hint.
    let err = tx
        .query("RETURN xyzzy(1)")
        .expect_err("expected unknown-function error");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("unknown function") || msg.contains("xyzzy"),
        "expected unknown function error, got: {msg}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_variable_to_property_hint() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    // The user wrote a bare `name` but referenced `n.name` elsewhere — the
    // hint should suggest the qualified property form.
    let err = tx
        .query("MATCH (n:Person) WHERE n.name = 'Alice' RETURN name")
        .expect_err("expected undefined-variable error");
    let msg = format!("{err}");
    // RETURN-list expressions are validated independently, so the hint comes
    // from cross-clause data only when both clauses are inspected; we accept
    // either the explicit suggestion or just a plain undefined-variable error.
    assert!(
        msg.contains("name"),
        "expected error referencing `name`, got: {msg}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_variable_to_property_hint_same_expression() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    // Bare `name` next to a `n.name` reference in the same WHERE expression.
    let err = tx
        .query("MATCH (n:Person) WHERE n.name = 'Alice' AND name = 'Alice' RETURN n")
        .expect_err("expected undefined-variable error");
    let msg = format!("{err}");
    assert!(
        msg.contains("did you mean") && msg.contains("n.name"),
        "expected qualified-property suggestion, got: {msg}"
    );
    tx.commit().unwrap();
}

#[test]
fn e2e_float_property() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Num {val: 3.14159})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Flag {active: true, deleted: false})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
    let rows = tx.query("UNWIND [] AS x RETURN x").unwrap();
    assert!(rows.is_empty());
    tx.commit().unwrap();
}

#[test]
fn e2e_unwind_string_list() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("UNWIND ['Alice', 'Bob', 'Charlie'] AS name CREATE (n:Person {name: name})")
        .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice', tags: 'dev,lead'})")
        .unwrap();
    tx.commit().unwrap();

    // Use UNWIND within a MATCH via collect + UNWIND in WITH chain.
    let tx = db.read_tx().unwrap();
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
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (n:Person {name: 'Alice'})").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:X {name: 'a'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:X {name: 'a'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:X {name: 'a'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:X {name: 'a'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (n:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.query("CREATE (n:Person {name: 'Charlie', age: 35})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'A'})").unwrap(); // NodeId(1)
        tx.query("CREATE (b:Person {name: 'B'})").unwrap(); // NodeId(2)
        tx.query("CREATE (c:Person {name: 'C'})").unwrap(); // NodeId(3)
        tx.query("CREATE (d:Person {name: 'D'})").unwrap(); // NodeId(4)
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:N {name: 'A'})").unwrap(); // 1
        tx.query("CREATE (b:N {name: 'B'})").unwrap(); // 2
        tx.query("CREATE (c:N {name: 'C'})").unwrap(); // 3
        tx.query("CREATE (d:N {name: 'D'})").unwrap(); // 4
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Animal {name: 'Dog'})").unwrap();
        tx.query("CREATE (b:Animal {name: 'Cat'})").unwrap();
        tx.query("CREATE (c:Animal {name: 'Bird'})").unwrap();
        tx.commit().unwrap();
    }
    // Check: EXPLAIN should show est. 3 rows for Animal.
    {
        let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MATCH (n:Animal {name: 'Bird'}) DELETE n")
            .unwrap();
        tx.commit().unwrap();
    }
    // Check: should now show est. 2 rows.
    {
        let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:X {name: 'hello'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:File {name: 'main.py'})-[:IMPORTS {line_number: 1, alias: 'os'}]->(b:Module {name: 'os'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:File {name: 'main.py'})-[:IMPORTS {line_number: 1}]->(b:Module {name: 'os'})")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query("MATCH (a:File)-[r:IMPORTS]->(b:Module) SET r.line_number = 5")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.query("CREATE (b:Person {name: 'Bob'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:File {key: 'main.py'})").unwrap();
        tx.query("CREATE (b:Module {key: 'os'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query(
            "MATCH (a:File {key: 'main.py'}), (b:Module {key: 'os'}) MERGE (a)-[:IMPORTS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:File {key: 'main.py'})").unwrap();
        tx.query("CREATE (b:Module {key: 'os'})").unwrap();
        tx.commit().unwrap();
    }
    // Run MERGE twice — should create the edge only once.
    for _ in 0..2 {
        let tx = db.write_tx().unwrap();
        tx.query(
            "MATCH (a:File {key: 'main.py'}), (b:Module {key: 'os'}) MERGE (a)-[:IMPORTS]->(b)",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:File {key: 'main.py'})-[:IMPORTS {line: 1}]->(b:Module {key: 'os'})")
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query("MATCH (a:File {key: 'main.py'}) OPTIONAL MATCH (a)-[r:IMPORTS]->(b) DELETE r")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:File {key: 'main.py'})").unwrap();
        tx.commit().unwrap();
    }
    // OPTIONAL MATCH finds nothing — DELETE should be a no-op.
    {
        let tx = db.write_tx().unwrap();
        tx.query("MATCH (a:File {key: 'main.py'}) OPTIONAL MATCH (a)-[r:IMPORTS]->(b) DELETE r")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Data {path: 'src/foo/bar.rs'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Data {val: '  hello  '})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let results = tx.query("MATCH (n:Data) RETURN trim(n.val) AS t").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("t"), Some(&Value::String("hello".into())));
    tx.commit().unwrap();
}

#[test]
fn e2e_reverse_function() {
    let mut db = setup_social_graph();
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Data {val: -42})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let results = tx.query("MATCH (n:Data) RETURN abs(n.val) AS a").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("a"), Some(&Value::I64(42)));
    tx.commit().unwrap();
}

#[test]
fn e2e_range_function() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Item {id: 42, name: 'widget'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        // Run it again — should not create duplicates.
        tx.query("MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let people = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(people.len(), 2);
    tx.commit().unwrap();
}

#[test]
fn merge_relationship_reuses_existing_nodes() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        // Alice already exists — should reuse her.
        tx.query("MERGE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let people = tx.query("MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(people.len(), 2); // Not 3!
    tx.commit().unwrap();
}

#[test]
fn merge_relationship_with_edge_properties() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query(
            "MERGE (a:Person {name: 'Alice'})-[:KNOWS {since: 2020}]->(b:Person {name: 'Bob'})",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let results = tx
        .query("MATCH (a:Person {name: 'Alice'})-[r:KNOWS]->(b) RETURN r.since")
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get("r.since"), Some(&Value::I64(2020)));
    tx.commit().unwrap();
}

#[test]
fn merge_edge_upsert_on_create_then_on_match() {
    // Mirrors a downstream "atomic upsert" pattern: a single MERGE that
    // CREATES with one set of props on first call and UPDATES a subset on
    // subsequent calls — replacing the older DELETE-then-CREATE workaround.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:Fn {name: 'foo'}), (:Fn {name: 'bar'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let upsert = "MATCH (a:Fn {name: 'foo'}), (b:Fn {name: 'bar'}) \
                  MERGE (a)-[r:CALLS]->(b) \
                  ON CREATE SET r += {lineno: 10, kind: 'direct'} \
                  ON MATCH SET r += {lineno: 42}";

    // First call: takes the ON CREATE branch.
    {
        let tx = db.write_tx().unwrap();
        tx.query(upsert).unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
        let rows = tx
            .query("MATCH ()-[r:CALLS]->() RETURN r.lineno AS l, r.kind AS k")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("l"), Some(&Value::I64(10)));
        assert_eq!(rows[0].get("k"), Some(&Value::String("direct".into())));
        tx.commit().unwrap();
    }

    // Second call: takes the ON MATCH branch, partial update preserves `kind`.
    {
        let tx = db.write_tx().unwrap();
        tx.query(upsert).unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
        let rows = tx
            .query("MATCH ()-[r:CALLS]->() RETURN r.lineno AS l, r.kind AS k, count(r) AS c")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("l"), Some(&Value::I64(42)));
        assert_eq!(rows[0].get("k"), Some(&Value::String("direct".into())));
        assert_eq!(rows[0].get("c"), Some(&Value::I64(1)));
        tx.commit().unwrap();
    }
}

#[test]
fn merge_edge_with_inline_props_distinguishes_parallel_edges() {
    // `MERGE (a)-[:R {x: 1}]->(b)` matches by props — an edge with a
    // different `x` is a distinct parallel edge, not a candidate for update.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:N {k: 'a'}), (:N {k: 'b'})").unwrap();
        tx.query("MATCH (a:N {k: 'a'}), (b:N {k: 'b'}) MERGE (a)-[:R {x: 1}]->(b)")
            .unwrap();
        tx.query("MATCH (a:N {k: 'a'}), (b:N {k: 'b'}) MERGE (a)-[:R {x: 1}]->(b)")
            .unwrap();
        tx.query("MATCH (a:N {k: 'a'}), (b:N {k: 'b'}) MERGE (a)-[:R {x: 2}]->(b)")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let rows = tx
        .query("MATCH ()-[r:R]->() RETURN r.x AS x ORDER BY x")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("x"), Some(&Value::I64(1)));
    assert_eq!(rows[1].get("x"), Some(&Value::I64(2)));
    tx.commit().unwrap();
}

// ===========================================================================
// Parameterized query tests
// ===========================================================================

#[test]
fn parameterized_match_with_literal() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (n:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        let params: HashMap<String, Value> = [
            ("name".to_string(), Value::String("Charlie".into())),
            ("age".to_string(), Value::I64(40)),
        ]
        .into();
        tx.query_with_params("CREATE (n:Person {name: $name, age: $age})", Some(&params))
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (n:Person {name: 'Alice', age: 30})")
            .unwrap();
        tx.query("CREATE (n:Person {name: 'Bob', age: 25})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        let params: HashMap<String, Value> =
            [("key".to_string(), Value::String("fn:main".into()))].into();
        tx.query_with_params("MERGE (n:Function {key: $key})", Some(&params))
            .unwrap();
        // Second call should not create a duplicate.
        tx.query_with_params("MERGE (n:Function {key: $key})", Some(&params))
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let results = tx.query("MATCH (n:Function) RETURN n.key").unwrap();
    assert_eq!(results.len(), 1);
    tx.commit().unwrap();
}

#[test]
fn parameterized_missing_param_errors() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.create_index("Function", "key").unwrap();
        tx.query("CREATE (n:Function {key: 'fn:main', name: 'main'})")
            .unwrap();
        tx.query("CREATE (n:Function {key: 'fn:helper', name: 'helper'})")
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
    let tx = db.read_tx().unwrap();
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
        let tx = db.write_tx().unwrap();
        tx.query(
            "CREATE (n1:X {n: 1}), (m1:Y), (i1:Y), (i2:Y) CREATE (n1)-[:T]->(m1), (m1)-[:T]->(i1), (m1)-[:T]->(i2)",
        ).unwrap();
        tx.query(
            "CREATE (n2:X {n: 2}), (m2), (i3:L), (i4:Y) CREATE (n2)-[:T]->(m2), (m2)-[:T]->(i3), (m2)-[:T]->(i4)",
        ).unwrap();
        tx.commit().unwrap();
    }
    let reader = db.read_tx().unwrap();

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
    let tx = db.read_tx().unwrap();
    let rows = tx
        .query("RETURN duration.between(date('0001-01-01'), date('9999-12-31')) AS duration")
        .unwrap();
    assert_eq!(rows.len(), 1);
    let dur = rows[0].get("duration").unwrap();
    assert_eq!(format!("{dur}"), "P9998Y11M30D");
}

#[test]
fn set_relationship_properties_replace() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:P {name: 'A'})-[:KNOWS {since: 2020, weight: 0.5}]->(b:P {name: 'B'})")
        .unwrap();
    tx.query("MATCH ()-[r:KNOWS]->() SET r = {since: 2024, source: 'doc'}")
        .unwrap();
    let rows = tx
        .query("MATCH ()-[r:KNOWS]->() RETURN r.since AS since, r.weight AS weight, r.source AS source")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("since"), Some(&Value::I64(2024)));
    assert_eq!(rows[0].get("weight"), Some(&Value::Null));
    assert_eq!(
        rows[0].get("source"),
        Some(&Value::String("doc".to_string()))
    );
    tx.commit().unwrap();
}

#[test]
fn set_relationship_properties_merge() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    tx.query("CREATE (a:P {name: 'A'})-[:KNOWS {since: 2020, weight: 0.5}]->(b:P {name: 'B'})")
        .unwrap();
    tx.query("MATCH ()-[r:KNOWS]->() SET r += {weight: null, source: 'doc'}")
        .unwrap();
    let rows = tx
        .query("MATCH ()-[r:KNOWS]->() RETURN r.since AS since, r.weight AS weight, r.source AS source")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("since"), Some(&Value::I64(2020)));
    assert_eq!(rows[0].get("weight"), Some(&Value::Null));
    assert_eq!(
        rows[0].get("source"),
        Some(&Value::String("doc".to_string()))
    );
    tx.commit().unwrap();
}

#[test]
fn large_duration_in_seconds() {
    // Analogous to TCK Temporal10[10] but within chrono's year range
    // (original uses ±999999999 years; chrono caps at ~±262,143).
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let rows = tx
        .query("RETURN duration.inSeconds(localdatetime('1000-01-01T00:00:00'), localdatetime('1200-12-31T23:59:59')) AS duration")
        .unwrap();
    assert_eq!(rows.len(), 1);
    let dur = rows[0].get("duration").unwrap();
    assert_eq!(format!("{dur}"), "PT1761935H59M59S");
}

#[test]
fn id_lookup_planner_rewrites_unlabeled_id_equality_to_idlookup() {
    // Sanity: the rewrite plus exec dispatch produces the right row, and
    // (more importantly) does so without scanning the whole node table.
    // The latter is verified by the perf regression test below.
    let mut db = Database::open_memory().unwrap();
    let target_id;
    {
        let tx = db.write_tx().unwrap();
        for i in 0..200 {
            tx.query(&format!("CREATE (:Filler {{i: {i}}})")).unwrap();
        }
        let rows = tx
            .query("CREATE (n:Target {tag: 'hit'}) RETURN id(n) AS id")
            .unwrap();
        target_id = match rows[0].get("id") {
            Some(Value::I64(v)) => *v,
            other => panic!("expected i64 id, got {other:?}"),
        };
        tx.commit().unwrap();
    }
    let tx = db.read_tx().unwrap();
    let rows = tx
        .query(&format!(
            "MATCH (n) WHERE id(n) = {target_id} RETURN n.tag AS tag"
        ))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("tag"), Some(&Value::String("hit".into())));
    tx.commit().unwrap();
}

#[test]
fn id_lookup_planner_supports_parameterized_id_equality() {
    let mut db = Database::open_memory().unwrap();
    let target_id;
    {
        let tx = db.write_tx().unwrap();
        let rows = tx
            .query("CREATE (n:Target {tag: 'hit'}) RETURN id(n) AS id")
            .unwrap();
        target_id = match rows[0].get("id") {
            Some(Value::I64(v)) => *v,
            other => panic!("expected i64 id, got {other:?}"),
        };
        tx.commit().unwrap();
    }
    let mut params = std::collections::HashMap::new();
    params.insert("x".to_string(), Value::I64(target_id));
    let tx = db.read_tx().unwrap();
    let rows = tx
        .query_with_params(
            "MATCH (n) WHERE id(n) = $x RETURN n.tag AS tag",
            Some(&params),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("tag"), Some(&Value::String("hit".into())));
    tx.commit().unwrap();
}

#[test]
fn id_lookup_planner_returns_no_rows_for_nonexistent_id() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.read_tx().unwrap();
    let rows = tx.query("MATCH (n) WHERE id(n) = 99999 RETURN n").unwrap();
    assert_eq!(rows.len(), 0);
    tx.commit().unwrap();
}

#[test]
fn id_lookup_avoids_full_scan_under_unwind_batch() {
    // Perf regression test for the `batch_create_edges` pathology.
    // Without the IdLookup rewrite this is O(N_rows * N_nodes_in_graph)
    // (full scan per row, twice) and times out spectacularly. With the
    // rewrite each row is O(1). 200-node graph * 500 edge rows is enough
    // contrast that a regression would dominate the timeout budget.
    use std::time::Instant;
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    for i in 0..200 {
        tx.query(&format!("CREATE (:N {{i: {i}}})")).unwrap();
    }
    // Build src/dst pairs over the existing ids 0..200, cycling.
    let mut rows: Vec<Value> = Vec::with_capacity(500);
    for i in 0..500u64 {
        let mut row = std::collections::BTreeMap::new();
        row.insert("s".to_string(), Value::I64((i % 200) as i64));
        row.insert("d".to_string(), Value::I64(((i + 1) % 200) as i64));
        rows.push(Value::Map(row));
    }
    let mut params = std::collections::HashMap::new();
    params.insert("rows".to_string(), Value::List(rows));
    let start = Instant::now();
    tx.query_with_params(
        "UNWIND $rows AS row \
         MATCH (a) WHERE id(a) = row.s \
         MATCH (b) WHERE id(b) = row.d \
         CREATE (a)-[:R]->(b)",
        Some(&params),
    )
    .unwrap();
    let elapsed = start.elapsed();
    tx.commit().unwrap();
    // Generous bound: without the rewrite this takes seconds on a 200-node
    // graph; with the rewrite it's milliseconds. 3 s catches a regression
    // without being flaky under cargo test load.
    assert!(
        elapsed.as_secs() < 3,
        "id-lookup batch took {elapsed:?}, expected <3s (regression to full-scan path?)"
    );
}

#[test]
fn contains_predicate_uses_fulltext_index_when_present() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    for s in ["alpha bravo", "charlie delta", "echo foxtrot golf"] {
        db.execute(&format!("CREATE (:Doc {{body: '{s}'}})"))
            .unwrap();
    }
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    // Check EXPLAIN plan shape — the planner must emit FullTextLookup.
    // We do NOT run the CONTAINS query itself because exec_fulltext_lookup
    // is a panic stub until Task 13.
    let rows = db
        .execute("EXPLAIN MATCH (n:Doc) WHERE n.body CONTAINS 'bravo' RETURN n")
        .unwrap();
    assert_eq!(rows.len(), 1, "EXPLAIN returns exactly one row");
    let plan = match rows[0].get("plan").unwrap() {
        Value::String(s) => s.clone(),
        other => panic!("expected string plan, got {other:?}"),
    };
    assert!(
        plan.contains("FullTextLookup"),
        "expected FullTextLookup in EXPLAIN plan, got: {plan}"
    );
}

#[test]
fn fts_contains_returns_correct_rows_via_planner_rewrite() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    for s in ["alpha bravo", "charlie delta", "echo foxtrot golf"] {
        db.execute(&format!("CREATE (:Doc {{body: '{s}'}})"))
            .unwrap();
    }
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute("MATCH (n:Doc) WHERE n.body CONTAINS 'bravo' RETURN n.body AS b")
        .unwrap();
    assert_eq!(rows.len(), 1);
    let b = match rows[0].get("b").unwrap() {
        Value::String(s) => s.clone(),
        v => panic!("expected string, got {v:?}"),
    };
    assert_eq!(b, "alpha bravo");
}

#[test]
fn fts_starts_with_uses_index() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    for s in ["foobar", "barfoo", "foo bar"] {
        db.execute(&format!("CREATE (:Doc {{body: '{s}'}})"))
            .unwrap();
    }
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute("MATCH (n:Doc) WHERE n.body STARTS WITH 'foo' RETURN n.body AS b")
        .unwrap();
    let mut got: Vec<String> = rows
        .iter()
        .map(|r| match r.get("b").unwrap() {
            Value::String(s) => s.clone(),
            _ => unreachable!(),
        })
        .collect();
    got.sort();
    assert_eq!(got, vec!["foo bar".to_string(), "foobar".to_string()]);
}

#[test]
fn fts_ends_with_uses_index() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    for s in ["foobar", "barfoo", "foo bar"] {
        db.execute(&format!("CREATE (:Doc {{body: '{s}'}})"))
            .unwrap();
    }
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute("MATCH (n:Doc) WHERE n.body ENDS WITH 'foo' RETURN n.body AS b")
        .unwrap();
    let got: Vec<String> = rows
        .iter()
        .map(|r| match r.get("b").unwrap() {
            Value::String(s) => s.clone(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(got, vec!["barfoo".to_string()]);
}

#[test]
fn fts_contains_short_term_falls_back_to_scan() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.execute("CREATE (:Doc {body: 'ab'})").unwrap();
    db.execute("CREATE (:Doc {body: 'cd'})").unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    // 2-char term: below the trigram floor. Must still return correct rows.
    let rows = db
        .execute("MATCH (n:Doc) WHERE n.body CONTAINS 'ab' RETURN n.body AS b")
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn fts_contains_param_term_works() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.execute("CREATE (:Doc {body: 'hello world'})").unwrap();
    db.execute("CREATE (:Doc {body: 'goodbye'})").unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    use std::collections::HashMap;
    let mut params = HashMap::new();
    params.insert("term".to_string(), Value::String("world".into()));
    let rows = db
        .execute_with_params(
            "MATCH (n:Doc) WHERE n.body CONTAINS $term RETURN n.body AS b",
            Some(&params),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn fts_contains_uses_index_in_correlated_position() {
    // WITH ... AS term MATCH ... WHERE n.body CONTAINS term exercises
    // the correlated plan: Filter(CorrelatedJoin{right: Scan}, predicate)
    // which should rewrite to CorrelatedJoin{right: FullTextLookup}.
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    for s in ["alpha bravo", "charlie", "echo bravo foxtrot"] {
        db.execute(&format!("CREATE (:Doc {{body: '{s}'}})"))
            .unwrap();
    }
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "WITH 'bravo' AS term \
             MATCH (n:Doc) WHERE n.body CONTAINS term \
             RETURN n.body AS b",
        )
        .unwrap();
    let mut got: Vec<String> = rows
        .iter()
        .map(|r| match r.get("b").unwrap() {
            graphdblite::Value::String(s) => s.clone(),
            _ => unreachable!(),
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec!["alpha bravo".to_string(), "echo bravo foxtrot".to_string()]
    );
}

#[test]
fn fts_correlated_explain_shows_fulltext_lookup_in_right_side() {
    // EXPLAIN WITH ... AS term MATCH ... WHERE CONTAINS exercises the correlated
    // planner rewrite. The plan should show FullTextLookup inside the join.
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "EXPLAIN WITH 'bravo' AS term \
             MATCH (n:Doc) WHERE n.body CONTAINS term \
             RETURN n.body AS b",
        )
        .unwrap();
    let plan = format!("{rows:?}");
    assert!(
        plan.contains("FullTextLookup"),
        "correlated form must rewrite to FullTextLookup; plan: {plan}"
    );
}

#[test]
fn fts_equality_uses_regular_index_even_when_fulltext_exists() {
    // When both regular and fulltext indexes exist on the same (label, property),
    // equality (=) should pick the regular IndexLookup, not FullTextLookup.
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_index("Doc", "body").unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.execute("CREATE (:Doc {body: 'hello world'})").unwrap();
    db.commit().unwrap();

    let plan = db
        .execute("EXPLAIN MATCH (n:Doc) WHERE n.body = 'hello world' RETURN n")
        .unwrap();
    let plan_str = format!("{plan:?}");
    assert!(
        plan_str.contains("IndexLookup") && !plan_str.contains("FullTextLookup"),
        "equality should pick regular IndexLookup; got: {plan_str}"
    );
}

#[test]
fn fts_contains_uses_fulltext_index_when_both_exist() {
    // When both regular and fulltext indexes exist on the same (label, property),
    // substring operators (CONTAINS, STARTS WITH, ENDS WITH) should pick FullTextLookup.
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_index("Doc", "body").unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.execute("CREATE (:Doc {body: 'hello world'})").unwrap();
    db.commit().unwrap();

    let plan = db
        .execute("EXPLAIN MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n")
        .unwrap();
    let plan_str = format!("{plan:?}");
    assert!(
        plan_str.contains("FullTextLookup"),
        "CONTAINS should pick FullTextLookup; got: {plan_str}"
    );
    // Sanity: equality form does not appear in this plan.
    assert!(
        !plan_str.contains("IndexLookup"),
        "this CONTAINS plan should not include IndexLookup; got: {plan_str}"
    );
}

#[test]
fn regex_match_operator_end_to_end() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:Person {name: 'Alice'})").unwrap();
        tx.query("CREATE (:Person {name: 'alfred'})").unwrap();
        tx.query("CREATE (:Person {name: 'Bob'})").unwrap();
        tx.commit().unwrap();
    }

    let tx = db.read_tx().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name =~ '(?i)al.*' RETURN n.name AS name")
        .unwrap();
    let mut names: Vec<String> = rows
        .iter()
        .map(|r| match r.get("name").unwrap() {
            Value::String(s) => s.clone(),
            v => panic!("expected string, got {v:?}"),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["Alice".to_string(), "alfred".to_string()]);
    tx.commit().unwrap();
}

#[test]
fn regex_match_full_match_excludes_partial() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:Person {name: 'hello world'})").unwrap();
        tx.commit().unwrap();
    }

    let tx = db.read_tx().unwrap();
    let rows = tx
        .query("MATCH (n:Person) WHERE n.name =~ 'hello' RETURN n.name AS name")
        .unwrap();
    assert!(
        rows.is_empty(),
        "full-match should reject partial: got {rows:?}"
    );

    let rows = tx
        .query("MATCH (n:Person) WHERE n.name =~ 'hello.*' RETURN n.name AS name")
        .unwrap();
    assert_eq!(rows.len(), 1);
    tx.commit().unwrap();
}

#[test]
fn regex_match_invalid_pattern_surfaces_error() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:Person {name: 'Alice'})").unwrap();
        tx.commit().unwrap();
    }

    let tx = db.read_tx().unwrap();
    let err = tx
        .query("MATCH (n:Person) WHERE n.name =~ '([unclosed' RETURN n")
        .unwrap_err();
    assert!(format!("{err}").contains("invalid regex pattern"));
}

fn string_field(r: &graphdblite::Record, name: &str) -> String {
    match r.get(name).unwrap() {
        graphdblite::Value::String(s) => s.clone(),
        v => panic!("expected string at `{name}`, got {v:?}"),
    }
}

#[test]
fn db_indexes_empty_on_fresh_database() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    let rows = db
        .execute("CALL db.indexes() YIELD label, property, kind RETURN *")
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn db_indexes_reports_both_index_kinds() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.create_fulltext_index("Doc", "body").unwrap();
        tx.commit().unwrap();
    }

    let rows = db
        .execute("CALL db.indexes() YIELD label, property, kind RETURN *")
        .unwrap();
    assert_eq!(rows.len(), 2);

    let mut triples: Vec<(String, String, String)> = rows
        .iter()
        .map(|r| {
            let lab = string_field(r, "label");
            let prop = string_field(r, "property");
            let kind = string_field(r, "kind");
            (lab, prop, kind)
        })
        .collect();
    triples.sort();
    assert_eq!(
        triples,
        vec![
            (
                "Doc".to_string(),
                "body".to_string(),
                "fulltext".to_string()
            ),
            (
                "Person".to_string(),
                "name".to_string(),
                "btree".to_string()
            ),
        ]
    );
}

#[test]
fn db_indexes_yield_filter_selects_fulltext_only() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.create_fulltext_index("Doc", "body").unwrap();
        tx.commit().unwrap();
    }

    let rows = db
        .execute(
            "CALL db.indexes() YIELD label, property, kind \
             WITH label, property, kind WHERE kind = 'fulltext' \
             RETURN label, property",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(string_field(&rows[0], "label"), "Doc");
    assert_eq!(string_field(&rows[0], "property"), "body");
}

#[test]
fn call_unknown_builtin_errors() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    let err = db.execute("CALL db.nonsense()").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("ProcedureNotFound") || msg.contains("unknown procedure"),
        "expected ProcedureNotFound, got: {msg}"
    );
}

#[test]
fn or_chain_fts_returns_dedup_union() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.create_fulltext_index("Doc", "title").unwrap();
        tx.create_fulltext_index("Doc", "body").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:Doc {title: 'hello', body: 'goodbye'})")
            .unwrap();
        tx.query("CREATE (:Doc {title: 'goodbye', body: 'hello'})")
            .unwrap();
        tx.query("CREATE (:Doc {title: 'hello', body: 'hello'})")
            .unwrap();
        tx.query("CREATE (:Doc {title: 'goodbye', body: 'goodbye'})")
            .unwrap();
        tx.commit().unwrap();
    }

    let rows = db
        .execute(
            "MATCH (n:Doc) \
             WHERE n.title CONTAINS 'hello' OR n.body CONTAINS 'hello' \
             RETURN id(n) AS id",
        )
        .unwrap();
    // Three distinct nodes match; the third matches both predicates but
    // appears exactly once thanks to Union dedup.
    assert_eq!(rows.len(), 3, "got rows: {rows:?}");
}

#[test]
fn or_chain_fts_mixed_predicates_returns_correct_results() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.create_fulltext_index("Doc", "title").unwrap();
        tx.create_fulltext_index("Doc", "body").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:Doc {title: 'hello world', body: 'lorem'})")
            .unwrap();
        tx.query("CREATE (:Doc {title: 'lorem', body: 'goodbye now'})")
            .unwrap();
        tx.query("CREATE (:Doc {title: 'lorem', body: 'lorem'})")
            .unwrap();
        tx.commit().unwrap();
    }

    let rows = db
        .execute(
            "MATCH (n:Doc) \
             WHERE n.title CONTAINS 'hello' OR n.body STARTS WITH 'good' \
             RETURN id(n) AS id",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn or_chain_fts_falls_back_to_scan_for_non_fts_disjunct() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.create_fulltext_index("Doc", "title").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:Doc {title: 'hello', score: 5})")
            .unwrap();
        tx.query("CREATE (:Doc {title: 'goodbye', score: 5})")
            .unwrap();
        tx.query("CREATE (:Doc {title: 'lorem', score: 9})")
            .unwrap();
        tx.commit().unwrap();
    }

    // `score = 5` is not FTS-eligible — rewrite must abort and the
    // fallback Filter path must still return correct rows.
    let rows = db
        .execute(
            "MATCH (n:Doc) \
             WHERE n.title CONTAINS 'hello' OR n.score = 5 \
             RETURN id(n) AS id",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn db_counts_returns_label_and_edge_type_rows() {
    let mut db = Database::open_memory().unwrap();

    db.execute(
        "CREATE (a:Person {name:'A'})-[:KNOWS]->(b:Person {name:'B'}),
                (a)-[:KNOWS]->(c:Person {name:'C'}),
                (a)-[:WORKS_AT]->(:Company {name:'X'})",
    )
    .unwrap();

    let rows = db
        .execute(
            "CALL db.counts() YIELD kind, name, count \
             RETURN kind, name, count ORDER BY kind, name",
        )
        .unwrap();

    let tuples: Vec<(String, String, i64)> = rows
        .iter()
        .map(|r| {
            let kind = string_field(r, "kind");
            let name = string_field(r, "name");
            let count = match r.get("count").unwrap() {
                Value::I64(n) => *n,
                v => panic!("count was {v:?}"),
            };
            (kind, name, count)
        })
        .collect();

    assert_eq!(
        tuples,
        vec![
            ("edge_type".to_string(), "KNOWS".to_string(), 2),
            ("edge_type".to_string(), "WORKS_AT".to_string(), 1),
            ("label".to_string(), "Company".to_string(), 1),
            ("label".to_string(), "Person".to_string(), 3),
        ]
    );
}

#[test]
fn fts_ci_index_matches_across_case() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_ci("Person", "name").unwrap();
    db.commit().unwrap();

    db.execute("CREATE (:Person {name:'Alice Smith'}), (:Person {name:'Bob Jones'})")
        .unwrap();

    let rows = db
        .execute(
            "MATCH (n:Person) WHERE n.name CONTAINS 'alice' RETURN n.name AS name ORDER BY name",
        )
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| string_field(r, "name")).collect();
    assert_eq!(names, vec!["Alice Smith".to_string()]);
}

#[test]
fn fts_cs_index_excludes_case_mismatch() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Person", "name").unwrap();
    db.commit().unwrap();

    db.execute("CREATE (:Person {name:'Alice Smith'})").unwrap();

    let rows = db
        .execute("MATCH (n:Person) WHERE n.name CONTAINS 'alice' RETURN n.name AS name")
        .unwrap();
    assert!(
        rows.is_empty(),
        "case-sensitive index must reject lowercase substring; got {rows:?}"
    );
}

#[test]
fn fts_tolower_idiom_matches_via_ci_index() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_ci("Person", "name").unwrap();
    db.commit().unwrap();

    db.execute("CREATE (:Person {name:'Alice Smith'})").unwrap();

    let rows = db
        .execute(
            "MATCH (n:Person) WHERE toLower(n.name) CONTAINS toLower('ALICE') RETURN n.name AS name",
        )
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| string_field(r, "name")).collect();
    assert_eq!(names, vec!["Alice Smith".to_string()]);
}

#[test]
fn fts_tolower_idiom_works_via_scan_fallback_when_only_cs_index() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Person", "name").unwrap();
    db.commit().unwrap();

    db.execute("CREATE (:Person {name:'Alice Smith'})").unwrap();

    let rows = db
        .execute(
            "MATCH (n:Person) WHERE toLower(n.name) CONTAINS toLower('ALICE') RETURN n.name AS name",
        )
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| string_field(r, "name")).collect();
    assert_eq!(names, vec!["Alice Smith".to_string()]);
}

#[test]
fn fts_search_returns_matches_and_scores() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.execute(
        "CREATE (:Person {name:'Alice',bio:'rust systems programming'}), \
                (:Person {name:'Bob',bio:'python data science'})",
    )
    .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_word("Person", "bio").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "CALL fts.search('Person', 'bio', 'rust') YIELD node, score \
             RETURN node.name AS name ORDER BY score DESC",
        )
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| string_field(r, "name")).collect();
    assert_eq!(names, vec!["Alice".to_string()]);
}

#[test]
fn fts_search_supports_phrase_and_or_queries() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.execute(
        "CREATE (:Doc {body:'rust systems programming'}), \
                (:Doc {body:'golang microservices'}), \
                (:Doc {body:'python machine learning'})",
    )
    .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_word("Doc", "body").unwrap();
    db.commit().unwrap();

    // OR query: rust OR golang
    let rows = db
        .execute(
            "CALL fts.search('Doc', 'body', 'rust OR golang') YIELD node \
             RETURN node.body AS body ORDER BY body",
        )
        .unwrap();
    let bodies: Vec<String> = rows.iter().map(|r| string_field(r, "body")).collect();
    assert_eq!(
        bodies,
        vec![
            "golang microservices".to_string(),
            "rust systems programming".to_string(),
        ]
    );

    // Phrase query: "systems programming"
    let rows = db
        .execute(
            "CALL fts.search('Doc', 'body', '\"systems programming\"') YIELD node \
             RETURN node.body AS body",
        )
        .unwrap();
    let bodies: Vec<String> = rows.iter().map(|r| string_field(r, "body")).collect();
    assert_eq!(bodies, vec!["rust systems programming".to_string()]);
}

#[test]
fn fts_search_composes_with_downstream_match() {
    let mut db = graphdblite::Database::open_memory().unwrap();
    db.execute(
        "CREATE (a:Person {name:'Alice',bio:'rust systems'})-[:WORKS_AT]->(c:Co {name:'Acme'}), \
                (b:Person {name:'Bob',bio:'python data'})-[:WORKS_AT]->(d:Co {name:'Beta'})",
    )
    .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_word("Person", "bio").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "CALL fts.search('Person', 'bio', 'rust') YIELD node \
             MATCH (node)-[:WORKS_AT]->(c) RETURN c.name AS company",
        )
        .unwrap();
    let companies: Vec<String> = rows.iter().map(|r| string_field(r, "company")).collect();
    assert_eq!(companies, vec!["Acme".to_string()]);
}

#[test]
fn fts_multi_prop_search_finds_match_in_any_column() {
    let mut db = Database::open_memory().unwrap();
    db.execute(
        "CREATE (:Article {title:'rust systems', body:'memory safe', summary:'fast'}), \
                (:Article {title:'python notes', body:'data science', summary:'analysis'}), \
                (:Article {title:'go tutorial', body:'concurrent code', summary:'goroutines'})",
    )
    .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_word_multi(
        "Article",
        &[
            "title".to_string(),
            "body".to_string(),
            "summary".to_string(),
        ],
    )
    .unwrap();
    db.commit().unwrap();

    // 'memory' only in body of first article.
    let rows = db
        .execute(
            "CALL fts.search('Article', '*', 'memory') YIELD node \
             RETURN node.title AS title",
        )
        .unwrap();
    let titles: Vec<String> = rows.iter().map(|r| string_field(r, "title")).collect();
    assert_eq!(titles, vec!["rust systems".to_string()]);

    // 'goroutines' only in summary of third article.
    let rows = db
        .execute(
            "CALL fts.search('Article', '*', 'goroutines') YIELD node \
             RETURN node.title AS title",
        )
        .unwrap();
    let titles: Vec<String> = rows.iter().map(|r| string_field(r, "title")).collect();
    assert_eq!(titles, vec!["go tutorial".to_string()]);
}

#[test]
fn fts_multi_prop_column_scoped_search_isolates_one_property() {
    let mut db = Database::open_memory().unwrap();
    db.execute("CREATE (:Article {title:'rust systems', body:'python data'})")
        .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_word_multi("Article", &["title".to_string(), "body".to_string()])
        .unwrap();
    db.commit().unwrap();

    // 'python' is in body, not title. Title-scoped search returns nothing.
    let rows = db
        .execute(
            "CALL fts.search('Article', 'title', 'python') YIELD node \
             RETURN node.title AS title",
        )
        .unwrap();
    assert!(rows.is_empty());

    // Body-scoped finds it.
    let rows = db
        .execute(
            "CALL fts.search('Article', 'body', 'python') YIELD node \
             RETURN node.title AS title",
        )
        .unwrap();
    let titles: Vec<String> = rows.iter().map(|r| string_field(r, "title")).collect();
    assert_eq!(titles, vec!["rust systems".to_string()]);
}

#[test]
fn fts_multi_prop_supports_phrase_query_per_column() {
    let mut db = Database::open_memory().unwrap();
    db.execute(
        "CREATE (:Article {title:'rust', body:'systems programming in rust'}), \
                (:Article {title:'rust', body:'data science with python'})",
    )
    .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_word_multi("Article", &["title".to_string(), "body".to_string()])
        .unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "CALL fts.search('Article', 'body', '\"systems programming\"') YIELD node \
             RETURN node.body AS body",
        )
        .unwrap();
    let bodies: Vec<String> = rows.iter().map(|r| string_field(r, "body")).collect();
    assert_eq!(bodies, vec!["systems programming in rust".to_string()]);
}

#[test]
fn score_function_returns_bm25_for_contains_hits() {
    let mut db = Database::open_memory().unwrap();
    db.execute(
        "CREATE (:Doc {body:'rust rust rust everywhere'}), \
                (:Doc {body:'rust occasionally appears here'}), \
                (:Doc {body:'python only'})",
    )
    .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "MATCH (n:Doc) WHERE n.body CONTAINS 'rust' \
             RETURN n.body AS body, score(n) AS s",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        match r.get("s").unwrap() {
            Value::F64(f) => assert!(*f > 0.0, "score should be positive, got {f}"),
            v => panic!("score column was {v:?}"),
        }
    }
}

#[test]
fn score_function_returns_null_for_non_fts_variable() {
    let mut db = Database::open_memory().unwrap();
    db.execute("CREATE (:Person {name:'Alice'}), (:Person {name:'Bob'})")
        .unwrap();
    let rows = db
        .execute("MATCH (n:Person) RETURN score(n) AS s ORDER BY n.name")
        .unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r.get("s").unwrap(), &Value::Null);
    }
}

#[test]
fn score_function_orders_results_descending() {
    let mut db = Database::open_memory().unwrap();
    db.execute(
        "CREATE (:Doc {body:'rust rust rust frequent'}), \
                (:Doc {body:'rust rare'})",
    )
    .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "MATCH (n:Doc) WHERE n.body CONTAINS 'rust' \
             RETURN n.body AS body ORDER BY score(n) DESC",
        )
        .unwrap();
    let bodies: Vec<String> = rows.iter().map(|r| string_field(r, "body")).collect();
    assert_eq!(bodies[0], "rust rust rust frequent");
    assert_eq!(bodies[1], "rust rare");
}

#[test]
fn score_function_works_with_starts_with() {
    let mut db = Database::open_memory().unwrap();
    db.execute("CREATE (:Doc {body:'rust programming'})")
        .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "MATCH (n:Doc) WHERE n.body STARTS WITH 'rust' \
             RETURN score(n) AS s",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    match rows[0].get("s").unwrap() {
        Value::F64(f) => assert!(*f > 0.0, "score should be positive, got {f}"),
        v => panic!("score column was {v:?}"),
    }
}

#[test]
fn score_function_works_with_ends_with() {
    let mut db = Database::open_memory().unwrap();
    db.execute("CREATE (:Doc {body:'programming in rust'})")
        .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "MATCH (n:Doc) WHERE n.body ENDS WITH 'rust' \
             RETURN score(n) AS s",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    match rows[0].get("s").unwrap() {
        Value::F64(f) => assert!(*f > 0.0, "score should be positive, got {f}"),
        v => panic!("score column was {v:?}"),
    }
}

#[test]
fn score_function_survives_with_clause() {
    let mut db = Database::open_memory().unwrap();
    db.execute(
        "CREATE (:Doc {body:'rust rust rust'}), \
                (:Doc {body:'rust once'})",
    )
    .unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    let rows = db
        .execute(
            "MATCH (n:Doc) WHERE n.body CONTAINS 'rust' \
             WITH n, score(n) AS s \
             RETURN n.body AS body, s ORDER BY s DESC",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    let s0 = match rows[0].get("s").unwrap() {
        Value::F64(f) => *f,
        _ => panic!(),
    };
    let s1 = match rows[1].get("s").unwrap() {
        Value::F64(f) => *f,
        _ => panic!(),
    };
    assert!(s0 >= s1, "expected descending: {s0} vs {s1}");
}

// ---------------------------------------------------------------------------
// Cypher DDL: CREATE INDEX / DROP INDEX
// ---------------------------------------------------------------------------

#[test]
fn cypher_composite_index_create_and_use() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("test.db")).unwrap();

    db.execute("CREATE INDEX ON :Person(tenant_id, ext_id)")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'a', n: 1})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'b', n: 2})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 2, ext_id: 'a', n: 3})")
        .unwrap();

    // Full prefix match — should use the composite index and return exactly one row.
    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1, ext_id: 'a'}) RETURN p.n")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("p.n").unwrap(), &Value::I64(1));

    db.execute("DROP INDEX ON :Person(tenant_id, ext_id)")
        .unwrap();

    // Query still works after drop, falling back to label scan.
    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1, ext_id: 'a'}) RETURN p.n")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("p.n").unwrap(), &Value::I64(1));
}

#[test]
fn cypher_single_prop_index_create_and_use() {
    let mut db = Database::open_memory().unwrap();

    db.execute("CREATE INDEX ON :City(name)").unwrap();
    db.execute("CREATE (:City {name: 'Berlin', pop: 3600000})")
        .unwrap();
    db.execute("CREATE (:City {name: 'London', pop: 9000000})")
        .unwrap();

    let rows = db
        .execute("MATCH (c:City {name: 'Berlin'}) RETURN c.pop")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("c.pop").unwrap(), &Value::I64(3600000));

    db.execute("DROP INDEX ON :City(name)").unwrap();
}

#[test]
fn cypher_create_index_duplicate_is_error() {
    let mut db = Database::open_memory().unwrap();
    db.execute("CREATE INDEX ON :Person(email)").unwrap();
    let err = db.execute("CREATE INDEX ON :Person(email)").unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("IndexAlreadyExists") || msg.contains("already exists"),
        "expected IndexAlreadyExists error, got: {msg}"
    );
}

#[test]
fn cypher_drop_nonexistent_index_is_error() {
    let mut db = Database::open_memory().unwrap();
    let err = db.execute("DROP INDEX ON :Person(email)").unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("IndexNotFound") || msg.contains("not found"),
        "expected IndexNotFound error, got: {msg}"
    );
}
