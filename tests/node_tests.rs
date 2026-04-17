use std::collections::HashMap;

use graphdblite::{Database, NodeId, Value};

fn props(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn create_and_get_node() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    let id = tx
        .create_node("Person", props(&[("name", Value::String("Alice".into()))]))
        .unwrap();
    let node = tx.get_node(id).unwrap();
    assert_eq!(node.labels, vec!["Person".to_string()]);
    assert_eq!(
        node.properties.get("name"),
        Some(&Value::String("Alice".into()))
    );
    tx.commit().unwrap();
}

#[test]
fn node_ids_are_sequential() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    let id1 = tx.create_node("A", HashMap::new()).unwrap();
    let id2 = tx.create_node("B", HashMap::new()).unwrap();
    let id3 = tx.create_node("C", HashMap::new()).unwrap();
    assert_eq!(id1, NodeId(1));
    assert_eq!(id2, NodeId(2));
    assert_eq!(id3, NodeId(3));
    tx.commit().unwrap();
}

#[test]
fn get_nonexistent_node() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_read().unwrap();
    let result = tx.get_node(NodeId(999));
    assert!(result.is_err());
    tx.commit().unwrap();
}

#[test]
fn delete_node() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    let id = tx.create_node("Person", HashMap::new()).unwrap();
    assert!(tx.node_exists(id).unwrap());
    tx.delete_node(id).unwrap();
    assert!(!tx.node_exists(id).unwrap());
    tx.commit().unwrap();
}

#[test]
fn set_and_remove_property() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    let id = tx.create_node("Person", HashMap::new()).unwrap();

    tx.set_node_property(id, "age", Value::I64(30)).unwrap();
    let node = tx.get_node(id).unwrap();
    assert_eq!(node.properties.get("age"), Some(&Value::I64(30)));

    tx.remove_node_property(id, "age").unwrap();
    let node = tx.get_node(id).unwrap();
    assert!(!node.properties.contains_key("age"));

    tx.commit().unwrap();
}

#[test]
fn find_nodes_by_label() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.create_node("Person", props(&[("name", Value::String("Alice".into()))]))
        .unwrap();
    tx.create_node("Person", props(&[("name", Value::String("Bob".into()))]))
        .unwrap();
    tx.create_node("Company", props(&[("name", Value::String("Acme".into()))]))
        .unwrap();

    let people = tx.find_nodes_by_label("Person").unwrap();
    assert_eq!(people.len(), 2);

    let companies = tx.find_nodes_by_label("Company").unwrap();
    assert_eq!(companies.len(), 1);

    tx.commit().unwrap();
}
