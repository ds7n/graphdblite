//! Mapping between openCypher error type names (as they appear in TCK
//! scenarios, e.g. `SyntaxError`, `TypeError`) and graphdblite's structured
//! `QueryError` variants.
//!
//! TCK `Then` steps look like:
//!
//! ```text
//! Then a SyntaxError should be raised at runtime: InvalidArgumentExpression
//! ```
//!
//! The harness uses this map to verify the kind + phase match exactly — no
//! string matching on error messages required.

use graphdblite::{GraphError, QueryError, QueryPhase};

/// Check whether an actual `GraphError` matches the expected openCypher
/// (kind, phase) pair declared in a scenario.
pub fn matches_tck_error(
    actual: &GraphError,
    expected_kind: &str,
    expected_phase: Option<&str>,
) -> bool {
    let query_err = match actual {
        GraphError::Query(q) => q,
        _ => return false,
    };

    if !kind_matches(query_err, expected_kind) {
        return false;
    }

    if let Some(phase_str) = expected_phase {
        return phase_matches(query_err.phase(), phase_str);
    }

    true
}

fn kind_matches(err: &QueryError, expected: &str) -> bool {
    err.kind().eq_ignore_ascii_case(expected)
}

fn phase_matches(phase: QueryPhase, expected: &str) -> bool {
    // openCypher TCK uses lower-case phase names like "runtime", "parse",
    // "semantic analysis" / "semanticAnalysis".
    let normalized_expected = expected.trim().to_ascii_lowercase().replace([' ', '-'], "");
    let phase_str = match phase {
        QueryPhase::Parse => "parse",
        QueryPhase::SemanticAnalysis => "semanticanalysis",
        QueryPhase::Runtime => "runtime",
    };
    normalized_expected == phase_str
}
