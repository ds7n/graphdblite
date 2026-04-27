//! The cucumber `World` for TCK scenarios. A fresh instance is constructed per
//! scenario; it owns a disposable in-memory `Database` plus snapshot state used
//! to evaluate side-effect assertions.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use cucumber::World as CucumberWorld;
use graphdblite::{Database, GraphError, Record, Value};
use rusqlite::Connection;

/// Hash a Value for property fingerprinting (used in side-effect diffing).
fn hash_value(v: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut hasher);
    hasher.finish()
}

/// Counts used to compute the side-effect diff that TCK scenarios assert
/// via `And the side effects should be:`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphCounts {
    pub nodes: i64,
    pub relationships: i64,
    pub labels: i64,
    /// Set of node storage keys — used to compute gross +/-nodes when
    /// a query both creates and deletes nodes in the same pipeline.
    pub node_keys: HashSet<Vec<u8>>,
    /// Set of edge storage keys — used to compute gross +/-relationships.
    pub edge_keys: HashSet<Vec<u8>>,
    /// Set of (owner_key, prop_name, value_hash) tuples — tracks individual
    /// property VALUES so that `SET n.x = 2` (was 1) registers as +1/-1.
    pub property_fingerprints: HashSet<(String, String, u64)>,
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
        // Collect node/edge keys and property fingerprints.
        #[derive(serde::Deserialize)]
        struct NodeBlob {
            #[allow(dead_code)]
            labels: Vec<String>,
            properties: HashMap<String, graphdblite::Value>,
        }
        let mut fingerprints = HashSet::new();
        let mut node_keys = HashSet::new();
        let mut edge_keys = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT key, value FROM nodes").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let key: Vec<u8> = row.get(0).unwrap();
                node_keys.insert(key.clone());
                let owner = format!("n:{key:?}");
                let data: Vec<u8> = row.get(1).unwrap();
                if let Ok(rec) = rmp_serde::from_slice::<NodeBlob>(&data) {
                    for (k, v) in &rec.properties {
                        fingerprints.insert((owner.clone(), k.clone(), hash_value(v)));
                    }
                }
            }
        }
        {
            let mut stmt = conn.prepare("SELECT key, value FROM edge_props").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let key: Vec<u8> = row.get(0).unwrap();
                edge_keys.insert(key.clone());
                let owner = format!("e:{key:?}");
                let data: Vec<u8> = row.get(1).unwrap();
                if let Ok(props) =
                    rmp_serde::from_slice::<HashMap<String, graphdblite::Value>>(&data)
                {
                    for (k, v) in &props {
                        fingerprints.insert((owner.clone(), k.clone(), hash_value(v)));
                    }
                }
            }
        }
        Self {
            nodes,
            relationships,
            labels,
            node_keys,
            edge_keys,
            property_fingerprints: fingerprints,
        }
    }

    /// Compute the diff `other - self` (new - old) as a map of named deltas.
    ///
    /// Uses set-based diffing for nodes and relationships so that queries
    /// that both create and delete in the same pipeline (e.g., DELETE + MERGE)
    /// correctly report gross additions and removals, not just the net delta.
    pub fn delta(&self, after: &Self) -> HashMap<String, i64> {
        let mut m = HashMap::new();
        // Nodes: keys in after but not before = created; keys in before but not after = deleted.
        let nodes_added = after.node_keys.difference(&self.node_keys).count() as i64;
        let nodes_removed = self.node_keys.difference(&after.node_keys).count() as i64;
        m.insert("+nodes".into(), nodes_added);
        m.insert("-nodes".into(), nodes_removed);
        // Relationships: same approach with edge keys.
        let rels_added = after.edge_keys.difference(&self.edge_keys).count() as i64;
        let rels_removed = self.edge_keys.difference(&after.edge_keys).count() as i64;
        m.insert("+relationships".into(), rels_added);
        m.insert("-relationships".into(), rels_removed);
        m.insert("+labels".into(), (after.labels - self.labels).max(0));
        m.insert("-labels".into(), (self.labels - after.labels).max(0));
        // Properties: count fingerprints added/removed (catches value changes).
        let added = after
            .property_fingerprints
            .difference(&self.property_fingerprints)
            .count() as i64;
        let removed = self
            .property_fingerprints
            .difference(&after.property_fingerprints)
            .count() as i64;
        m.insert("+properties".into(), added);
        m.insert("-properties".into(), removed);
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
