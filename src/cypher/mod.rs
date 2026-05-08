pub mod ast;
pub mod cost;
pub mod eval;
pub mod executor;
pub mod ir;
pub mod iter;
pub mod parse_cache;
pub mod parser;
pub mod planner;
pub mod procedure;
pub mod record;
pub mod record_v2;
pub mod row_sink;
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
    let mut stmt = match cache {
        Some(c) => c.get_or_parse(cypher)?,
        None => parser::parse(cypher)?,
    };
    if let Some(p) = params {
        stmt = parser::resolve_params(&stmt, p)?;
    }
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
