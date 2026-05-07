//! Column-header golden test (Phase 0.2 of `plans/record-v2.md`).
//!
//! Runs the openCypher TCK feature suite identically to `tests/tck.rs` but
//! sets `GRAPHDBLITE_HEADERS_LOG` to a tempfile so the harness records one
//! line per scenario:
//!
//! ```text
//! Feature::Scenario\tcol1|col2|...
//! ```
//!
//! After the run, the captured lines are sorted (cucumber executes
//! scenarios in nondeterministic order) and compared byte-for-byte against
//! the committed golden at `tests/record_columns_golden.txt`.
//!
//! Pass `BLESS=1` to overwrite the golden instead of comparing — required
//! when an intentional column-name change lands. Reviewers should treat
//! any golden diff as a deliberate header change and inspect carefully.

use std::collections::HashSet;
use std::path::Path;

use cucumber::writer::Stats;
use graphdblite::tck_support::world::World;

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let base = manifest.join("tests").join("tck");
    let features_dir = base.join("features");
    let golden_path = manifest.join("tests").join("record_columns_golden.txt");

    if !features_dir.exists() {
        eprintln!(
            "record_columns_golden: feature dir not found at {} — skipping",
            features_dir.display()
        );
        return;
    }

    let skiplist = load_skiplist(&base.join("skiplist.txt"));

    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    std::env::set_var("GRAPHDBLITE_HEADERS_LOG", tmp.path());

    let writer = pollster::block_on(
        <World as cucumber::World>::cucumber()
            .before(|feat, _rule, scenario, world| {
                world.current_feature = feat.name.clone();
                world.current_scenario = scenario.name.clone();
                Box::pin(async {})
            })
            .filter_run(&features_dir, move |feat, _rule, scenario| {
                let key = format!("{}::{}", feat.name, scenario.name);
                !skiplist.contains(key.as_str())
            }),
    );

    if writer.failed_steps() > 0 || writer.hook_errors() > 0 {
        eprintln!(
            "record_columns_golden: TCK failures present ({} failed steps, {} hook errors) — \
             refusing to update golden. Fix TCK first.",
            writer.failed_steps(),
            writer.hook_errors(),
        );
        std::process::exit(1);
    }

    // Collect, sort, dedupe.
    let captured = std::fs::read_to_string(tmp.path()).expect("read header log");
    let mut lines: Vec<&str> = captured.lines().filter(|l| !l.is_empty()).collect();
    lines.sort_unstable();
    lines.dedup();
    let actual = lines.join("\n") + "\n";

    if std::env::var_os("BLESS").is_some() {
        std::fs::write(&golden_path, &actual).expect("write golden");
        eprintln!(
            "record_columns_golden: blessed {} ({} lines)",
            golden_path.display(),
            lines.len()
        );
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
        panic!(
            "record_columns_golden: golden file {} missing or unreadable: {e}\n\
             Run with BLESS=1 to create it.",
            golden_path.display()
        )
    });

    if actual != expected {
        // Print a small diff context.
        let actual_set: HashSet<&str> = actual.lines().collect();
        let expected_set: HashSet<&str> = expected.lines().collect();
        let only_in_actual: Vec<&&str> = actual_set.difference(&expected_set).take(20).collect();
        let only_in_golden: Vec<&&str> = expected_set.difference(&actual_set).take(20).collect();
        eprintln!("record_columns_golden: drift detected vs {}", golden_path.display());
        eprintln!("  lines only in actual ({} shown):", only_in_actual.len());
        for l in &only_in_actual {
            eprintln!("    + {l}");
        }
        eprintln!("  lines only in golden ({} shown):", only_in_golden.len());
        for l in &only_in_golden {
            eprintln!("    - {l}");
        }
        eprintln!("If intentional, rerun with BLESS=1.");
        std::process::exit(1);
    }
    eprintln!(
        "record_columns_golden: OK ({} scenarios match)",
        lines.len()
    );
}

fn load_skiplist(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}
