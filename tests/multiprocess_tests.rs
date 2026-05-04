use std::collections::HashMap;
use std::process::Command;

use graphdblite::{Database, Value};

/// Multi-process concurrency test.
///
/// Spawns N child processes that each write nodes to the same database file.
/// Verifies all nodes are present after all processes complete.
#[test]
fn concurrent_writers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.db");

    // Initialize the database.
    {
        let _db = Database::open(&path).unwrap();
    }

    let num_writers = 4;
    let nodes_per_writer = 25;

    // Spawn child processes. Each writes nodes_per_writer nodes.
    let mut children = Vec::new();
    for writer_id in 0..num_writers {
        let child = Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("child_writer")
            .env("GRAPHDB_TEST_PATH", path.to_str().unwrap())
            .env("GRAPHDB_WRITER_ID", writer_id.to_string())
            .env("GRAPHDB_NODES_COUNT", nodes_per_writer.to_string())
            .spawn()
            .expect("failed to spawn child process");
        children.push(child);
    }

    // Wait for all children.
    for mut child in children {
        let status = child.wait().expect("failed to wait on child");
        assert!(status.success(), "child process failed: {status}");
    }

    // Verify all nodes were written.
    let mut db = Database::open(&path).unwrap();
    let tx = db.read_tx().unwrap();
    let all_nodes = tx.find_nodes_by_label("Writer").unwrap();
    assert_eq!(
        all_nodes.len(),
        (num_writers * nodes_per_writer) as usize,
        "expected {} nodes, found {}",
        num_writers * nodes_per_writer,
        all_nodes.len()
    );
    tx.commit().unwrap();
}

/// Child process entry point for concurrent_writers test.
/// Invoked via --ignored flag so it doesn't run normally.
#[test]
#[ignore]
fn child_writer() {
    let path = match std::env::var("GRAPHDB_TEST_PATH") {
        Ok(p) => p,
        Err(_) => return, // Not a child invocation.
    };
    let writer_id: u32 = std::env::var("GRAPHDB_WRITER_ID").unwrap().parse().unwrap();
    let count: u32 = std::env::var("GRAPHDB_NODES_COUNT")
        .unwrap()
        .parse()
        .unwrap();

    let mut db = Database::open(&path).unwrap();

    for i in 0..count {
        let tx = db.write_tx().unwrap();
        tx.create_node("Writer", {
            let mut props = HashMap::new();
            props.insert("writer_id".to_string(), Value::I64(writer_id as i64));
            props.insert("seq".to_string(), Value::I64(i as i64));
            props
        })
        .unwrap();
        tx.commit().unwrap();
    }
}
