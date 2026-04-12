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
