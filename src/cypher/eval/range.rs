//! `range(start, end [, step])`: argument handling shared by the `range()`
//! function and by `UNWIND range(...)`, which iterates the sequence lazily
//! instead of building the whole list first.

use crate::cypher::ast::{Expr, ExprKind};
use crate::cypher::record_view::RecordView;
use crate::types::{value_type_name, ErrorCode, GraphError, QueryError, QueryPhase, Result, Value};

use super::{eval_expr, EvalCx, MAX_LIST_LEN};

fn range_error(message: String, code: ErrorCode) -> GraphError {
    GraphError::Query(QueryError::ArgumentError {
        phase: QueryPhase::Runtime,
        message,
        code,
        hint: None,
        span: None,
    })
}

/// A validated integer sequence: `len` values starting at `start`, each
/// `step` further on.
pub(crate) struct RangeSpec {
    start: i64,
    step: i64,
    len: usize,
}

impl RangeSpec {
    /// Number of values in the sequence.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The `k`-th value (`k < len`). Always fits an `i64`: it lies between
    /// `start` and the validated end.
    fn nth(&self, k: usize) -> i64 {
        (self.start as i128 + k as i128 * self.step as i128) as i64
    }

    /// Lazily yield the sequence as `Value::I64`s.
    pub(crate) fn values(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.len).map(|k| Value::I64(self.nth(k)))
    }

    /// Build the whole list, after checking it fits the query's remaining
    /// materialization budget (so a huge range fails before allocating).
    pub(crate) fn into_list(self) -> Result<Value> {
        crate::cypher::executor::check_materialized_headroom(
            (self.len as u64).saturating_mul(std::mem::size_of::<Value>() as u64),
        )?;
        Ok(Value::List(self.values().collect()))
    }
}

/// Evaluate and validate the arguments of `range(...)`. `None` means the
/// result is `null` (a null start or end, or a missing end).
pub(crate) fn range_spec(
    args: &[Expr],
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> Result<Option<RangeSpec>> {
    let start = args
        .first()
        .map(|a| eval_expr(a, record, ecx))
        .transpose()?
        .unwrap_or(Value::Null);
    let end = args.get(1).map(|a| eval_expr(a, record, ecx)).transpose()?;
    let step = args.get(2).map(|a| eval_expr(a, record, ecx)).transpose()?;
    // Validate argument types.
    for (label, val) in [("start", &start)]
        .into_iter()
        .chain(end.as_ref().map(|v| ("end", v)))
        .chain(step.as_ref().map(|v| ("step", v)))
    {
        if !matches!(val, Value::I64(_) | Value::Null) {
            return Err(range_error(
                format!(
                    "InvalidArgumentType: range() {label} argument must be an integer, got {}",
                    value_type_name(val)
                ),
                ErrorCode::Other,
            ));
        }
    }
    let (Value::I64(s), Some(Value::I64(e))) = (start, end) else {
        return Ok(None);
    };
    let step = match step {
        Some(Value::I64(st)) => {
            if st == 0 {
                return Err(range_error(
                    "NumberOutOfRange: step argument to range() cannot be zero".to_string(),
                    ErrorCode::Other,
                ));
            }
            st
        }
        _ => 1,
    };
    // Pre-compute expected length in i128 to reject huge allocations up
    // front (security finding H2). Cap at a generous 10M entries: a real
    // query producing more is almost certainly a mistake or an attack.
    const MAX_RANGE_LEN: i128 = MAX_LIST_LEN as i128;
    let len: i128 = if (step > 0 && s <= e) || (step < 0 && s >= e) {
        let span = (e as i128) - (s as i128);
        span / (step as i128) + 1
    } else {
        0
    };
    if len > MAX_RANGE_LEN {
        return Err(range_error(
            format!(
                "NumberOutOfRange: range({s}, {e}, {step}) would produce \
                 {len} elements, exceeding the {MAX_RANGE_LEN}-element cap"
            ),
            ErrorCode::NumberOutOfRange,
        ));
    }
    Ok(Some(RangeSpec {
        start: s,
        step,
        len: len as usize,
    }))
}

/// The values an `UNWIND` expression produces for one input row.
pub(crate) enum UnwindItems {
    /// An already-built list.
    List(std::vec::IntoIter<Value>),
    /// A `range()` call, generated on demand.
    Range { spec: RangeSpec, next: usize },
}

impl UnwindItems {
    /// Number of values not yet yielded.
    pub(crate) fn remaining(&self) -> usize {
        match self {
            UnwindItems::List(it) => it.len(),
            UnwindItems::Range { spec, next } => spec.len() - next,
        }
    }
}

impl Iterator for UnwindItems {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        match self {
            UnwindItems::List(it) => it.next(),
            UnwindItems::Range { spec, next } => {
                if *next >= spec.len() {
                    return None;
                }
                let v = Value::I64(spec.nth(*next));
                *next += 1;
                Some(v)
            }
        }
    }
}

/// Evaluate the expression of an `UNWIND`. `UNWIND range(...)` is kept lazy
/// so `UNWIND range(1, 5000000) AS x RETURN x LIMIT 5` does not first build
/// five million values; anything else is evaluated to a list. `null` unwinds
/// to nothing; any other non-list is a semantic error.
pub(crate) fn eval_unwind_items(
    expr: &Expr,
    record: &dyn RecordView,
    ecx: EvalCx<'_>,
) -> Result<UnwindItems> {
    if let ExprKind::FunctionCall { name, args, .. } = &expr.kind {
        if name.eq_ignore_ascii_case("range") {
            return Ok(match super::range::range_spec(args, record, ecx)? {
                Some(spec) => UnwindItems::Range { spec, next: 0 },
                None => UnwindItems::List(Vec::new().into_iter()),
            });
        }
    }
    match eval_expr(expr, record, ecx)? {
        Value::List(items) => Ok(UnwindItems::List(items.into_iter())),
        Value::Null => Ok(UnwindItems::List(Vec::new().into_iter())),
        other => Err(GraphError::semantic(format!(
            "UNWIND requires a list, got {}",
            crate::types::value_type_name(&other)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(start: i64, step: i64, len: usize) -> RangeSpec {
        RangeSpec { start, step, len }
    }

    #[test]
    fn values_follow_start_and_step() {
        let got: Vec<Value> = spec(10, -3, 4).values().collect();
        assert_eq!(
            got,
            vec![Value::I64(10), Value::I64(7), Value::I64(4), Value::I64(1)]
        );
    }

    #[test]
    fn values_do_not_overflow_at_i64_extremes() {
        let got: Vec<Value> = spec(i64::MAX - 1, 1, 2).values().collect();
        assert_eq!(got, vec![Value::I64(i64::MAX - 1), Value::I64(i64::MAX)]);
        let got: Vec<Value> = spec(i64::MIN + 1, -1, 2).values().collect();
        assert_eq!(got, vec![Value::I64(i64::MIN + 1), Value::I64(i64::MIN)]);
    }

    #[test]
    fn unwind_items_range_yields_lazily_and_counts_down() {
        let mut items = UnwindItems::Range {
            spec: spec(1, 1, 3),
            next: 0,
        };
        assert_eq!(items.remaining(), 3);
        assert_eq!(items.next(), Some(Value::I64(1)));
        assert_eq!(items.remaining(), 2);
        assert_eq!(items.next(), Some(Value::I64(2)));
        assert_eq!(items.next(), Some(Value::I64(3)));
        assert_eq!(items.next(), None);
        assert_eq!(items.remaining(), 0);
    }
}
