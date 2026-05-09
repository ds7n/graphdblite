//! CALL procedure execution.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::eval_expr;
use crate::cypher::record::NamedRecord;
use crate::types::*;

use super::*;

pub(in crate::cypher::executor) fn exec_call(
    conn: &Connection,
    input: &LogicalOp,
    procedure_name: &str,
    args: &[Expr],
    yield_items: &[(String, Option<String>)],
    _yield_star: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;

    let proc_def = ctx.procedures.get(procedure_name).ok_or_else(|| {
        GraphError::Query(crate::types::QueryError::ProcedureError {
            phase: crate::types::QueryPhase::Runtime,
            message: format!("ProcedureNotFound: unknown procedure `{procedure_name}`"),
            code: ErrorCode::Other,
            hint: None,
            span: None,
        })
    })?;

    let mut results = Vec::new();

    for rec in &records {
        // Evaluate argument expressions.
        let mut eval_args = Vec::new();
        for arg in args {
            eval_args.push(eval_expr(arg, rec, conn)?);
        }

        // Filter procedure data rows by matching input values.
        let matching_rows: Vec<_> = if proc_def.inputs.is_empty() || eval_args.is_empty() {
            proc_def.rows.iter().collect()
        } else {
            proc_def
                .rows
                .iter()
                .filter(|row| {
                    proc_def
                        .inputs
                        .iter()
                        .zip(&eval_args)
                        .all(|(param, arg_val)| match row.get(&param.name) {
                            Some(row_val) => values_match(row_val, arg_val),
                            None => true,
                        })
                })
                .collect()
        };

        if yield_items.is_empty() {
            // No columns to yield. For standalone CALL, this produces an empty result.
            // For in-query CALL (multi-clause), the rows pass through unchanged.
            // In multi-clause context, the input records carry bindings from prior clauses.
            // We check if the input record has any bindings: if yes, it's in-query context.
            if !rec.fields.is_empty() {
                results.push(rec.clone());
            }
            // Otherwise, standalone CALL with no outputs → produce no rows.
        } else if matching_rows.is_empty() {
            // Procedure has outputs but no matching rows — produce no rows.
        } else {
            for data_row in &matching_rows {
                let mut new_rec = rec.clone();
                for (col, alias) in yield_items {
                    let bind_name = alias.as_ref().unwrap_or(col);
                    if let Some(val) = data_row.get(col) {
                        new_rec.set(bind_name.clone(), val.clone());
                    } else {
                        new_rec.set(bind_name.clone(), Value::Null);
                    }
                }
                results.push(new_rec);
            }
        }
    }

    check_row_limit(&results, ctx)?;
    Ok(results)
}

/// Compare two values for procedure row filtering, with numeric coercion.
pub(in crate::cypher::executor) fn values_match(row_val: &Value, arg_val: &Value) -> bool {
    match (row_val, arg_val) {
        (Value::I64(a), Value::I64(b)) => a == b,
        (Value::F64(a), Value::F64(b)) => a == b,
        (Value::I64(a), Value::F64(b)) => (*a as f64) == *b,
        (Value::F64(a), Value::I64(b)) => *a == (*b as f64),
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Null, Value::Null) => true,
        _ => row_val == arg_val,
    }
}
