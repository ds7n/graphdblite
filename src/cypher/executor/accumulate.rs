//! Incremental aggregation: per-aggregate accumulators fed one row at a
//! time, and a streaming group-by built on them.
//!
//! `count`, `sum`, `avg`, `min`, `max` hold O(1) state per group, so
//! aggregating millions of rows needs memory proportional to the number of
//! groups, not rows. `collect`, `DISTINCT`, percentiles and `stDev` must
//! keep values; what they keep is charged to the query's materialization
//! budget (`Config::max_materialized_bytes`).

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::{
    eval_expr, expr_to_column_name, grouping_key_vec, into_grouping_key, EvalCx, ListBudget,
};
use crate::cypher::ir::{AggregateExpr, AggregateFunction};
use crate::cypher::record::NamedRecord;
use crate::cypher::record_v2::{Record as SlotRecord, RecordSchema, SlotId};
use crate::cypher::record_view::{RecordView, SlotView};
use crate::types::*;

use super::util::{agg_col_name, compare_values_for_sort};
use super::{approx_value_bytes, charge_materialized};

/// Bookkeeping allowance per retained group or distinct entry.
const ENTRY_BYTES: u64 = 64;

/// Running state of one aggregate function over the rows of one group.
enum AggState {
    /// `count(*)`: every row counts.
    CountStar(i64),
    /// `count(expr)`: rows where `expr` is not null (an evaluation error
    /// counts as a non-null row, as it always has).
    Count(i64),
    /// `sum`: integer sum checked for overflow, plus a float sum used once
    /// any float appears.
    Sum {
        i64_sum: Option<i64>,
        f64_sum: f64,
        all_integer: bool,
    },
    Avg {
        sum: f64,
        count: i64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
    Collect {
        items: Vec<Value>,
        budget: ListBudget,
    },
    /// `percentileDisc` / `percentileCont`: the percentile is evaluated
    /// against the first row, the numeric inputs are kept for sorting.
    Percentile {
        pct: Option<f64>,
        originals: Vec<Value>,
    },
    StDev(Vec<f64>),
}

/// Accumulator for one aggregate expression over one group.
pub(crate) struct AggAccumulator<'a> {
    agg: &'a AggregateExpr,
    state: AggState,
    /// Grouping forms of the non-null inputs seen so far (`DISTINCT` only).
    seen: Option<HashSet<Value>>,
}

impl<'a> AggAccumulator<'a> {
    pub(crate) fn new(agg: &'a AggregateExpr) -> Self {
        let star = matches!(agg.input.kind, ExprKind::Star);
        let state = match agg.function {
            AggregateFunction::Count if star => AggState::CountStar(0),
            AggregateFunction::Count => AggState::Count(0),
            AggregateFunction::Sum => AggState::Sum {
                i64_sum: Some(0),
                f64_sum: 0.0,
                all_integer: true,
            },
            AggregateFunction::Avg => AggState::Avg { sum: 0.0, count: 0 },
            AggregateFunction::Min => AggState::Min(None),
            AggregateFunction::Max => AggState::Max(None),
            AggregateFunction::Collect => AggState::Collect {
                items: Vec::new(),
                budget: ListBudget::default(),
            },
            AggregateFunction::PercentileDisc | AggregateFunction::PercentileCont => {
                AggState::Percentile {
                    pct: None,
                    originals: Vec::new(),
                }
            }
            AggregateFunction::StDev | AggregateFunction::StDevP => AggState::StDev(Vec::new()),
        };
        let seen = (agg.distinct && !star).then(HashSet::new);
        Self { agg, state, seen }
    }

    /// Feed one row.
    pub(crate) fn update(&mut self, rec: &dyn RecordView, conn: &Connection) -> Result<()> {
        let agg = self.agg;
        let eval = |rec: &dyn RecordView| eval_expr(&agg.input, rec, EvalCx::new(conn));
        // DISTINCT: only the first row of each non-null input value counts.
        let mut value = None;
        if let Some(seen) = self.seen.as_mut() {
            let val = eval(rec)?;
            if matches!(val, Value::Null) {
                return Ok(());
            }
            let key = into_grouping_key(val.clone());
            if seen.contains(&key) {
                return Ok(());
            }
            charge_materialized(approx_value_bytes(&key) + ENTRY_BYTES)?;
            seen.insert(key);
            value = Some(val);
        }
        let mut input = |rec: &dyn RecordView| match value.take() {
            Some(v) => Ok(v),
            None => eval(rec),
        };
        match &mut self.state {
            AggState::CountStar(n) => *n += 1,
            AggState::Count(n) => {
                if !matches!(input(rec), Ok(Value::Null)) {
                    *n += 1;
                }
            }
            AggState::Sum {
                i64_sum,
                f64_sum,
                all_integer,
            } => match input(rec)? {
                Value::I64(n) => {
                    *i64_sum = i64_sum.and_then(|s| s.checked_add(n));
                    *f64_sum += n as f64;
                }
                Value::F64(n) => {
                    *all_integer = false;
                    *f64_sum += n;
                }
                _ => {}
            },
            AggState::Avg { sum, count } => match input(rec)? {
                Value::I64(n) => {
                    *sum += n as f64;
                    *count += 1;
                }
                Value::F64(n) => {
                    *sum += n;
                    *count += 1;
                }
                _ => {}
            },
            AggState::Min(current) => {
                let val = input(rec)?;
                if !matches!(val, Value::Null)
                    && current.as_ref().is_none_or(|c| {
                        compare_values_for_sort(&val, c) == std::cmp::Ordering::Less
                    })
                {
                    *current = Some(val);
                }
            }
            AggState::Max(current) => {
                let val = input(rec)?;
                if !matches!(val, Value::Null)
                    && current.as_ref().is_none_or(|c| {
                        compare_values_for_sort(c, &val) == std::cmp::Ordering::Less
                    })
                {
                    *current = Some(val);
                }
            }
            AggState::Collect { items, budget } => {
                let val = input(rec)?;
                if !matches!(val, Value::Null) {
                    budget.add(&val)?;
                    charge_materialized(approx_value_bytes(&val))?;
                    items.push(val);
                }
            }
            AggState::Percentile { pct, originals } => {
                if pct.is_none() {
                    *pct = Some(eval_percentile(agg, rec, conn)?);
                }
                let val = input(rec)?;
                if matches!(val, Value::I64(_) | Value::F64(_)) {
                    charge_materialized(approx_value_bytes(&val))?;
                    originals.push(val);
                }
            }
            AggState::StDev(values) => match input(rec)? {
                Value::I64(n) => {
                    charge_materialized(8)?;
                    values.push(n as f64);
                }
                Value::F64(n) => {
                    charge_materialized(8)?;
                    values.push(n);
                }
                _ => {}
            },
        }
        Ok(())
    }

    /// The aggregate's value over the rows fed so far.
    pub(crate) fn finish(self, conn: &Connection) -> Result<Value> {
        match self.state {
            AggState::CountStar(n) | AggState::Count(n) => Ok(Value::I64(n)),
            AggState::Sum {
                i64_sum,
                f64_sum,
                all_integer,
            } => {
                if !all_integer {
                    return Ok(Value::F64(f64_sum));
                }
                // Integer sums are checked like `+`.
                i64_sum.map(Value::I64).ok_or_else(|| {
                    GraphError::number_out_of_range(
                        QueryPhase::Runtime,
                        "integer overflow in sum()",
                    )
                })
            }
            AggState::Avg { sum, count } => Ok(if count > 0 {
                Value::F64(sum / count as f64)
            } else {
                Value::Null
            }),
            AggState::Min(v) | AggState::Max(v) => Ok(v.unwrap_or(Value::Null)),
            AggState::Collect { items, .. } => Ok(Value::List(items)),
            AggState::Percentile { pct, mut originals } => {
                // No rows reached `update`: the percentile argument is
                // still validated, against an empty row.
                let pct = match pct {
                    Some(p) => p,
                    None => eval_percentile(self.agg, &NamedRecord::new(), conn)?,
                };
                if originals.is_empty() {
                    return Ok(Value::Null);
                }
                originals.sort_by(compare_values_for_sort);
                if matches!(self.agg.function, AggregateFunction::PercentileDisc) {
                    let idx = (pct * (originals.len() - 1) as f64).round() as usize;
                    return Ok(originals.swap_remove(idx));
                }
                let values: Vec<f64> = originals
                    .iter()
                    .map(|v| match v {
                        Value::I64(n) => *n as f64,
                        Value::F64(f) => *f,
                        _ => unreachable!("only numbers were collected"),
                    })
                    .collect();
                // PercentileCont: linear interpolation.
                let pos = pct * (values.len() - 1) as f64;
                let lower = pos.floor() as usize;
                let upper = pos.ceil() as usize;
                if lower == upper {
                    return Ok(Value::F64(values[lower]));
                }
                let frac = pos - lower as f64;
                Ok(Value::F64(
                    values[lower] * (1.0 - frac) + values[upper] * frac,
                ))
            }
            AggState::StDev(values) => {
                let n = values.len();
                let is_sample = matches!(self.agg.function, AggregateFunction::StDev);
                if n == 0 || (is_sample && n < 2) {
                    return Ok(Value::F64(0.0));
                }
                let mean = values.iter().sum::<f64>() / n as f64;
                let variance: f64 = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>()
                    / if is_sample { (n - 1) as f64 } else { n as f64 };
                Ok(Value::F64(variance.sqrt()))
            }
        }
    }
}

/// Evaluate and validate the percentile argument of `percentileDisc` /
/// `percentileCont` against `rec`.
fn eval_percentile(agg: &AggregateExpr, rec: &dyn RecordView, conn: &Connection) -> Result<f64> {
    let Some(pct_expr) = &agg.extra_arg else {
        return Err(GraphError::argument(
            QueryPhase::Runtime,
            "percentile function requires a second argument".to_string(),
        )
        .with_code(ErrorCode::NumberOutOfRange));
    };
    let pct = match eval_expr(pct_expr, rec, EvalCx::new(conn))? {
        Value::F64(v) => v,
        Value::I64(v) => v as f64,
        other => {
            return Err(GraphError::argument(
                QueryPhase::Runtime,
                format!("expected number but got {}", value_type_name(&other)),
            )
            .with_code(ErrorCode::NumberOutOfRange));
        }
    };
    if !(0.0..=1.0).contains(&pct) {
        return Err(GraphError::argument(
            QueryPhase::Runtime,
            format!("percentile must be between 0.0 and 1.0, got {pct}"),
        )
        .with_code(ErrorCode::NumberOutOfRange));
    }
    Ok(pct)
}

/// One group of a streaming group-by.
struct Group<'a> {
    key_vals: Vec<Value>,
    /// For a bare-variable group key, the flattened `<var>.<...>` values of
    /// the group's first row, as `(output slot, value)`.
    carried: Vec<(SlotId, Value)>,
    accs: Vec<AggAccumulator<'a>>,
}

/// Slot-native group-by that consumes rows one at a time. Same column
/// naming and group-key rules as [`super::aggregate::aggregate_named_records`]:
/// keys compare in grouping form (1 and 1.0 are one group), each group keeps
/// its first row's key values, and groups come out in first-seen order.
pub(crate) struct StreamingAggregate<'a> {
    input_schema: RecordSchema,
    output_schema: &'a RecordSchema,
    group_keys: &'a [Expr],
    aggregates: &'a [AggregateExpr],
    group_index: HashMap<Vec<Value>, usize>,
    groups: Vec<Group<'a>>,
}

impl<'a> StreamingAggregate<'a> {
    pub(crate) fn new(
        input_schema: RecordSchema,
        output_schema: &'a RecordSchema,
        group_keys: &'a [Expr],
        aggregates: &'a [AggregateExpr],
    ) -> Self {
        Self {
            input_schema,
            output_schema,
            group_keys,
            aggregates,
            group_index: HashMap::new(),
            groups: Vec::new(),
        }
    }

    fn new_group(&self, key_vals: Vec<Value>, rec: &SlotRecord) -> Group<'a> {
        let mut carried = Vec::new();
        for key_expr in self.group_keys {
            // A bare-variable key carries the variable's flattened
            // properties from a representative row so `RETURN n.name`
            // keeps working after aggregation.
            let ExprKind::Variable(var) = &key_expr.kind else {
                continue;
            };
            let prefix = format!("{var}.");
            for (in_slot, name) in self.input_schema.iter() {
                if !name.starts_with(&prefix) {
                    continue;
                }
                if let Some(out_slot) = self.output_schema.slot(name) {
                    carried.push((out_slot, rec.get(in_slot).clone()));
                }
            }
        }
        Group {
            key_vals,
            carried,
            accs: self.aggregates.iter().map(AggAccumulator::new).collect(),
        }
    }

    /// Feed one input row.
    pub(crate) fn push(&mut self, rec: &SlotRecord, conn: &Connection) -> Result<()> {
        let view = SlotView::new(&self.input_schema, rec);
        let idx = if self.group_keys.is_empty() {
            if self.groups.is_empty() {
                let group = self.new_group(Vec::new(), rec);
                self.groups.push(group);
            }
            0
        } else {
            let key_vals: Vec<Value> = self
                .group_keys
                .iter()
                .map(|k| eval_expr(k, &view, EvalCx::new(conn)))
                .collect::<Result<_>>()?;
            let norm = grouping_key_vec(&key_vals);
            match self.group_index.get(norm.as_ref()) {
                Some(&i) => i,
                None => {
                    let norm = norm.into_owned();
                    let bytes = approx_key_bytes(&norm) + approx_key_bytes(&key_vals);
                    charge_materialized(bytes + ENTRY_BYTES)?;
                    let group = self.new_group(key_vals, rec);
                    let carried: u64 = group
                        .carried
                        .iter()
                        .map(|(_, v)| approx_value_bytes(v))
                        .sum();
                    charge_materialized(carried)?;
                    self.group_index.insert(norm, self.groups.len());
                    self.groups.push(group);
                    self.groups.len() - 1
                }
            }
        };
        for acc in &mut self.groups[idx].accs {
            acc.update(&view, conn)?;
        }
        Ok(())
    }

    /// Finish every group into an output row. Global aggregation (no group
    /// keys) yields one row even when no input arrived.
    pub(crate) fn finish(mut self, conn: &Connection) -> Result<Vec<SlotRecord>> {
        if self.group_keys.is_empty() && self.groups.is_empty() {
            self.groups.push(Group {
                key_vals: Vec::new(),
                carried: Vec::new(),
                accs: self.aggregates.iter().map(AggAccumulator::new).collect(),
            });
        }
        let mut out_rows = Vec::with_capacity(self.groups.len());
        for group in self.groups {
            let mut out = SlotRecord::with_capacity(self.output_schema.len());
            for (key_expr, key_val) in self.group_keys.iter().zip(group.key_vals) {
                if let Some(slot) = self.output_schema.slot(&expr_to_column_name(key_expr)) {
                    out.set(slot, key_val);
                }
            }
            for (slot, val) in group.carried {
                out.set(slot, val);
            }
            for (agg, acc) in self.aggregates.iter().zip(group.accs) {
                let val = acc.finish(conn)?;
                if let Some(slot) = self.output_schema.slot(&agg_col_name(agg)) {
                    out.set(slot, val);
                }
            }
            out_rows.push(out);
        }
        Ok(out_rows)
    }
}

fn approx_key_bytes(vals: &[Value]) -> u64 {
    vals.iter().map(approx_value_bytes).sum()
}
