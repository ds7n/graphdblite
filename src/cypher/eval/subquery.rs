//! EXISTS predicates and EXISTS-subquery evaluation.

use crate::cypher::ast::*;
use crate::cypher::eval::EvalCx;
use crate::cypher::record_view::{view_to_named, RecordView};
use crate::types::Value;

pub(in crate::cypher::eval) fn eval_exists(
    patterns: &[crate::cypher::ast::Pattern],
    where_clause: Option<&Expr>,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<Value> {
    let conn = ecx.conn;
    use crate::cypher::executor::execute_first_match;
    use crate::cypher::ir::LogicalOp;
    use crate::cypher::planner::plan_patterns;

    // Plan the subquery patterns into a scan/expand chain.
    let mut op = plan_patterns(conn, patterns)?;

    // Apply the optional WHERE filter from within the EXISTS block.
    if let Some(predicate) = where_clause {
        op = LogicalOp::Filter {
            input: Box::new(op),
            predicate: predicate.clone(),
        };
    }

    // Extract correlated bindings from the outer record (bare alias keys, no dots).
    let mut outer_bindings: Vec<(String, Value)> = Vec::new();
    record.for_each_field(&mut |k, v| {
        if !k.contains('.') {
            outer_bindings.push((k.to_string(), v.clone()));
        }
    });

    // Short-circuit: return true on the first matching row.
    let found = execute_first_match(conn, &op, &outer_bindings)?;
    Ok(Value::Bool(found))
}

/// Evaluate an EXISTS { MATCH ... [WITH ...] [RETURN ...] } full subquery predicate.
/// Plans the inner statement and executes it as a correlated subquery using the
/// outer record's bindings. Returns true if at least one row is produced.
pub(in crate::cypher::eval) fn eval_exists_subquery(
    stmt: &crate::cypher::ast::Statement,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> crate::types::Result<Value> {
    let conn = ecx.conn;
    use crate::cypher::executor::exec_correlated_exists;
    use crate::cypher::planner::plan_subquery;

    let base_plan = plan_subquery(conn, stmt)?;

    // Execute the subquery as a correlated subquery, pushing the outer
    // record's bindings down so nested expressions can see them. The
    // correlated executor still operates on `NamedRecord`, so we pay
    // a single materialization at this boundary (rare path).
    let outer = view_to_named(record);
    let found = exec_correlated_exists(conn, &base_plan, &outer)?;
    Ok(Value::Bool(found))
}
