#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz parse → plan against a fresh in-memory database. Catches planner
// panics on parseable-but-degenerate ASTs (e.g. invariant assertions, type
// validation walks, name-resolution unwraps). Errors are expected and
// ignored; only panics fail the run.
fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else { return };
    let _ = graphdblite::__fuzz::parse_and_plan(input);
});
