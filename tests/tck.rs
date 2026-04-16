//! openCypher TCK (Technology Compatibility Kit) conformance harness.
//!
//! Feature files under `tests/tck/features/` are vendored from the openCypher
//! project (Apache-2.0, see `tests/tck/NOTICE` and `tests/tck/VERSION`). Each
//! scenario exercises graphdblite through its public query API and compares
//! results against the expected openCypher semantics.
//!
//! # Exit status
//!
//! During Phase 3 rollout the harness **always exits 0**, even if scenarios
//! fail. Pass-rate is informational at this stage; the harness is meant to
//! surface bugs and drive fixes, not to gate CI on a moving target. Once the
//! skiplist lands (Phase 4), this flips to failing on any non-skiplisted
//! scenario — turning the TCK into a proper regression gate.

mod tck_support;

use tck_support::world::World;

fn main() {
    // Path to the vendored TCK feature subset.
    let features_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("tck")
        .join("features");

    if !features_dir.exists() {
        eprintln!(
            "TCK: feature directory not found at {} — skipping harness run",
            features_dir.display()
        );
        return;
    }

    // cucumber-rs is async; pollster provides a lightweight blocking executor.
    // We intentionally exit 0 regardless of scenario pass/fail during Phase 3.
    let runner = <World as cucumber::World>::cucumber();
    pollster::block_on(runner.run(features_dir));
}
