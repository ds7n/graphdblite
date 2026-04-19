use rusqlite::Connection;

use crate::cypher::ir::LogicalOp;
use crate::cypher::record::Record;
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

        LogicalOp::CrossProduct { left, right }
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
pub fn format_explain(conn: &Connection, plan: &LogicalOp) -> Vec<Record> {
    let mut lines = Vec::new();
    format_plan_tree(conn, plan, 0, &mut lines);
    let text = lines.join("\n");
    let mut rec = Record::new();
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
            format!("IndexLookup :{label}.{property} = {value:?} AS {alias}")
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
        LogicalOp::Delete {
            variables, detach, ..
        } => {
            let d = if *detach { "DETACH " } else { "" };
            format!("{d}Delete {variables:?}")
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
    };

    lines.push(format!("{indent}{desc} (est. {rows:.0} rows)"));

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
        LogicalOp::CrossProduct { left, right }
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
