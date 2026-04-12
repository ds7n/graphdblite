use std::collections::HashMap;

use graphdblite::{Database, NodeId, Value};

/// Helper: set up a small social graph for testing.
fn setup_social_graph() -> Database {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.begin_write().unwrap();
        tx.query("CREATE (a:Person {name: 'Alice', age: 30})").unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', age: 25})").unwrap();
        tx.query("CREATE (c:Person {name: 'Charlie', age: 35})").unwrap();
        tx.query("CREATE (d:Company {name: 'Acme'})").unwrap();
        tx.commit().unwrap();
    }
    {
        // Create edges via the typed API since CREATE with edges needs existing nodes.
        let tx = db.begin_write().unwrap();
        tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new()).unwrap();
        tx.create_edge(NodeId(2), NodeId(3), "KNOWS", HashMap::new()).unwrap();
        tx.create_edge(NodeId(1), NodeId(4), "WORKS_AT", HashMap::new()).unwrap();
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
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Bob".into()))
    );
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
    let results = tx
        .query("MATCH (n:Person) RETURN count(*) AS cnt")
        .unwrap();
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
    assert_eq!(
        results[1].get("n.name"),
        Some(&Value::String("Bob".into()))
    );
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
        tx.query("CREATE (n:Person {name: 'Dave', age: 40})").unwrap();
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
        tx.query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})").unwrap();
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
        assert_eq!(
            results[0].get("b.name"),
            Some(&Value::String("Bob".into()))
        );
        tx.commit().unwrap();
    }
}

#[test]
fn e2e_delete_node() {
    let mut db = setup_social_graph();
    {
        let tx = db.begin_write().unwrap();
        tx.query("MATCH (n:Person) WHERE n.name = 'Charlie' DELETE n").unwrap();
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
        tx.query("MATCH (n:Person) WHERE n.name = 'Alice' SET n.age = 31").unwrap();
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
        tx.query("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.source = 'created'").unwrap();
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
        tx.query("MERGE (n:Person {name: 'Alice'}) ON MATCH SET n.seen = true").unwrap();
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
    // Should include user properties via alias.prop keys.
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(30)));
    // Internal fields must NOT appear.
    assert!(results[0].get("n.__id").is_none());
    assert!(results[0].get("n.__label").is_none());
    // Bare alias (raw node ID) must NOT appear.
    assert!(results[0].get("n").is_none());
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
    // Bare variable should expand to n.prop fields.
    assert_eq!(
        results[0].get("n.name"),
        Some(&Value::String("Bob".into()))
    );
    assert_eq!(results[0].get("n.age"), Some(&Value::I64(25)));
    // No internal fields.
    assert!(results[0].get("n.__id").is_none());
    assert!(results[0].get("n.__label").is_none());
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
    // Both aliases should have their properties expanded.
    assert_eq!(
        results[0].get("a.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(
        results[0].get("b.name"),
        Some(&Value::String("Bob".into()))
    );
    // No internal fields.
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
        assert_eq!(
            results[0].get("b.name"),
            Some(&Value::String("Bob".into()))
        );
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
        tx.query(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) CREATE (b)-[:KNOWS]->(a)",
        )
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
        tx.query("CREATE (a:Person {name: 'Alice', dept: 'eng'})").unwrap();
        tx.query("CREATE (b:Person {name: 'Bob', dept: 'eng'})").unwrap();
        tx.query("CREATE (c:Person {name: 'Charlie', dept: 'sales'})").unwrap();
        tx.commit().unwrap();
    }
    let tx = db.begin_read().unwrap();
    let results = tx
        .query("MATCH (n:Person) RETURN n.dept, count(*) AS cnt ORDER BY n.dept")
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get("n.dept"), Some(&Value::String("eng".into())));
    assert_eq!(results[0].get("cnt"), Some(&Value::I64(2)));
    assert_eq!(results[1].get("n.dept"), Some(&Value::String("sales".into())));
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
    assert_eq!(results[0].get("a.name"), Some(&Value::String("Alice".into())));
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
    assert_eq!(results[0].get("a.name"), Some(&Value::String("Alice".into())));
    assert_eq!(results[0].get("c.name"), Some(&Value::String("Acme".into())));
    assert_eq!(results[1].get("a.name"), Some(&Value::String("Bob".into())));
    assert_eq!(results[1].get("c.name"), Some(&Value::Null));
    assert_eq!(results[2].get("a.name"), Some(&Value::String("Charlie".into())));
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
    assert_eq!(results[0].get("a.name"), Some(&Value::String("Alice".into())));
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
    assert_eq!(results[0].get("a.name"), Some(&Value::String("Charlie".into())));
    assert_eq!(results[0].get("c.name"), Some(&Value::Null));
    tx.commit().unwrap();
}
