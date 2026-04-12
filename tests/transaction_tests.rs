use std::collections::HashMap;

use graphdblite::{Database, NodeId, Value};

#[test]
fn write_transaction_commit() {
    let mut db = Database::open_memory().unwrap();

    // Create a node in a write tx, commit it.
    {
        let tx = db.begin_write().unwrap();
        tx.create_node("A", HashMap::new()).unwrap();
        tx.commit().unwrap();
    }

    // Should be visible in a new read tx.
    {
        let tx = db.begin_read().unwrap();
        assert!(tx.node_exists(NodeId(1)).unwrap());
        tx.commit().unwrap();
    }
}

#[test]
fn write_transaction_rollback() {
    let mut db = Database::open_memory().unwrap();

    // Create a node but rollback.
    {
        let tx = db.begin_write().unwrap();
        tx.create_node("A", HashMap::new()).unwrap();
        tx.rollback().unwrap();
    }

    // Should NOT be visible.
    {
        let tx = db.begin_read().unwrap();
        assert!(!tx.node_exists(NodeId(1)).unwrap());
        tx.commit().unwrap();
    }
}

#[test]
fn write_transaction_drop_rollsback() {
    let mut db = Database::open_memory().unwrap();

    // Create a node but drop the tx without commit.
    {
        let tx = db.begin_write().unwrap();
        tx.create_node("A", HashMap::new()).unwrap();
        // tx dropped here — implicit rollback
    }

    // Should NOT be visible.
    {
        let tx = db.begin_read().unwrap();
        assert!(!tx.node_exists(NodeId(1)).unwrap());
        tx.commit().unwrap();
    }
}

#[test]
fn persistent_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Create and commit.
    {
        let mut db = Database::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        tx.create_node("Person", {
            let mut m = HashMap::new();
            m.insert("name".to_string(), Value::String("Alice".into()));
            m
        }).unwrap();
        tx.commit().unwrap();
    }

    // Reopen and verify.
    {
        let mut db = Database::open(&path).unwrap();
        let tx = db.begin_read().unwrap();
        let node = tx.get_node(NodeId(1)).unwrap();
        assert_eq!(node.label, "Person");
        assert_eq!(
            node.properties.get("name"),
            Some(&Value::String("Alice".into()))
        );
        tx.commit().unwrap();
    }
}
