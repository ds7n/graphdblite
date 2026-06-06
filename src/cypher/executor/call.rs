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

    use crate::cypher::builtin_procedures;
    use crate::cypher::procedure::ProcParam;

    // Resolve the procedure: registry first (test-only mocks may shadow
    // built-ins by name), then built-in. Registry procedures carry their
    // own row table; built-ins are invoked per outer record with the
    // evaluated args, since arg-parameterized built-ins (e.g. fts.search)
    // produce different rows per call.
    enum ProcSource<'a> {
        Registry(&'a crate::cypher::procedure::ProcedureDef),
        Builtin(builtin_procedures::BuiltinSig),
    }

    let source: ProcSource<'_> = match ctx.procedures.get(procedure_name) {
        Some(def) => ProcSource::Registry(def),
        None => match builtin_procedures::builtin_signature(procedure_name) {
            Some(sig) => ProcSource::Builtin(sig),
            None => {
                return Err(GraphError::Query(
                    crate::types::QueryError::ProcedureError {
                        phase: crate::types::QueryPhase::Runtime,
                        message: format!("ProcedureNotFound: unknown procedure `{procedure_name}`"),
                        code: ErrorCode::Other,
                        hint: None,
                        span: None,
                    },
                ));
            }
        },
    };

    let mut results = Vec::new();

    for rec in &records {
        // Evaluate argument expressions.
        let mut eval_args = Vec::new();
        for arg in args {
            eval_args.push(eval_expr(arg, rec, crate::cypher::eval::EvalCx::new(conn))?);
        }

        // Resolve the per-record row set + input spec for filtering.
        type ProcRows<'a> = std::borrow::Cow<'a, [std::collections::HashMap<String, Value>]>;
        let (proc_inputs, data_rows): (&[ProcParam], ProcRows<'_>) = match &source {
            ProcSource::Registry(def) => (
                def.inputs.as_slice(),
                std::borrow::Cow::Borrowed(def.rows.as_slice()),
            ),
            ProcSource::Builtin(sig) => {
                let rows = builtin_procedures::execute_builtin(procedure_name, &eval_args, conn)?;
                (sig.inputs.as_slice(), std::borrow::Cow::Owned(rows))
            }
        };

        // Built-ins produce rows already parameterized by `eval_args`, so
        // skip the input-match filter; registry rows are static and must
        // be filtered.
        let matching_rows: Vec<&std::collections::HashMap<String, Value>> = match &source {
            ProcSource::Builtin(_) => data_rows.iter().collect(),
            ProcSource::Registry(_) => {
                if proc_inputs.is_empty() || eval_args.is_empty() {
                    data_rows.iter().collect()
                } else {
                    data_rows
                        .iter()
                        .filter(|row| {
                            proc_inputs.iter().zip(&eval_args).all(|(param, arg_val)| {
                                match row.get(&param.name) {
                                    Some(row_val) => values_match(row_val, arg_val),
                                    None => true,
                                }
                            })
                        })
                        .collect()
                }
            }
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

#[cfg(test)]
mod tests {
    use crate::Database;

    #[test]
    fn call_db_indexes_returns_rows_for_each_index() {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.create_index("Person", "name").unwrap();
            tx.create_fulltext_index("Doc", "body").unwrap();
            tx.commit().unwrap();
        }

        let rows = db
            .execute("CALL db.indexes() YIELD label, property, kind RETURN *")
            .unwrap();
        assert_eq!(rows.len(), 2);
    }
}
