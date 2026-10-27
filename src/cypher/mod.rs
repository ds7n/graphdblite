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
pub(crate) mod size_estimate;

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
    let need = parser::stack_requirement(cypher);
    stacker::maybe_grow(need, need, || {
        execute_cypher_inner(conn, cypher, params, ctx, caches)
    })
}

fn execute_cypher_inner(
    conn: &Connection,
    cypher: &str,
    params: Option<&HashMap<String, Value>>,
    ctx: executor::ExecContext,
    caches: ExecCaches<'_>,
) -> Result<Vec<record::NamedRecord>> {
    #[cfg(feature = "test-hooks")]
    if cypher.trim() == "CALL graphdblite.testPanic()" {
        panic!("graphdblite test hook: deliberate panic");
    }
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
            let cookie = plan_cache::schema_cookie(conn)?;
            pc.get_or_plan_with_schema(cypher, e, cookie, || {
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
    // Callers run this inside an explicit transaction. A write statement
    // gets its own savepoint so that, if it fails part-way, only its own
    // writes are undone and the transaction stays usable (statement-level
    // atomicity). Read-only plans have nothing to undo.
    let result = if executor::is_read_only(&plan) {
        executor::execute_with_ctx(conn, &plan, &ctx)?
    } else {
        run_in_savepoint(conn, || executor::execute_with_ctx(conn, &plan, &ctx))?
    };
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

/// Savepoint name used for per-statement atomicity. Savepoints nest, so a
/// fixed name is fine even if a statement were ever run re-entrantly.
const STATEMENT_SAVEPOINT: &str = "graphdblite_stmt";

/// Run `op` inside `SAVEPOINT graphdblite_stmt`: release it on success, roll
/// back to it (then release) on failure. The caller's enclosing transaction
/// is left open either way.
///
/// If SQLite itself already rolled back the whole transaction (for example
/// on `SQLITE_FULL` or an I/O error), the savepoint is gone and the cleanup
/// statements fail; those failures are ignored so the original error is
/// reported. Callers that track transaction state resync it from
/// `Connection::is_autocommit`.
fn run_in_savepoint<T>(conn: &Connection, op: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch(&format!("SAVEPOINT {STATEMENT_SAVEPOINT}"))?;
    match op() {
        Ok(v) => {
            conn.execute_batch(&format!("RELEASE SAVEPOINT {STATEMENT_SAVEPOINT}"))?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!(
                "ROLLBACK TO SAVEPOINT {STATEMENT_SAVEPOINT}; \
                 RELEASE SAVEPOINT {STATEMENT_SAVEPOINT}"
            ));
            Err(e)
        }
    }
}
