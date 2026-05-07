//! openCypher TCK (Technology Compatibility Kit) conformance harness.
//!
//! Feature files under `tests/tck/features/` are vendored from the openCypher
//! project (Apache-2.0, see `tests/tck/NOTICE` and `tests/tck/VERSION`). Each
//! scenario exercises graphdblite through its public query API and compares
//! results against the expected openCypher semantics.
//!
//! # Skiplist
//!
//! `tests/tck/skiplist.txt` lists known-failing scenarios. Each line is
//! `Feature Name::Scenario Name` (e.g. `Create1 - Creating nodes::[1] Create
//! a single node`). The harness skips these and exits non-zero only if a
//! *non-skiplisted* scenario fails — making the TCK a regression gate.

use std::collections::HashSet;
use std::path::Path;

use cucumber::writer::Stats;
use graphdblite::tck_support::world::World;

fn main() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("tck");
    let features_dir = base.join("features");

    if !features_dir.exists() {
        eprintln!(
            "TCK: feature directory not found at {} — skipping harness run",
            features_dir.display()
        );
        return;
    }

    let skiplist = load_skiplist(&base.join("skiplist.txt"));
    let skip_count = skiplist.len();

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

    if skip_count > 0 {
        eprintln!("TCK: {skip_count} scenarios skipped via skiplist");
    }

    // Exit non-zero on step failures but not on parsing errors (which the
    // skiplist cannot prevent — they happen before scenario filtering).
    if writer.failed_steps() > 0 || writer.hook_errors() > 0 {
        std::process::exit(1);
    }
}

/// Load scenario keys to skip from a text file.
///
/// Format: one `Feature Name::Scenario Name` per line. Lines starting
/// with `#` and blank lines are ignored.
fn load_skiplist(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}
