#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz the Cypher parser. Errors are expected; only panics (e.g. unexpected
// pest pair shapes, integer overflows in span arithmetic, unwraps on
// missing AST children) constitute a failure.
fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else { return };
    let _ = graphdblite::__fuzz::parse(input);
});
