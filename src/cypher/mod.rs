pub mod ast;
pub(crate) mod builtin_procedures;
pub mod cost;
pub mod eval;
pub mod executor;
pub mod ir;
pub mod iter;
pub mod iter_slot;
pub mod parse_cache;
pub mod parser;
pub mod plan_cache;
pub mod planner;
pub mod procedure;
pub mod record;
pub mod record_v2;
pub mod record_view;
pub mod schema_infer;

use crate::types::{Result, Value};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Caches threaded through `execute_cypher`. All fields are optional so
/// callers without a `Database` (planner test harness, fuzzer) can opt out.
#[derive(Default, Clone, Copy)]
pub(crate) struct ExecCaches<'a> {
    pub parse: Option<&'a parse_cache::ParseCache>,
    pub plan: Option<&'a plan_cache::PlanCache>,
    pub schema_epoch: Option<&'a AtomicU64>,
}

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
    caches: ExecCaches<'_>,
) -> Result<Vec<record::NamedRecord>> {
    let stmt = match caches.parse {
        Some(c) => c.get_or_parse(cypher)?,
        None => parser::parse(cypher)?,
    };
    // Params are no longer baked into the AST before planning. The planner
    // sees `ExprKind::Parameter` nodes and treats them as opaque "unknown
    // literal" markers (Phase 2 of plans/plan-cache.md); the executor
    // resolves them via the `ParamScope` thread-local at eval time.
    //
    // Validate that every `$name` reference has a matching entry in `params`.
    // No-alloc walker — preserves fail-fast on missing params even when no row
    // actually evaluates the reference.
    if let Some(p) = params {
        parser::validate_params(&stmt, p)?;
    }
    let _scope = eval::ParamScope::enter(params);
    let _regex_scope = eval::RegexCacheScope::enter(&ctx.regex_cache);
    let plan = match (caches.plan, caches.schema_epoch) {
        (Some(pc), Some(epoch)) if plan_cache::is_plan_cacheable(&stmt) => {
            let e = epoch.load(Ordering::Acquire);
            pc.get_or_plan(cypher, e, || {
                planner::plan_with_procedures(conn, &stmt, &ctx.procedures, params)
            })?
        }
        _ => planner::plan_with_procedures(conn, &stmt, &ctx.procedures, params)?,
    };
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
    let is_ddl = matches!(
        plan,
        ir::LogicalOp::CreateIndex { .. } | ir::LogicalOp::DropIndex { .. }
    );
    let result = executor::execute_with_ctx(conn, &plan, &ctx)?;
    // Bump schema epoch after successful DDL so that subsequent queries see
    // an invalidated plan cache (matching the behaviour of the Rust API methods
    // WriteTransaction::create_composite_index / drop_composite_index).
    if is_ddl {
        if let Some(epoch) = caches.schema_epoch {
            epoch.fetch_add(1, Ordering::AcqRel);
        }
    }
    Ok(result)
}
