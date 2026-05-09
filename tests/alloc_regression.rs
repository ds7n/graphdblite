//! Allocation-count regression test (Phase 0.3 of `plans/record-v2.md`).
//!
//! Runs a fixed query set under `dhat`'s heap profiler and compares the
//! per-query allocation block count against the committed baseline at
//! `tests/alloc_baseline.txt`. Drift outside ±5% fails. `BLESS=1` rewrites
//! the baseline.
//!
//! Allocation counts are inherently sensitive to: rustc version, indexmap
//! version, hashbrown's internal growth schedule, and any change in the
//! `Record` shape — which is exactly what makes this useful as a record-v2
//! guardrail. After Phase 5 lands the slot refactor, the baseline gets
//! re-blessed and the new numbers tell us whether the refactor actually
//! shed allocations as predicted.
//!
//! Each query is wrapped in a `warmup → snapshot → measure → snapshot`
//! sequence on a freshly-built in-memory `Database`. The warmup absorbs
//! lazy initialisation (parser caches, regex compilation, etc.) so the
//! measured delta reflects steady-state alloc churn, not first-call
//! overhead.
//!
//! Run: `cargo test --test alloc_regression --features dhat-heap --release`

#![cfg(feature = "dhat-heap")]

use std::collections::BTreeMap;
use std::path::Path;

use graphdblite::Database;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const TOLERANCE: f64 = 0.05;

fn main() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let queries: Vec<(&str, fn() -> u64)> = vec![
        ("indexed_lookup", run_indexed_lookup),
        ("traversal_one_hop", run_traversal_one_hop),
        ("aggregate_group_by", run_aggregate_group_by),
        ("var_length", run_var_length),
        ("merge_pattern", run_merge_pattern),
        ("delete_many", run_delete_many),
        ("complex_project", run_complex_project),
    ];

    let mut results: BTreeMap<String, u64> = BTreeMap::new();
    for (name, f) in queries {
        let allocs = f();
        results.insert(name.to_string(), allocs);
    }

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The slot path adds materialize-and-eval conversion overhead in
    // Phase 3 — eval still operates on `NamedRecord`. Tracking this
    // against the named-path baseline produces noise (see
    // `plans/record-v2.md` "Verification strategy"), so each feature
    // state owns its own baseline. Phase 5 re-baselines once eval is
    // slot-aware and the materialize trade-off goes away.
    let baseline_name = if cfg!(feature = "record-v2") {
        "alloc_baseline_v2.txt"
    } else {
        "alloc_baseline.txt"
    };
    let baseline_path = manifest.join("tests").join(baseline_name);

    let actual = serialize(&results);

    if std::env::var_os("BLESS").is_some() {
        std::fs::write(&baseline_path, &actual).expect("write baseline");
        eprintln!(
            "alloc_regression: blessed {} ({} queries)",
            baseline_path.display(),
            results.len()
        );
        return;
    }

    let baseline_text = std::fs::read_to_string(&baseline_path).unwrap_or_else(|e| {
        panic!(
            "alloc_regression: baseline {} missing or unreadable: {e}\n\
             Run with BLESS=1 to create it.",
            baseline_path.display()
        )
    });
    let baseline = parse(&baseline_text);

    let mut failed = false;
    for (name, &actual_v) in &results {
        let Some(&base_v) = baseline.get(name) else {
            eprintln!("alloc_regression: new query {name} (not in baseline; rerun BLESS=1)");
            failed = true;
            continue;
        };
        let lo = (base_v as f64 * (1.0 - TOLERANCE)).floor() as u64;
        let hi = (base_v as f64 * (1.0 + TOLERANCE)).ceil() as u64;
        let pct = if base_v == 0 {
            0.0
        } else {
            (actual_v as i64 - base_v as i64) as f64 / base_v as f64 * 100.0
        };
        if actual_v < lo || actual_v > hi {
            eprintln!(
                "alloc_regression: {name}: {actual_v} blocks (baseline {base_v}, {pct:+.1}%) \
                 — outside ±{}% tolerance",
                (TOLERANCE * 100.0) as u32
            );
            failed = true;
        } else {
            eprintln!("alloc_regression: {name}: {actual_v} blocks ({pct:+.1}%) OK");
        }
    }
    for name in baseline.keys() {
        if !results.contains_key(name) {
            eprintln!("alloc_regression: baseline has stale query {name}");
            failed = true;
        }
    }

    if failed {
        eprintln!("alloc_regression: drift detected — investigate or rerun with BLESS=1.");
        std::process::exit(1);
    }
}

fn serialize(results: &BTreeMap<String, u64>) -> String {
    let mut s = String::new();
    for (k, v) in results {
        s.push_str(&format!("{k}={v}\n"));
    }
    s
}

fn parse(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

// ─── Per-query measurement ───────────────────────────────────────────────

/// Build an in-memory DB seeded with 50 :Person nodes (`p0`..`p49`) chained
/// via `:KNOWS` edges and an index on `:Person(name)`.
fn make_seed_db() -> Database {
    let mut db = Database::open_memory().expect("open_memory");
    {
        let tx = db.write_tx().expect("begin write for index");
        tx.create_index("Person", "name").expect("create_index");
        tx.commit().expect("commit index");
    }
    db.execute(
        "UNWIND range(0, 49) AS i \
         CREATE (:Person {name: 'p' + toString(i), age: 20 + i})",
    )
    .expect("seed people");
    db.execute(
        "MATCH (a:Person), (b:Person) \
         WHERE b.age = a.age + 1 \
         CREATE (a)-[:KNOWS]->(b)",
    )
    .expect("seed edges");
    db
}

fn measure<F: FnMut(&mut Database)>(db: &mut Database, mut run: F) -> u64 {
    run(db); // warmup
    let pre = dhat::HeapStats::get();
    run(db);
    let post = dhat::HeapStats::get();
    post.total_blocks - pre.total_blocks
}

fn run_indexed_lookup() -> u64 {
    let mut db = make_seed_db();
    measure(&mut db, |db| {
        let _ = db
            .execute("MATCH (p:Person {name: 'p25'}) RETURN p.age")
            .expect("indexed_lookup");
    })
}

fn run_traversal_one_hop() -> u64 {
    let mut db = make_seed_db();
    measure(&mut db, |db| {
        let _ = db
            .execute("MATCH (p:Person)-[:KNOWS]->(q) RETURN q.name")
            .expect("traversal_one_hop");
    })
}

fn run_aggregate_group_by() -> u64 {
    let mut db = make_seed_db();
    measure(&mut db, |db| {
        let _ = db
            .execute("MATCH (p:Person) RETURN p.age, count(*) ORDER BY p.age")
            .expect("aggregate_group_by");
    })
}

fn run_var_length() -> u64 {
    let mut db = make_seed_db();
    measure(&mut db, |db| {
        let _ = db
            .execute("MATCH (a:Person {name: 'p0'})-[:KNOWS*1..3]->(b) RETURN b.name")
            .expect("var_length");
    })
}

fn run_merge_pattern() -> u64 {
    let mut db = make_seed_db();
    // Pre-create the merge target so every measured run takes the
    // match-existing path, not the create-new path.
    db.execute("MERGE (p:Person {name: 'merge_target'}) ON CREATE SET p.age = 99 RETURN p")
        .expect("seed merge target");
    measure(&mut db, |db| {
        let _ = db
            .execute(
                "MERGE (p:Person {name: 'merge_target'}) \
                 ON CREATE SET p.age = 99 RETURN p",
            )
            .expect("merge_pattern");
    })
}

fn run_delete_many() -> u64 {
    let mut db = make_seed_db();
    // Seed two parallel batches of throwaway nodes so warmup deletes one
    // batch and the measured run deletes the other (same shape, same count).
    db.execute("UNWIND range(0, 19) AS i CREATE (:Throwaway1 {idx: i})")
        .expect("seed throwaway1");
    db.execute("UNWIND range(0, 19) AS i CREATE (:Throwaway2 {idx: i})")
        .expect("seed throwaway2");
    let _ = db
        .execute("MATCH (t:Throwaway1) DELETE t")
        .expect("warmup delete");
    let pre = dhat::HeapStats::get();
    let _ = db
        .execute("MATCH (t:Throwaway2) DELETE t")
        .expect("delete_many");
    let post = dhat::HeapStats::get();
    post.total_blocks - pre.total_blocks
}

fn run_complex_project() -> u64 {
    let mut db = make_seed_db();
    measure(&mut db, |db| {
        let _ = db
            .execute(
                "MATCH (p:Person) \
                 WITH p, p.age * 2 AS doubled \
                 WHERE doubled > 50 \
                 RETURN p.name, doubled \
                 ORDER BY doubled DESC LIMIT 10",
            )
            .expect("complex_project");
    })
}
