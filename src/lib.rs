// ───────────────────────────────────────────────────────────────────────────
// graphdblite public API surface.
//
// The Cypher pipeline (parser/planner/executor/IR/AST/eval/iter/cost) is
// `pub(crate)`: it is internal and can evolve freely. Downstream consumers
// reach the database through `Database` (stateful API) and `TxGuard` (the
// RAII Rust wrapper). See `plans/api-lockdown.md` for the full surface.
// ───────────────────────────────────────────────────────────────────────────

pub(crate) mod cypher;
mod db;
pub mod edge;
mod id;
pub mod index;
pub mod node;
mod schema;
pub(crate) mod stats;
pub(crate) mod storage;
pub mod temporal;
mod transaction;
pub mod types;

// --- Public surface ---------------------------------------------------------

pub use cypher::record::Record;
pub use db::{Config, Database, SyncMode};
pub use transaction::{ReadTransaction, TxGuard, WriteTransaction};
pub use types::{
    Direction, Edge, ErrorCode, GraphError, Node, NodeId, PathValue, Properties, QueryError,
    QueryPhase, Span, Value,
};

/// Procedure-registration types for `CALL <name>(...)` support.
///
/// Bindings register procedure definitions on a `Registry` and pass it
/// through to the executor. Names mirror the module path: prefer
/// `procedures::Registry`, `procedures::Def`, `procedures::Param` over
/// the legacy aliases.
pub mod procedures {
    pub use crate::cypher::procedure::ProcParam as Param;
    pub use crate::cypher::procedure::ProcedureDef as Def;
    pub use crate::cypher::procedure::ProcedureRegistry as Registry;
}

/// Deprecated. Use [`procedures::Registry`].
pub use cypher::procedure::ProcedureRegistry;
