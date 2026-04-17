//! Loader for named graphs. TCK scenarios sometimes begin with
//! `Given the <name> graph`; the named graph's Cypher definition lives at
//! `tests/tck/graphs/<name>.cypher` and is applied to a fresh in-memory DB.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use graphdblite::Database;

/// Open a fresh in-memory database seeded with the named graph definition.
pub fn load_named_graph(name: &str) -> Result<Database> {
    let graphs_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("tck")
        .join("graphs");

    // Named graphs live in subdirectories: graphs/<name>/<name>.cypher
    // Fall back to flat layout: graphs/<name>.cypher
    let path = {
        let nested = graphs_dir.join(name).join(format!("{name}.cypher"));
        if nested.exists() {
            nested
        } else {
            graphs_dir.join(format!("{name}.cypher"))
        }
    };

    let source = fs::read_to_string(&path)
        .with_context(|| format!("reading named graph {name}: {}", path.display()))?;

    let mut db = Database::open_memory().context("opening in-memory database")?;
    {
        let tx = db.begin_write().context("begin_write")?;
        for stmt in split_cypher_statements(&source) {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            tx.query(stmt)
                .with_context(|| format!("seeding named graph {name}: {stmt}"))?;
        }
        tx.commit().context("commit named-graph seed")?;
    }
    Ok(db)
}

/// Split a multi-statement Cypher script on `;` boundaries at top level.
///
/// TCK graph scripts are simple — one CREATE per line or semicolon-separated.
/// This splitter doesn't respect string/quote nesting because the vendored
/// scripts don't contain `;` inside string literals.
fn split_cypher_statements(source: &str) -> Vec<String> {
    source
        .split(';')
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
        .collect()
}
