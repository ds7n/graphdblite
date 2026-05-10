pub mod ast;
pub mod cost;
pub mod eval;
pub mod executor;
pub mod ir;
pub mod iter;
pub mod iter_slot;
pub mod parse_cache;
pub mod parser;
pub mod planner;
pub mod procedure;
pub mod record;
pub mod record_v2;
pub mod record_view;
pub mod schema_infer;

use crate::types::{Result, Value};
use rusqlite::Connection;
use std::collections::HashMap;

/// Single shared parse → plan → execute pipeline.
///
/// Used by both the typed `ReadTransaction`/`WriteTransaction::query` methods
/// and the stateful `Database::execute` method, ensuring no behavioral drift
/// between the two paths.
pub(crate) fn execute_cypher(
    conn: &Connection,
    cypher: &str,
    params: Option<&HashMap<String, Value>>,
    ctx: executor::ExecContext,
    cache: Option<&parse_cache::ParseCache>,
) -> Result<Vec<record::NamedRecord>> {
    let stmt = match cache {
        Some(c) => c.get_or_parse(cypher)?,
        None => parser::parse(cypher)?,
    };
    // Params are no longer baked into the AST before planning. The planner
    // sees `ExprKind::Parameter` nodes and treats them as opaque "unknown
    // literal" markers (Phase 2 of plans/plan-cache.md); the executor
    // resolves them via the `ParamScope` thread-local at eval time.
    //
    // Run `resolve_params` purely for its validation side effect (errors at
    // SemanticAnalysis if a `$name` reference has no matching entry) and
    // discard the substituted AST. This preserves fail-fast on missing
    // params even when no row actually evaluates the reference.
    if let Some(p) = params {
        let _ = parser::resolve_params(&stmt, p)?;
    }
    let _scope = eval::ParamScope::enter(params);
    let plan = planner::plan_with_procedures(conn, &stmt, &ctx.procedures, params)?;
    if matches!(stmt, ast::Statement::Explain(_)) {
        return Ok(cost::format_explain(conn, &plan));
    }
    if ctx.require_read_only && !executor::is_read_only(&plan) {
        return Err(crate::types::GraphError::Transaction {
            message: "write operations are not permitted inside a read transaction".to_string(),
            hint: Some(
                "begin a write transaction with begin_write/write_tx, or remove the \
                 CREATE/SET/DELETE/MERGE/REMOVE clause"
                    .to_string(),
            ),
        });
    }
    executor::execute_with_ctx(conn, &plan, &ctx)
}
