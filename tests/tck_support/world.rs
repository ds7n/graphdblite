//! The cucumber `World` for TCK scenarios. A fresh instance is constructed per
//! scenario; it owns a disposable in-memory `Database` plus snapshot state used
//! to evaluate side-effect assertions.

use std::collections::HashMap;

use cucumber::World as CucumberWorld;
use graphdblite::{Database, GraphError, Record, Value};
use rusqlite::Connection;

/// Counts used to compute the side-effect diff that TCK scenarios assert
/// via `And the side effects should be:`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphCounts {
    pub nodes: i64,
    pub relationships: i64,
    pub labels: i64,
    pub properties: i64,
}

impl GraphCounts {
    /// Compute counts by inspecting the underlying storage tables directly.
    pub fn snapshot(conn: &Connection) -> Self {
        let nodes: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap_or(0);
        // Edges are stored in a sparse edge-index table; a cheap count works.
        let relationships: i64 = conn
            .query_row("SELECT COUNT(*) FROM edge_props", [], |r| r.get(0))
            .unwrap_or(0);
        let labels: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata WHERE key LIKE 'stats:label_count:%'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        // Count properties by iterating all node and edge property blobs.
        // NodeRecord is pub(crate), so we define a local mirror for deserialization.
        #[derive(serde::Deserialize)]
        struct NodeBlob {
            #[allow(dead_code)]
            labels: Vec<String>,
            properties: HashMap<String, graphdblite::Value>,
        }
        let node_props: i64 = {
            let mut stmt = conn.prepare("SELECT value FROM nodes").unwrap();
            let mut rows = stmt.query([]).unwrap();
            let mut count: i64 = 0;
            while let Some(row) = rows.next().unwrap() {
                let data: Vec<u8> = row.get(0).unwrap();
                if let Ok(rec) = rmp_serde::from_slice::<NodeBlob>(&data) {
                    count += rec.properties.len() as i64;
                }
            }
            count
        };
        let edge_property_count: i64 = {
            let mut stmt = conn.prepare("SELECT value FROM edge_props").unwrap();
            let mut rows = stmt.query([]).unwrap();
            let mut count: i64 = 0;
            while let Some(row) = rows.next().unwrap() {
                let data: Vec<u8> = row.get(0).unwrap();
                if let Ok(props) =
                    rmp_serde::from_slice::<HashMap<String, graphdblite::Value>>(&data)
                {
                    count += props.len() as i64;
                }
            }
            count
        };
        Self {
            nodes,
            relationships,
            labels,
            properties: node_props + edge_property_count,
        }
    }

    /// Compute the diff `other - self` (new - old) as a map of named deltas.
    pub fn delta(&self, after: &Self) -> HashMap<String, i64> {
        let mut m = HashMap::new();
        m.insert("+nodes".into(), (after.nodes - self.nodes).max(0));
        m.insert("-nodes".into(), (self.nodes - after.nodes).max(0));
        m.insert(
            "+relationships".into(),
            (after.relationships - self.relationships).max(0),
        );
        m.insert(
            "-relationships".into(),
            (self.relationships - after.relationships).max(0),
        );
        m.insert("+labels".into(), (after.labels - self.labels).max(0));
        m.insert("-labels".into(), (self.labels - after.labels).max(0));
        m.insert(
            "+properties".into(),
            (after.properties - self.properties).max(0),
        );
        m.insert(
            "-properties".into(),
            (self.properties - after.properties).max(0),
        );
        m
    }
}

/// Per-scenario state for cucumber. Cucumber constructs one instance per
/// scenario via `Default::default()` and passes a `&mut` into each step.
#[derive(CucumberWorld, Default)]
#[world(init = Self::default)]
pub struct World {
    /// The database under test. `None` before `Given any graph` fires.
    pub db: Option<Database>,
    /// Result from the most recent `When executing query:` step.
    pub last_result: Option<Vec<Record>>,
    /// Error from the most recent `When executing query:` step, if it failed.
    pub last_error: Option<GraphError>,
    /// Parameters accumulated via `And parameters are:` before the query runs.
    pub params: HashMap<String, Value>,
    /// Graph counts snapshot captured *before* the query executes, used to
    /// compute side-effect deltas post-execution.
    pub pre_counts: GraphCounts,
}

impl std::fmt::Debug for World {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("World")
            .field("db", &self.db.as_ref().map(|_| "<Database>"))
            .field("last_result", &self.last_result)
            .field("last_error", &self.last_error)
            .field("params", &self.params)
            .field("pre_counts", &self.pre_counts)
            .finish()
    }
}
