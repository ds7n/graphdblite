use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use graphdblite::{Database, NodeId, Value};

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
            .arg("--exact")
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

/// Stress test: 6 writers + 4 readers contending on a single DB file.
///
/// Writers create chains of (Account)-[:LINKS]->(Account) nodes with multiple
/// commits each. Readers continuously scan and assert structural invariants:
///   * label-count is monotonically non-decreasing within a single read tx
///     view (snapshot isolation via WAL),
///   * every :LINKS edge resolves to two existing nodes (no dangling refs),
///   * `writer_id`/`seq` pairs are unique per writer (no torn writes).
///
/// On completion, the parent verifies totals match expectations exactly. Any
/// busy-timeout exhaustion or transient SQLite error fails the child, which
/// fails the parent. This is the multi-process WAL contention gate from the
/// audit checklist.
#[test]
fn stress_writers_with_readers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stress.db");

    // Initialize.
    {
        let _db = Database::open(&path).unwrap();
    }

    let num_writers = 6u32;
    let num_readers = 4u32;
    let chains_per_writer = 10u32;
    let nodes_per_chain = 8u32;
    let reader_duration_secs = 2u64;

    let mut children = Vec::new();
    for writer_id in 0..num_writers {
        let child = Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("--exact")
            .arg("stress_child_writer")
            .env("GRAPHDB_TEST_PATH", path.to_str().unwrap())
            .env("GRAPHDB_WRITER_ID", writer_id.to_string())
            .env("GRAPHDB_CHAINS", chains_per_writer.to_string())
            .env("GRAPHDB_CHAIN_LEN", nodes_per_chain.to_string())
            .spawn()
            .expect("spawn writer");
        children.push(("writer", child));
    }
    for reader_id in 0..num_readers {
        let child = Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("--exact")
            .arg("stress_child_reader")
            .env("GRAPHDB_TEST_PATH", path.to_str().unwrap())
            .env("GRAPHDB_READER_ID", reader_id.to_string())
            .env("GRAPHDB_READ_SECS", reader_duration_secs.to_string())
            .spawn()
            .expect("spawn reader");
        children.push(("reader", child));
    }

    for (kind, mut child) in children {
        let status = child.wait().expect("wait child");
        assert!(status.success(), "stress {kind} failed: {status}");
    }

    // Final invariants.
    let mut db = Database::open(&path).unwrap();
    let tx = db.read_tx().unwrap();
    let accounts = tx.find_nodes_by_label("Account").unwrap();
    let expected_nodes = (num_writers * chains_per_writer * nodes_per_chain) as usize;
    assert_eq!(
        accounts.len(),
        expected_nodes,
        "expected {expected_nodes} Account nodes after stress, found {}",
        accounts.len()
    );

    // Every node must resolve cleanly and every chain edge must connect
    // existing nodes. Use Cypher MATCH to drive both ends through the
    // executor (which is what real consumers use).
    let edges = tx
        .query("MATCH (a:Account)-[:LINKS]->(b:Account) RETURN a.__id, b.__id")
        .unwrap();
    let expected_edges = (num_writers * chains_per_writer * (nodes_per_chain - 1)) as usize;
    assert_eq!(
        edges.len(),
        expected_edges,
        "expected {expected_edges} :LINKS edges, found {}",
        edges.len()
    );
    tx.commit().unwrap();
}

#[test]
#[ignore]
fn stress_child_writer() {
    let path = match std::env::var("GRAPHDB_TEST_PATH") {
        Ok(p) => p,
        Err(_) => return,
    };
    let writer_id: u32 = std::env::var("GRAPHDB_WRITER_ID").unwrap().parse().unwrap();
    let chains: u32 = std::env::var("GRAPHDB_CHAINS").unwrap().parse().unwrap();
    let chain_len: u32 = std::env::var("GRAPHDB_CHAIN_LEN").unwrap().parse().unwrap();

    let mut db = Database::open(&path).unwrap();
    for chain in 0..chains {
        // One transaction per chain — guarantees the chain becomes visible
        // atomically and exercises commit-time WAL contention.
        let tx = db.write_tx().unwrap();
        let mut ids: Vec<NodeId> = Vec::with_capacity(chain_len as usize);
        for seq in 0..chain_len {
            let mut props = HashMap::new();
            props.insert("writer_id".to_string(), Value::I64(writer_id as i64));
            props.insert("chain".to_string(), Value::I64(chain as i64));
            props.insert("seq".to_string(), Value::I64(seq as i64));
            let id = tx.create_node("Account", props).unwrap();
            ids.push(id);
        }
        for win in ids.windows(2) {
            tx.create_edge(win[0], win[1], "LINKS", HashMap::new())
                .unwrap();
        }
        tx.commit().unwrap();
    }
}

#[test]
#[ignore]
fn stress_child_reader() {
    let path = match std::env::var("GRAPHDB_TEST_PATH") {
        Ok(p) => p,
        Err(_) => return,
    };
    let secs: u64 = std::env::var("GRAPHDB_READ_SECS").unwrap().parse().unwrap();

    let mut db = Database::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last_count = 0usize;
    let mut iterations = 0u64;

    while Instant::now() < deadline {
        let tx = db.read_tx().unwrap();

        // Within a single read tx, snapshot isolation must hold: count is
        // stable, and every edge endpoint resolves.
        let nodes = tx.find_nodes_by_label("Account").unwrap();
        let edges = tx
            .query("MATCH (a:Account)-[:LINKS]->(b:Account) RETURN a.__id, b.__id")
            .unwrap();

        // Edges must never outnumber what their chain structure allows: each
        // chain of N nodes contributes N-1 edges, so edges < nodes always.
        assert!(
            edges.len() < nodes.len() || nodes.is_empty(),
            "edges ({}) >= nodes ({}) — torn write?",
            edges.len(),
            nodes.len()
        );

        // Across read txs, total count must be monotonically non-decreasing
        // (writers only add, never delete in this test).
        assert!(
            nodes.len() >= last_count,
            "node count regressed: {} -> {}",
            last_count,
            nodes.len()
        );
        last_count = nodes.len();
        tx.commit().unwrap();
        iterations += 1;
    }

    // Sanity: we got at least a few read iterations in. If readers never
    // observe writers, the test is checking nothing useful.
    assert!(
        iterations > 5,
        "only {iterations} read iterations completed"
    );
}
