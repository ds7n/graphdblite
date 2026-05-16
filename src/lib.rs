// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ds7n

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
mod id;
mod schema;
pub(crate) mod stats;
pub(crate) mod storage;
pub(crate) mod temporal;
mod transaction;

// Existing crate-internal callers refer to `crate::node`, `crate::edge`,
// `crate::index`. The primitives now live under `storage/`; re-export here
// so the move stays contained to the mod files (no churn in callers).
pub(crate) use storage::{edge, index, node};
// Value, error, and identifier types used throughout the API. Module is
// crate-internal; the stable surface is the re-exports below.
pub(crate) mod types;

#[cfg(feature = "tck-support")]
#[doc(hidden)]
pub mod tck_support;

// --- Public surface ---------------------------------------------------------

pub use cypher::record::NamedRecord as Record;
pub use db::{Config, Database, SyncMode};
pub use transaction::{ReadTxGuard, WriteTxGuard};
pub use types::{
    Direction, Edge, ErrorCode, GraphError, Node, NodeId, PathValue, Properties, QueryError,
    QueryPhase, Result, Span, Value,
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

// Procedure-registration types for `CALL <name>(...)` support are not part
// of the public API in 0.1.0. The TCK harness reaches them via
// `crate::cypher::procedure` directly. Will be revisited when a first-party
// binding has a concrete use case.
