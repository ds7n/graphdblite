//! Embedded graph database with a Cypher query interface, backed by SQLite.
//!
//! Single-file, zero-config, multi-process safe (WAL mode). The Cypher
//! pipeline (parser, planner, executor) is internal; downstream consumers
//! reach the database through [`Database`] (stateful API) and the
//! [`WriteTxGuard`] / [`ReadTxGuard`] RAII wrappers.
//!
//! # Quick start
//!
//! ```
//! use graphdblite::Database;
//!
//! let mut db = Database::open_memory().unwrap();
//! db.execute("CREATE (:Person {name: 'Alice'})").unwrap();
//! let rows = db.execute("MATCH (p:Person) RETURN p.name AS name").unwrap();
//! assert_eq!(rows.len(), 1);
//! ```

#![deny(missing_docs)]

pub(crate) mod cypher;
mod db;
pub(crate) mod edge;
mod id;
pub(crate) mod index;
pub(crate) mod node;
mod schema;
pub(crate) mod stats;
pub(crate) mod storage;
pub(crate) mod temporal;
mod transaction;
/// Public value, error, and identifier types used throughout the API.
pub mod types;

#[cfg(feature = "tck-support")]
#[doc(hidden)]
pub mod tck_support;

// --- Public surface ---------------------------------------------------------

pub use cypher::record::NamedRecord as Record;
pub use db::{Config, Database, SyncMode};
pub use transaction::{ReadTxGuard, WriteTxGuard};
pub use types::{
    Direction, Edge, ErrorCode, GraphError, Node, NodeId, PathValue, Properties, QueryError,
    QueryPhase, Span, Value,
};

/// Hidden re-exports for `cargo fuzz` targets in `fuzz/`. Enabled by the
/// `fuzzing` Cargo feature. Not part of the public API — no semver guarantees.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod __fuzz {
    use crate::cypher::{parser, planner};
    use crate::types::Result;

    /// Parse only — exercises the pest grammar and AST builder.
    pub fn parse(input: &str) -> Result<()> {
        parser::parse(input).map(|_| ())
    }

    /// Parse and plan against a fresh in-memory database (schema initialized,
    /// no data). Exercises the parse → plan pipeline including index-aware
    /// planner branches that need a real connection.
    pub fn parse_and_plan(input: &str) -> Result<()> {
        let stmt = parser::parse(input)?;
        let db = crate::Database::open_memory()?;
        planner::plan(db.connection(), &stmt).map(|_| ())
    }
}

/// Procedure-registration types for `CALL <name>(...)` support.
///
/// Bindings register procedure definitions on a `Registry` and pass it
/// through to the executor.
pub mod procedures {
    pub use crate::cypher::procedure::ProcParam as Param;
    pub use crate::cypher::procedure::ProcedureDef as Def;
    pub use crate::cypher::procedure::ProcedureRegistry as Registry;
}
