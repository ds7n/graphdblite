use graphdblite::{Database, NodeId};
use std::collections::HashMap;

fn main() {
    let mut db = Database::open("sample.db").expect("failed to open db");

    // Create nodes.
    let tx = db.begin_write().unwrap();
    tx.query("CREATE (a:Person {name: 'Alice', age: 30})")
        .unwrap();
    tx.query("CREATE (b:Person {name: 'Bob', age: 25})")
        .unwrap();
    tx.query("CREATE (c:Person {name: 'Charlie', age: 35})")
        .unwrap();
    tx.query("CREATE (d:Company {name: 'Acme', founded: 1995})")
        .unwrap();

    // Wire edges between existing nodes.
    tx.create_edge(NodeId(1), NodeId(2), "KNOWS", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(2), NodeId(3), "KNOWS", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(1), NodeId(4), "WORKS_AT", HashMap::new())
        .unwrap();
    tx.create_edge(NodeId(3), NodeId(4), "WORKS_AT", HashMap::new())
        .unwrap();
    tx.commit().unwrap();

    println!("Seeded sample.db with 4 nodes and 4 edges.");
}
