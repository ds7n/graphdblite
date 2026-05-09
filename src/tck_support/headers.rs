//! Per-scenario column-header capture for the column-header golden test.
//!
//! When env var `GRAPHDBLITE_HEADERS_LOG` is set to a writable path, the TCK
//! step `When executing query:` appends one line per successful query:
//!
//! ```text
//! Feature Name::Scenario Name\tcol1|col2|...
//! ```
//!
//! Errored scenarios produce no line. Successful queries with zero result
//! rows produce a `<EMPTY>` marker so the file enumerates every scenario
//! that ran. The `tests/record_columns_golden.rs` test diffs the resulting
//! file against `tests/record_columns_golden.txt`.

use crate::cypher::record::NamedRecord;
use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;

/// Lazily-opened append handle. `None` means capture is disabled (env var
/// unset). Initialized on first `log_headers` call.
static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn sink() -> Option<&'static Mutex<std::fs::File>> {
    SINK.get_or_init(|| {
        let path = std::env::var_os("GRAPHDBLITE_HEADERS_LOG")?;
        // Truncate on first use of the process so reruns are clean.
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap_or_else(|e| panic!("GRAPHDBLITE_HEADERS_LOG={path:?}: open failed: {e}"));
        Some(Mutex::new(f))
    })
    .as_ref()
}

/// Append a line for the given scenario. No-op if env var is unset.
pub fn log_headers(feature: &str, scenario: &str, records: &[NamedRecord]) {
    let Some(mu) = sink() else { return };
    let line = if records.is_empty() {
        format!("{feature}::{scenario}\t<EMPTY>\n")
    } else {
        let cols: Vec<&str> = records[0].fields.keys().map(String::as_str).collect();
        format!("{feature}::{scenario}\t{}\n", cols.join("|"))
    };
    let mut f = mu.lock().expect("headers sink mutex poisoned");
    f.write_all(line.as_bytes())
        .expect("write to GRAPHDBLITE_HEADERS_LOG failed");
}
