use std::collections::HashMap;

use graphdblite::{Database, Direction, Value};

fn props(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn create_edge_and_get_neighbors() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let alice = tx
        .create_node("Person", props(&[("name", Value::String("Alice".into()))]))
        .unwrap();
    let bob = tx
        .create_node("Person", props(&[("name", Value::String("Bob".into()))]))
        .unwrap();

    tx.create_edge(alice, bob, "KNOWS", HashMap::new()).unwrap();

    let out = tx
        .get_neighbors(alice, "KNOWS", Direction::Outgoing)
        .unwrap();
    assert_eq!(out, vec![bob]);

    let inc = tx.get_neighbors(bob, "KNOWS", Direction::Incoming).unwrap();
    assert_eq!(inc, vec![alice]);

    // No outgoing KNOWS from bob.
    let out_bob = tx.get_neighbors(bob, "KNOWS", Direction::Outgoing).unwrap();
    assert!(out_bob.is_empty());

    tx.commit().unwrap();
}

#[test]
fn edge_with_properties() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("A", HashMap::new()).unwrap();
    let b = tx.create_node("B", HashMap::new()).unwrap();

    tx.create_edge(a, b, "REL", props(&[("weight", Value::F64(0.5))]))
        .unwrap();

    let ep = tx.get_edge_properties(a, b, "REL").unwrap();
    assert_eq!(ep.get("weight"), Some(&Value::F64(0.5)));

    tx.commit().unwrap();
}

#[test]
fn delete_edge() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("A", HashMap::new()).unwrap();
    let b = tx.create_node("B", HashMap::new()).unwrap();

    tx.create_edge(a, b, "REL", HashMap::new()).unwrap();
    assert_eq!(
        tx.get_neighbors(a, "REL", Direction::Outgoing)
            .unwrap()
            .len(),
        1
    );

    tx.delete_edge(a, b, "REL").unwrap();
    assert!(tx
        .get_neighbors(a, "REL", Direction::Outgoing)
        .unwrap()
        .is_empty());
    assert!(tx
        .get_neighbors(b, "REL", Direction::Incoming)
        .unwrap()
        .is_empty());

    tx.commit().unwrap();
}

#[test]
fn cascading_delete_on_node_removal() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("A", HashMap::new()).unwrap();
    let b = tx.create_node("B", HashMap::new()).unwrap();
    let c = tx.create_node("C", HashMap::new()).unwrap();

    tx.create_edge(a, b, "X", HashMap::new()).unwrap();
    tx.create_edge(c, a, "Y", HashMap::new()).unwrap();

    // Delete node a — edges (a->b) and (c->a) should be removed.
    tx.delete_node(a).unwrap();

    assert!(tx
        .get_neighbors(b, "X", Direction::Incoming)
        .unwrap()
        .is_empty());
    assert!(tx
        .get_neighbors(c, "Y", Direction::Outgoing)
        .unwrap()
        .is_empty());

    tx.commit().unwrap();
}

#[test]
fn multiple_edge_labels() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("Person", HashMap::new()).unwrap();
    let b = tx.create_node("Person", HashMap::new()).unwrap();

    tx.create_edge(a, b, "KNOWS", HashMap::new()).unwrap();
    tx.create_edge(a, b, "WORKS_WITH", HashMap::new()).unwrap();

    let knows = tx.get_neighbors(a, "KNOWS", Direction::Outgoing).unwrap();
    assert_eq!(knows, vec![b]);

    let works = tx
        .get_neighbors(a, "WORKS_WITH", Direction::Outgoing)
        .unwrap();
    assert_eq!(works, vec![b]);

    tx.commit().unwrap();
}

#[test]
fn both_direction() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("A", HashMap::new()).unwrap();
    let b = tx.create_node("B", HashMap::new()).unwrap();
    let c = tx.create_node("C", HashMap::new()).unwrap();

    tx.create_edge(a, b, "E", HashMap::new()).unwrap();
    tx.create_edge(c, a, "E", HashMap::new()).unwrap();

    let both = tx.get_neighbors(a, "E", Direction::Both).unwrap();
    assert_eq!(both.len(), 2);
    assert!(both.contains(&b));
    assert!(both.contains(&c));

    tx.commit().unwrap();
}

#[test]
fn edge_to_nonexistent_node_fails() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("A", HashMap::new()).unwrap();
    let result = tx.create_edge(a, graphdblite::NodeId(999), "E", HashMap::new());
    assert!(result.is_err());

    tx.commit().unwrap();
}

#[test]
fn traverse_variable_length_path() {
    // Build a chain: a -> b -> c -> d
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("N", HashMap::new()).unwrap();
    let b = tx.create_node("N", HashMap::new()).unwrap();
    let c = tx.create_node("N", HashMap::new()).unwrap();
    let d = tx.create_node("N", HashMap::new()).unwrap();

    tx.create_edge(a, b, "NEXT", HashMap::new()).unwrap();
    tx.create_edge(b, c, "NEXT", HashMap::new()).unwrap();
    tx.create_edge(c, d, "NEXT", HashMap::new()).unwrap();

    // 1 hop from a: [b]
    let r = tx.traverse(a, "NEXT", Direction::Outgoing, 1, 1).unwrap();
    assert_eq!(r, vec![b]);

    // 1..2 hops from a: [b, c]
    let r = tx.traverse(a, "NEXT", Direction::Outgoing, 1, 2).unwrap();
    assert!(r.contains(&b));
    assert!(r.contains(&c));
    assert_eq!(r.len(), 2);

    // 1..3 hops from a: [b, c, d]
    let r = tx.traverse(a, "NEXT", Direction::Outgoing, 1, 3).unwrap();
    assert_eq!(r.len(), 3);

    // 2..3 hops from a: [c, d] (skip direct neighbor)
    let r = tx.traverse(a, "NEXT", Direction::Outgoing, 2, 3).unwrap();
    assert!(r.contains(&c));
    assert!(r.contains(&d));
    assert!(!r.contains(&b));

    tx.commit().unwrap();
}

#[test]
fn traverse_with_cycle() {
    // a -> b -> c -> a (cycle)
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("N", HashMap::new()).unwrap();
    let b = tx.create_node("N", HashMap::new()).unwrap();
    let c = tx.create_node("N", HashMap::new()).unwrap();

    tx.create_edge(a, b, "E", HashMap::new()).unwrap();
    tx.create_edge(b, c, "E", HashMap::new()).unwrap();
    tx.create_edge(c, a, "E", HashMap::new()).unwrap();

    // Should not infinite loop. 1..10 hops from a.
    let r = tx.traverse(a, "E", Direction::Outgoing, 1, 10).unwrap();
    assert_eq!(r.len(), 2); // b and c (a is start, excluded)

    tx.commit().unwrap();
}

#[test]
fn traverse_with_depth_returns_depth() {
    // Build a chain: a -> b -> c -> d
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("N", HashMap::new()).unwrap();
    let b = tx.create_node("N", HashMap::new()).unwrap();
    let c = tx.create_node("N", HashMap::new()).unwrap();
    let d = tx.create_node("N", HashMap::new()).unwrap();

    tx.create_edge(a, b, "NEXT", HashMap::new()).unwrap();
    tx.create_edge(b, c, "NEXT", HashMap::new()).unwrap();
    tx.create_edge(c, d, "NEXT", HashMap::new()).unwrap();

    // 1..3 hops from a: [(b,1), (c,2), (d,3)]
    let r = tx
        .traverse_with_depth(a, "NEXT", Direction::Outgoing, 1, 3)
        .unwrap();
    assert_eq!(r.len(), 3);
    assert!(r.contains(&(b, 1)));
    assert!(r.contains(&(c, 2)));
    assert!(r.contains(&(d, 3)));

    // 2..3 hops: [(c,2), (d,3)] — skip depth 1
    let r = tx
        .traverse_with_depth(a, "NEXT", Direction::Outgoing, 2, 3)
        .unwrap();
    assert_eq!(r.len(), 2);
    assert!(r.contains(&(c, 2)));
    assert!(r.contains(&(d, 3)));

    tx.commit().unwrap();
}

#[test]
fn traverse_with_depth_handles_cycle() {
    // a -> b -> c -> a (cycle)
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("N", HashMap::new()).unwrap();
    let b = tx.create_node("N", HashMap::new()).unwrap();
    let c = tx.create_node("N", HashMap::new()).unwrap();

    tx.create_edge(a, b, "E", HashMap::new()).unwrap();
    tx.create_edge(b, c, "E", HashMap::new()).unwrap();
    tx.create_edge(c, a, "E", HashMap::new()).unwrap();

    let r = tx
        .traverse_with_depth(a, "E", Direction::Outgoing, 1, 10)
        .unwrap();
    assert_eq!(r.len(), 2);
    assert!(r.contains(&(b, 1)));
    assert!(r.contains(&(c, 2)));

    tx.commit().unwrap();
}

// ===========================================================================
// Batch edge creation tests
// ===========================================================================

#[test]
fn batch_create_edges_basic() {
    use graphdblite::edge;

    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx
        .create_node("Node", props(&[("name", Value::String("A".into()))]))
        .unwrap();
    let b = tx
        .create_node("Node", props(&[("name", Value::String("B".into()))]))
        .unwrap();
    let c = tx
        .create_node("Node", props(&[("name", Value::String("C".into()))]))
        .unwrap();

    // Batch create edges from a to b and a to c.
    edge::batch_create_edges(
        tx.connection(),
        "KNOWS",
        &[(a, b, HashMap::new()), (a, c, HashMap::new())],
    )
    .unwrap();

    // Verify outgoing neighbors of a.
    let neighbors = tx.get_neighbors(a, "KNOWS", Direction::Outgoing).unwrap();
    assert_eq!(neighbors.len(), 2);
    assert!(neighbors.contains(&b));
    assert!(neighbors.contains(&c));

    // Verify incoming neighbors.
    let incoming_b = tx.get_neighbors(b, "KNOWS", Direction::Incoming).unwrap();
    assert!(incoming_b.contains(&a));

    tx.commit().unwrap();
}

#[test]
fn batch_create_edges_with_properties() {
    use graphdblite::edge;

    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let a = tx.create_node("Node", HashMap::new()).unwrap();
    let b = tx.create_node("Node", HashMap::new()).unwrap();

    edge::batch_create_edges(
        tx.connection(),
        "CALLS",
        &[(a, b, props(&[("line", Value::I64(42))]))],
    )
    .unwrap();

    let edge_props = tx.get_edge_properties(a, b, "CALLS").unwrap();
    assert_eq!(edge_props.get("line"), Some(&Value::I64(42)));

    tx.commit().unwrap();
}

#[test]
fn batch_create_edges_coalescing_many_from_same_source() {
    use graphdblite::edge;

    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();

    let src = tx.create_node("Node", HashMap::new()).unwrap();
    let mut targets = Vec::new();
    for _ in 0..50 {
        targets.push(tx.create_node("Node", HashMap::new()).unwrap());
    }

    let edges: Vec<_> = targets.iter().map(|&t| (src, t, HashMap::new())).collect();
    edge::batch_create_edges(tx.connection(), "E", &edges).unwrap();

    let neighbors = tx.get_neighbors(src, "E", Direction::Outgoing).unwrap();
    assert_eq!(neighbors.len(), 50);
    // Verify all targets are present.
    for t in &targets {
        assert!(neighbors.contains(t));
    }

    tx.commit().unwrap();
}

#[test]
fn batch_create_edges_matches_individual_create() {
    use graphdblite::edge;

    let mut db = Database::open_memory().unwrap();

    // Create nodes.
    let tx = db.write_tx().unwrap();
    let a = tx.create_node("Node", HashMap::new()).unwrap();
    let b = tx.create_node("Node", HashMap::new()).unwrap();
    let c = tx.create_node("Node", HashMap::new()).unwrap();
    tx.commit().unwrap();

    // Use batch API.
    let tx = db.write_tx().unwrap();
    edge::batch_create_edges(
        tx.connection(),
        "E",
        &[
            (a, b, props(&[("w", Value::I64(1))])),
            (a, c, props(&[("w", Value::I64(2))])),
            (b, c, HashMap::new()),
        ],
    )
    .unwrap();

    // Verify same as individual creation would produce.
    let a_out = tx.get_neighbors(a, "E", Direction::Outgoing).unwrap();
    assert_eq!(a_out.len(), 2);
    let b_out = tx.get_neighbors(b, "E", Direction::Outgoing).unwrap();
    assert_eq!(b_out.len(), 1);
    let c_in = tx.get_neighbors(c, "E", Direction::Incoming).unwrap();
    assert_eq!(c_in.len(), 2);

    let props_ab = tx.get_edge_properties(a, b, "E").unwrap();
    assert_eq!(props_ab.get("w"), Some(&Value::I64(1)));
    let props_ac = tx.get_edge_properties(a, c, "E").unwrap();
    assert_eq!(props_ac.get("w"), Some(&Value::I64(2)));

    tx.commit().unwrap();
}
