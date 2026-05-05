//! Performance baselines for the Cypher pipeline and storage layer.
//!
//! Run with: `cargo bench --bench cypher`
//! Filter:   `cargo bench --bench cypher -- node_create`
//!
//! Each group seeds a fresh on-disk database in a tempdir so results are
//! comparable to real workloads (memory-only DBs hide WAL/fsync costs).

use std::collections::HashMap;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use graphdblite::{Database, NodeId, Value};
use tempfile::TempDir;

/// Seed `n` :Person nodes with `name` (unique) and `age` (mod 100), plus a
/// :KNOWS chain so every node has at least one outgoing edge. Returns the
/// tempdir (kept alive for the benchmark) and an opened Database handle.
fn seed_chain(n: usize, indexed: bool) -> (TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench.db");
    let mut db = Database::open(&path).expect("open db");

    {
        let tx = db.write_tx().expect("write tx");
        if indexed {
            tx.create_index("Person", "name").expect("create index");
        }
        for i in 0..n {
            let mut props = HashMap::new();
            props.insert("name".to_string(), Value::String(format!("p{i}")));
            props.insert("age".to_string(), Value::I64((i % 100) as i64));
            tx.create_node("Person", props).expect("create node");
        }
        // Chain: p0 -KNOWS-> p1 -KNOWS-> p2 ... so 1-hop and var-length both
        // have something to traverse.
        for i in 0..n.saturating_sub(1) {
            tx.create_edge(
                NodeId((i + 1) as u64),
                NodeId((i + 2) as u64),
                "KNOWS",
                HashMap::new(),
            )
            .expect("create edge");
        }
        tx.commit().expect("commit");
    }
    (dir, db)
}

fn bench_node_create(c: &mut Criterion) {
    let mut group = c.benchmark_group("node_create");
    for &batch in &[1usize, 100, 1000] {
        group.throughput(Throughput::Elements(batch as u64));
        group.bench_with_input(BenchmarkId::from_parameter(batch), &batch, |b, &batch| {
            b.iter_with_setup(
                || {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("bench.db");
                    let db = Database::open(&path).unwrap();
                    (dir, db)
                },
                |(dir, mut db)| {
                    let tx = db.write_tx().unwrap();
                    for i in 0..batch {
                        let mut props = HashMap::new();
                        props.insert("name".to_string(), Value::String(format!("p{i}")));
                        tx.create_node("Person", props).unwrap();
                    }
                    tx.commit().unwrap();
                    drop(db);
                    drop(dir);
                },
            );
        });
    }
    group.finish();
}

fn bench_lookup(c: &mut Criterion) {
    let n = 10_000;
    let (_dir_idx, mut db_idx) = seed_chain(n, true);
    let (_dir_scan, mut db_scan) = seed_chain(n, false);

    let mut group = c.benchmark_group("lookup_by_property");
    group.throughput(Throughput::Elements(1));

    group.bench_function("indexed", |b| {
        b.iter(|| {
            let rows = db_idx
                .execute("MATCH (p:Person {name: 'p5000'}) RETURN p.age")
                .unwrap();
            assert_eq!(rows.len(), 1);
        });
    });

    group.bench_function("scan", |b| {
        b.iter(|| {
            let rows = db_scan
                .execute("MATCH (p:Person {name: 'p5000'}) RETURN p.age")
                .unwrap();
            assert_eq!(rows.len(), 1);
        });
    });

    group.finish();
}

fn bench_traversal(c: &mut Criterion) {
    let n = 5_000;
    let (_dir, mut db) = seed_chain(n, true);

    let mut group = c.benchmark_group("traversal");
    group.throughput(Throughput::Elements(1));

    group.bench_function("one_hop", |b| {
        b.iter(|| {
            let rows = db
                .execute("MATCH (p:Person {name: 'p100'})-[:KNOWS]->(q) RETURN q.name")
                .unwrap();
            assert_eq!(rows.len(), 1);
        });
    });

    group.bench_function("var_length_1_to_3", |b| {
        b.iter(|| {
            let rows = db
                .execute("MATCH (p:Person {name: 'p100'})-[:KNOWS*1..3]->(q) RETURN q.name")
                .unwrap();
            assert_eq!(rows.len(), 3);
        });
    });

    group.bench_function("var_length_1_to_5", |b| {
        b.iter(|| {
            let rows = db
                .execute("MATCH (p:Person {name: 'p100'})-[:KNOWS*1..5]->(q) RETURN q.name")
                .unwrap();
            assert_eq!(rows.len(), 5);
        });
    });

    group.finish();
}

fn bench_aggregate(c: &mut Criterion) {
    let n = 10_000;
    let (_dir, mut db) = seed_chain(n, true);

    let mut group = c.benchmark_group("aggregate");
    group.throughput(Throughput::Elements(n as u64));

    group.bench_function("count_star", |b| {
        b.iter(|| {
            let rows = db.execute("MATCH (p:Person) RETURN count(*)").unwrap();
            assert_eq!(rows.len(), 1);
        });
    });

    group.bench_function("group_by_age", |b| {
        b.iter(|| {
            let rows = db
                .execute("MATCH (p:Person) RETURN p.age, count(*) ORDER BY p.age")
                .unwrap();
            assert_eq!(rows.len(), 100);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_node_create,
    bench_lookup,
    bench_traversal,
    bench_aggregate
);
criterion_main!(benches);
