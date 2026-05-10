//! Cardinality estimation for the logical plan.
//!
//! Scope is intentionally minimal — two callers:
//!   1. `planner::plan_patterns` reorders multi-pattern MATCH branches by
//!      estimated cardinality (smallest first) to shrink intermediate
//!      cross-product sizes.
//!   2. `cypher::execute_cypher` / `Database::execute` format `EXPLAIN`
//!      output via `format_explain`, which annotates each plan node with
//!      its estimated row count and surfaces missing-index hints.
//!
//! The estimator is **not** consulted for join ordering, expand-direction
//! picks, or operator-shape selection. Those remain rule-based in the
//! planner. Expanding cost-driven decisions should be motivated by a
//! concrete benchmark regression — otherwise the heuristics here (fixed
//! 30% filter selectivity, 5x expand fan-out, 1000-row default label
//! count) are too coarse to make better choices than the rules do.
//!
//! Real label counts come from `crate::stats`; everything else is a
//! constant.

use rusqlite::Connection;

use crate::cypher::ast::{BinOp, Expr, ExprKind};
use crate::cypher::ir::{LogicalOp, LookupKey};
use crate::cypher::record::NamedRecord;
use crate::index;
use crate::stats;
use crate::types::Value;

/// Estimated cardinality (number of rows) for a plan operator.
#[derive(Debug, Clone)]
pub struct CostEstimate {
    pub estimated_rows: f64,
}

/// Default cardinality when no statistics are available.
const DEFAULT_LABEL_COUNT: f64 = 1000.0;

/// Default selectivity for WHERE filters (30% pass through).
const DEFAULT_FILTER_SELECTIVITY: f64 = 0.3;

/// Default fan-out for edge expansions.
const DEFAULT_EXPAND_FAN_OUT: f64 = 5.0;

/// Estimate the cardinality of a logical plan operator.
pub fn estimate(conn: &Connection, plan: &LogicalOp) -> CostEstimate {
    let rows = estimate_rows(conn, plan);
    CostEstimate {
        estimated_rows: rows,
    }
}

/// Recursively estimate the number of output rows for a plan operator.
fn estimate_rows(conn: &Connection, plan: &LogicalOp) -> f64 {
    match plan {
        LogicalOp::SingleRow => 1.0,
        LogicalOp::EmptyRow => 1.0,

        LogicalOp::Scan { label, .. } => {
            let count = stats::get_label_count(conn, label).unwrap_or(0);
            if count > 0 {
                count as f64
            } else {
                DEFAULT_LABEL_COUNT
            }
        }

        LogicalOp::IndexLookup { .. } => {
            // Exact match on index — estimate 1 row.
            1.0
        }

        LogicalOp::Expand { input, .. } => estimate_rows(conn, input) * DEFAULT_EXPAND_FAN_OUT,

        LogicalOp::CrossProduct { left, right, .. }
        | LogicalOp::CorrelatedJoin {
            input: left, right, ..
        } => estimate_rows(conn, left) * estimate_rows(conn, right),

        LogicalOp::MaterializePath { input, .. } => estimate_rows(conn, input),

        LogicalOp::Filter { input, .. } => estimate_rows(conn, input) * DEFAULT_FILTER_SELECTIVITY,

        LogicalOp::Project { input, .. } => estimate_rows(conn, input),

        LogicalOp::Aggregate {
            input, group_keys, ..
        } => {
            if group_keys.is_empty() {
                1.0 // No grouping — single aggregate row.
            } else {
                // Rough estimate: half the input rows are distinct groups.
                (estimate_rows(conn, input) * 0.5).max(1.0)
            }
        }

        LogicalOp::Distinct { input } => {
            // Assume ~half the rows are unique.
            estimate_rows(conn, input) * 0.5
        }

        LogicalOp::Sort { input, .. } => estimate_rows(conn, input),

        LogicalOp::Skip { input, count } => (estimate_rows(conn, input) - *count as f64).max(0.0),

        LogicalOp::Limit { input, count } => estimate_rows(conn, input).min(*count as f64),

        LogicalOp::Unwind { input, .. } => {
            // Assume average list length of 3.
            estimate_rows(conn, input) * 3.0
        }

        LogicalOp::LeftOuterJoin { input, right, .. } => {
            // At least as many rows as the left side.
            let left = estimate_rows(conn, input);
            let right = estimate_rows(conn, right);
            // Estimate: each left row matches ~1 right row on average.
            left.max(left.min(left * right * 0.1))
        }

        LogicalOp::ShortestPath {
            input, all_paths, ..
        } => {
            let input_rows = estimate_rows(conn, input);
            if *all_paths {
                input_rows * 2.0 // Rough: ~2 shortest paths per pair.
            } else {
                input_rows // One path per input row.
            }
        }

        LogicalOp::Call { .. } => 1.0,

        // Write operations — not relevant for cost estimation.
        LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::CreateSequence { .. }
        | LogicalOp::MatchCreate { .. }
        | LogicalOp::Delete { .. }
        | LogicalOp::SetProperty { .. }
        | LogicalOp::SetLabel { .. }
        | LogicalOp::SetProperties { .. }
        | LogicalOp::Remove { .. }
        | LogicalOp::Merge { .. }
        | LogicalOp::MatchMerge { .. } => 1.0,

        LogicalOp::Union { inputs, .. } => inputs.iter().map(|i| estimate_rows(conn, i)).sum(),
    }
}

/// Format a logical plan as EXPLAIN output records.
///
/// Returns a single record with a "plan" column containing the tree-formatted plan.
pub fn format_explain(conn: &Connection, plan: &LogicalOp) -> Vec<NamedRecord> {
    let mut lines = Vec::new();
    format_plan_tree(conn, plan, 0, &mut lines);
    let text = lines.join("\n");
    let mut rec = NamedRecord::new();
    rec.set("plan".to_string(), Value::String(text));
    vec![rec]
}

/// Recursively format a plan operator as indented tree lines.
fn format_plan_tree(conn: &Connection, plan: &LogicalOp, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let rows = estimate_rows(conn, plan);

    let desc = match plan {
        LogicalOp::Scan { label, alias } => format!("Scan :{label} AS {alias}"),
        LogicalOp::IndexLookup {
            label,
            alias,
            property,
            value,
            ..
        } => {
            let v = match value {
                LookupKey::Literal(lv) => format!("{lv:?}"),
                LookupKey::Param(name) => format!("${name}"),
            };
            format!("IndexLookup :{label}.{property} = {v} AS {alias}")
        }
        LogicalOp::Expand {
            src_alias,
            dst_alias,
            edge_types,
            min_hops,
            max_hops,
            ..
        } => {
            let et = if edge_types.is_empty() {
                "*".to_string()
            } else {
                edge_types.join("|")
            };
            format!("Expand ({src_alias})-[:{et}*{min_hops}..{max_hops}]->({dst_alias})")
        }
        LogicalOp::CrossProduct { .. } => "CrossProduct".to_string(),
        LogicalOp::CorrelatedJoin { .. } => "CorrelatedJoin".to_string(),
        LogicalOp::MaterializePath { .. } => "MaterializePath".to_string(),
        LogicalOp::Filter { .. } => "Filter".to_string(),
        LogicalOp::Project { .. } => "Project".to_string(),
        LogicalOp::Aggregate {
            group_keys,
            aggregates,
            ..
        } => {
            format!(
                "Aggregate (keys={}, aggs={})",
                group_keys.len(),
                aggregates.len()
            )
        }
        LogicalOp::Sort { .. } => "Sort".to_string(),
        LogicalOp::Distinct { .. } => "Distinct".to_string(),
        LogicalOp::Skip { count, .. } => format!("Skip {count}"),
        LogicalOp::Limit { count, .. } => format!("Limit {count}"),
        LogicalOp::ShortestPath {
            src_alias,
            dst_alias,
            path_alias,
            all_paths,
            ..
        } => {
            let fn_name = if *all_paths {
                "allShortestPaths"
            } else {
                "shortestPath"
            };
            format!("{fn_name} ({src_alias})->({dst_alias}) AS {path_alias}")
        }
        LogicalOp::LeftOuterJoin { .. } => "LeftOuterJoin".to_string(),
        LogicalOp::Unwind { alias, .. } => format!("Unwind AS {alias}"),
        LogicalOp::SingleRow => "SingleRow".to_string(),
        LogicalOp::EmptyRow => "EmptyRow".to_string(),
        LogicalOp::CreateNode { labels, alias, .. } => {
            format!(
                "CreateNode :{} AS {}",
                labels.join(":"),
                alias.as_deref().unwrap_or("_")
            )
        }
        LogicalOp::CreateEdge {
            src_alias,
            dst_alias,
            edge_type,
            ..
        } => {
            format!("CreateEdge ({src_alias})-[:{edge_type}]->({dst_alias})")
        }
        LogicalOp::CreateSequence { ops } => format!("CreateSequence ({} ops)", ops.len()),
        LogicalOp::MatchCreate { .. } => "MatchCreate".to_string(),
        LogicalOp::Delete { exprs, detach, .. } => {
            let d = if *detach { "DETACH " } else { "" };
            format!("{d}Delete {exprs:?}")
        }
        LogicalOp::SetProperty { .. } => "SetProperty".to_string(),
        LogicalOp::SetLabel {
            variable, labels, ..
        } => format!("SetLabel {variable}:{}", labels.join(":")),
        LogicalOp::SetProperties {
            variable, merge, ..
        } => {
            let mode = if *merge { "+=" } else { "=" };
            format!("SetProperties {variable} {mode}")
        }
        LogicalOp::Remove { .. } => "Remove".to_string(),
        LogicalOp::Merge { .. } => "Merge".to_string(),
        LogicalOp::MatchMerge { .. } => "MatchMerge".to_string(),
        LogicalOp::Union { inputs, all } => {
            let kind = if *all { "UNION ALL" } else { "UNION" };
            format!("{kind} ({} branches)", inputs.len())
        }
        LogicalOp::Call { procedure_name, .. } => format!("Call {procedure_name}"),
    };

    lines.push(format!("{indent}{desc} (est. {rows:.0} rows)"));

    // Index hint: when a Filter sits directly above a Scan with equality
    // predicates on properties that have no index on (label, property),
    // suggest creating one. Caught at the Filter node so the hint groups
    // with the Scan it would optimize.
    if let LogicalOp::Filter { input, predicate } = plan {
        if let LogicalOp::Scan { label, alias } = input.as_ref() {
            if !label.is_empty() {
                let indexed: Vec<String> = index::list_indexes_for_label(conn, label)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_, p)| p)
                    .collect();
                let mut suggested: Vec<String> = Vec::new();
                collect_eq_properties(predicate, alias, &mut suggested);
                suggested.retain(|p| !indexed.contains(p));
                suggested.sort();
                suggested.dedup();
                let hint_indent = "  ".repeat(depth + 1);
                for prop in suggested {
                    lines.push(format!(
                        "{hint_indent}(hint: no index on :{label}({prop}); \
                         consider `db.create_index(\"{label}\", \"{prop}\")` \
                         to turn this Scan+Filter into an IndexLookup)"
                    ));
                }
            }
        }
    }

    // Recurse into children.
    match plan {
        LogicalOp::Expand { input, .. }
        | LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Aggregate { input, .. }
        | LogicalOp::Distinct { input }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Skip { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::ShortestPath { input, .. }
        | LogicalOp::MatchCreate { input, .. }
        | LogicalOp::MatchMerge { input, .. }
        | LogicalOp::Delete { input, .. }
        | LogicalOp::SetProperty { input, .. }
        | LogicalOp::SetLabel { input, .. }
        | LogicalOp::SetProperties { input, .. }
        | LogicalOp::Remove { input, .. }
        | LogicalOp::Unwind { input, .. }
        | LogicalOp::MaterializePath { input, .. } => {
            format_plan_tree(conn, input, depth + 1, lines);
        }
        LogicalOp::CrossProduct { left, right, .. }
        | LogicalOp::CorrelatedJoin {
            input: left, right, ..
        }
        | LogicalOp::LeftOuterJoin {
            input: left, right, ..
        } => {
            format_plan_tree(conn, left, depth + 1, lines);
            format_plan_tree(conn, right, depth + 1, lines);
        }
        LogicalOp::CreateSequence { ops } => {
            for op in ops {
                format_plan_tree(conn, op, depth + 1, lines);
            }
        }
        LogicalOp::Union { inputs, .. } => {
            for input in inputs {
                format_plan_tree(conn, input, depth + 1, lines);
            }
        }
        _ => {} // Leaf nodes (Scan, IndexLookup, EmptyRow, CreateNode, etc.)
    }
}

/// Walk an equality-shaped predicate and collect property names targeted by
/// `<alias>.<prop> = <literal>` (or the symmetric `<literal> = <alias>.<prop>`),
/// recursing through `AND` so conjunctive filters all contribute. Other
/// shapes (range comparisons, OR, NOT) don't translate to a point IndexLookup
/// and are intentionally ignored.
fn collect_eq_properties(expr: &Expr, alias: &str, out: &mut Vec<String>) {
    if let ExprKind::BinaryOp { left, op, right } = &expr.kind {
        match op {
            BinOp::And => {
                collect_eq_properties(left, alias, out);
                collect_eq_properties(right, alias, out);
            }
            BinOp::Eq => {
                if let Some(prop) = property_against_literal(left, right, alias) {
                    out.push(prop);
                }
                if let Some(prop) = property_against_literal(right, left, alias) {
                    out.push(prop);
                }
            }
            _ => {}
        }
    }
}

/// If `prop_side` is `<alias>.<prop>` and `lit_side` is a literal,
/// return the property name.
fn property_against_literal(prop_side: &Expr, lit_side: &Expr, alias: &str) -> Option<String> {
    let prop = match &prop_side.kind {
        ExprKind::Property(var, prop) if var == alias => prop.clone(),
        _ => return None,
    };
    if matches!(lit_side.kind, ExprKind::Literal(_)) {
        Some(prop)
    } else {
        None
    }
}
