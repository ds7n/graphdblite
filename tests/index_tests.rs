use std::collections::HashMap;

use graphdblite::{Database, Value};

fn props(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn create_index_and_lookup() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();

    tx.create_node("Person", props(&[("name", Value::String("Alice".into()))]))
        .unwrap();
    tx.create_node("Person", props(&[("name", Value::String("Bob".into()))]))
        .unwrap();
    tx.create_node("Person", props(&[("name", Value::String("Alice".into()))]))
        .unwrap();

    tx.create_index("Person", "name").unwrap();

    let results = tx
        .index_lookup("Person", "name", &Value::String("Alice".into()))
        .unwrap();
    assert_eq!(results.len(), 2);

    let results = tx
        .index_lookup("Person", "name", &Value::String("Bob".into()))
        .unwrap();
    assert_eq!(results.len(), 1);

    let results = tx
        .index_lookup("Person", "name", &Value::String("Charlie".into()))
        .unwrap();
    assert!(results.is_empty());

    tx.commit().unwrap();
}

#[test]
fn index_updated_on_property_change() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();

    tx.create_index("Person", "name").unwrap();

    let id = tx
        .create_node("Person", props(&[("name", Value::String("Alice".into()))]))
        .unwrap();

    let results = tx
        .index_lookup("Person", "name", &Value::String("Alice".into()))
        .unwrap();
    assert_eq!(results.len(), 1);

    // Change the name.
    tx.set_node_property(id, "name", Value::String("Alicia".into()))
        .unwrap();

    // Old value should be gone.
    let results = tx
        .index_lookup("Person", "name", &Value::String("Alice".into()))
        .unwrap();
    assert!(results.is_empty());

    // New value should be found.
    let results = tx
        .index_lookup("Person", "name", &Value::String("Alicia".into()))
        .unwrap();
    assert_eq!(results.len(), 1);

    tx.commit().unwrap();
}

#[test]
fn index_updated_on_node_delete() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();

    tx.create_index("Person", "name").unwrap();

    let id = tx
        .create_node("Person", props(&[("name", Value::String("Alice".into()))]))
        .unwrap();
    assert_eq!(
        tx.index_lookup("Person", "name", &Value::String("Alice".into()))
            .unwrap()
            .len(),
        1
    );

    tx.delete_node(id).unwrap();
    assert!(tx
        .index_lookup("Person", "name", &Value::String("Alice".into()))
        .unwrap()
        .is_empty());

    tx.commit().unwrap();
}

#[test]
fn duplicate_index_creation_fails() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.create_index("Person", "name").unwrap();
    let result = tx.create_index("Person", "name");
    assert!(result.is_err());
    tx.commit().unwrap();
}

#[test]
fn drop_index() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.begin_write().unwrap();
    tx.create_index("Person", "name").unwrap();
    tx.drop_index("Person", "name").unwrap();

    // Lookup should fail — index gone.
    let result = tx.index_lookup("Person", "name", &Value::String("x".into()));
    assert!(result.is_err());

    tx.commit().unwrap();
}
