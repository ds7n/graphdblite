// ───────────────────────────────────────────────────────────────────────────
// graphdblite public API surface.
//
// The Cypher pipeline (parser/planner/executor/IR/AST/eval/iter/cost) is
// `pub(crate)`: it is internal and can evolve freely. Downstream consumers
// reach the database through `Database` (stateful API) and the
// `WriteTxGuard` / `ReadTxGuard` RAII wrappers. See `plans/api-lockdown.md`
// for the full surface.
// ───────────────────────────────────────────────────────────────────────────

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
pub mod types;

#[cfg(feature = "tck-support")]
#[doc(hidden)]
pub mod tck_support;

// --- Public surface ---------------------------------------------------------

pub use cypher::record::Record;
pub use db::{Config, Database, SyncMode};
pub use transaction::{ReadTxGuard, WriteTxGuard};
pub use types::{
    Direction, Edge, ErrorCode, GraphError, Node, NodeId, PathValue, Properties, QueryError,
    QueryPhase, Span, Value,
};

/// Procedure-registration types for `CALL <name>(...)` support.
///
/// Bindings register procedure definitions on a `Registry` and pass it
/// through to the executor.
pub mod procedures {
    pub use crate::cypher::procedure::ProcParam as Param;
    pub use crate::cypher::procedure::ProcedureDef as Def;
    pub use crate::cypher::procedure::ProcedureRegistry as Registry;
}
