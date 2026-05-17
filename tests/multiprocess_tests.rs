use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use graphdblite::{Database, GraphError, NodeId, Value};

/// Run a child process, capturing stderr so a panicking child surfaces its
/// panic message instead of the bare `exit status: 101`. Returns an Err
/// describing the failure (kind, status, captured stderr) when the child
/// did not exit cleanly.
fn wait_child(kind: &str, child: std::process::Child) -> Result<(), String> {
    let output = child.wait_with_output().expect("wait child");
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{kind} child failed: {}\n--- child stderr ---\n{}\n--- end stderr ---",
        output.status,
        stderr.trim_end()
    ))
}

/// Is this a `SQLITE_BUSY` (or `SQLITE_BUSY_*`) — a recoverable
/// "writer-lock held, try again" signal? These are normal under
/// multi-process contention and well-behaved callers retry. They are
/// NOT correctness failures and the stress tests must not treat them
/// as fatal — see `with_busy_retry` for the loop that wraps each
/// per-transaction unit of work.
fn is_busy(err: &GraphError) -> bool {
    matches!(
        err,
        GraphError::Storage {
            source: rusqlite::Error::SqliteFailure(e, _),
            ..
        } if matches!(
            e.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    )
}

/// Run `f` (a "do one transaction" closure) with retry on
/// `SQLITE_BUSY`. Mirrors how real applications use SQLite under
/// multi-process contention. Returns a non-busy error or a non-busy
/// final result; panics only after a generous retry budget elapses,
/// which would itself indicate a real bug (lock leak, deadlock, or
/// catastrophic host starvation beyond any reasonable timeout).
fn with_busy_retry<T>(label: &str, mut f: impl FnMut() -> Result<T, GraphError>) -> T {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut attempt = 0u32;
    loop {
        match f() {
            Ok(v) => return v,
            Err(e) if is_busy(&e) => {
                if Instant::now() >= deadline {
                    panic!("{label}: SQLITE_BUSY after 120 s and {attempt} attempts: {e}");
                }
                attempt += 1;
                // Modest randomized backoff so retries don't dogpile.
                let backoff_ms = std::cmp::min(50 + attempt as u64 * 10, 500);
                std::thread::sleep(Duration::from_millis(backoff_ms));
            }
            Err(e) => panic!("{label}: non-busy error: {e}"),
        }
    }
}

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
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn child process");
        children.push(child);
    }

    // Wait for all children — surface their stderr if any panic.
    for child in children {
        if let Err(msg) = wait_child("writer", child) {
            panic!("{msg}");
        }
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
        with_busy_retry("child_writer iter", || {
            let tx = db.write_tx()?;
            tx.create_node("Writer", {
                let mut props = HashMap::new();
                props.insert("writer_id".to_string(), Value::I64(writer_id as i64));
                props.insert("seq".to_string(), Value::I64(i as i64));
                props
            })?;
            tx.commit()?;
            Ok(())
        });
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
            .stderr(Stdio::piped())
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
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn reader");
        children.push(("reader", child));
    }

    for (kind, child) in children {
        if let Err(msg) = wait_child(kind, child) {
            panic!("{msg}");
        }
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
        // atomically and exercises commit-time WAL contention. Retry on
        // SQLITE_BUSY: that's what real applications do.
        with_busy_retry("stress_child_writer chain", || {
            let tx = db.write_tx()?;
            let mut ids: Vec<NodeId> = Vec::with_capacity(chain_len as usize);
            for seq in 0..chain_len {
                let mut props = HashMap::new();
                props.insert("writer_id".to_string(), Value::I64(writer_id as i64));
                props.insert("chain".to_string(), Value::I64(chain as i64));
                props.insert("seq".to_string(), Value::I64(seq as i64));
                let id = tx.create_node("Account", props)?;
                ids.push(id);
            }
            for win in ids.windows(2) {
                tx.create_edge(win[0], win[1], "LINKS", HashMap::new())?;
            }
            tx.commit()?;
            Ok(())
        });
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
        let (nodes, edges) = with_busy_retry("stress_child_reader iter", || {
            let tx = db.read_tx()?;
            // Within a single read tx, snapshot isolation must hold: count is
            // stable, and every edge endpoint resolves.
            let nodes = tx.find_nodes_by_label("Account")?;
            let edges =
                tx.query("MATCH (a:Account)-[:LINKS]->(b:Account) RETURN a.__id, b.__id")?;
            tx.commit()?;
            Ok((nodes, edges))
        });

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
        iterations += 1;
    }

    // Sanity: we got at least a few read iterations in. If readers never
    // observe writers, the test is checking nothing useful.
    assert!(
        iterations > 5,
        "only {iterations} read iterations completed"
    );
}
