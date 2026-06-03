//! Slot-schema inference for [`LogicalOp`] plans.
//!
//! `infer_schema(op)` returns the [`RecordSchema`] of names a downstream
//! consumer of `op` may legally read. Built as part of record-v2 phase 2 —
//! pure analysis, no executor wiring yet. The output is a *declared* schema
//! based on:
//!
//! - the metadata cluster every node/edge alias publishes (`a`, `a.__id`,
//!   `a.__label`, `a.__labels` for nodes; `r`, `r.__src`, `r.__dst`,
//!   `r.__type`, `r.__edge_seq` for edges),
//! - every `alias.prop` reference found anywhere in the plan (collected once
//!   up front so a `Scan` over `(p:Person)` knows which properties to slot
//!   even when the references live inside a downstream `Filter` or `Project`).
//!
//! This corresponds to the "(a) reserve a slot for every property name
//! mentioned anywhere in the query" recommendation in `plans/record-v2.md`
//! open question #1.
//!
//! Phase 3 will consume this schema in the executor; right now it only
//! drives the unit tests in this module.

#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};

use crate::cypher::ast::{Expr, ExprKind, Pattern, PatternElement, ReturnItem, SetItem, SortItem};
use crate::cypher::eval::expr_to_column_name;
use crate::cypher::ir::{AggregateExpr, LogicalOp};
use crate::cypher::record_v2::RecordSchema;

/// Per-alias set of property names referenced anywhere in a plan.
///
/// Built by [`collect_property_refs`] and consumed by
/// [`infer_with_props`] so that `Scan { alias }` reserves slots for every
/// property a downstream operator may read, even when the references live
/// in subtrees above the scan.
pub type PropertyRefs = HashMap<String, BTreeSet<String>>;

/// Compute the output [`RecordSchema`] of `op`'s pipeline.
///
/// For consistency when inferring schemas across a plan tree, prefer
/// [`collect_property_refs`] + [`infer_with_props`] so a single pre-pass
/// sees the whole plan's references and every subtree's schema agrees on
/// which property slots to reserve.
pub fn infer_schema(op: &LogicalOp) -> RecordSchema {
    let mut props = PropertyRefs::new();
    collect_property_refs(op, &mut props);
    infer_with_props(op, &props)
}

// ---------------------------------------------------------------------------
// Schema inference

/// Infer the output schema of `op`, using a pre-computed [`PropertyRefs`]
/// so subtree-level inference still slots properties referenced by
/// ancestors.
pub fn infer_with_props(op: &LogicalOp, props: &HashMap<String, BTreeSet<String>>) -> RecordSchema {
    use LogicalOp::*;

    match op {
        SingleRow | EmptyRow => RecordSchema::new(),

        Scan { alias, .. } => {
            let mut s = RecordSchema::new();
            add_node_metadata(&mut s, alias);
            add_referenced_props(&mut s, alias, props);
            s
        }

        IndexLookup { alias, .. } => {
            let mut s = RecordSchema::new();
            add_node_metadata(&mut s, alias);
            add_referenced_props(&mut s, alias, props);
            s
        }

        FullTextLookup { alias, .. } => {
            let mut s = RecordSchema::new();
            add_node_metadata(&mut s, alias);
            add_referenced_props(&mut s, alias, props);
            s
        }

        IdLookup { alias, .. } => {
            let mut s = RecordSchema::new();
            add_node_metadata(&mut s, alias);
            add_referenced_props(&mut s, alias, props);
            s
        }

        Expand {
            input,
            dst_alias,
            rel_alias,
            var_length,
            ..
        } => {
            let mut s = infer_with_props(input, props);
            add_node_metadata(&mut s, dst_alias);
            add_referenced_props(&mut s, dst_alias, props);
            if let Some(ra) = rel_alias {
                if *var_length {
                    // Var-length rel binds to a single Value::List slot.
                    s.add(ra.as_str());
                } else {
                    add_edge_metadata(&mut s, ra);
                    add_referenced_props(&mut s, ra, props);
                }
            }
            s
        }

        Filter { input, .. }
        | Sort { input, .. }
        | Distinct { input }
        | Skip { input, .. }
        | Limit { input, .. }
        | SetProperty { input, .. }
        | SetLabel { input, .. }
        | SetProperties { input, .. }
        | Remove { input, .. } => infer_with_props(input, props),

        // Delete sets `<var>.__deleted = true` on returned records for each
        // Variable expression. Reserve those slots so the slot bridge can
        // surface the flag to downstream operators (Project/Property reads
        // raise `EntityNotFound` when the flag is set).
        Delete { input, exprs, .. } => {
            let mut s = infer_with_props(input, props);
            for e in exprs {
                if let crate::cypher::ast::ExprKind::Variable(v) = &e.kind {
                    s.add(format!("{v}.__deleted"));
                }
            }
            s
        }

        Project {
            input,
            items,
            emit_compound,
        } => {
            let mut s = RecordSchema::new();
            // Mirror `exec_project`'s intermediate Variable rename: when an
            // item is `Variable(src) [AS alias]` and we're not at the
            // final RETURN (`emit_compound = false`), propagate every
            // `src.*` slot from the input schema as `alias.*` so downstream
            // operators can still resolve `alias.__id` / `alias.prop` etc.
            // Final RETURN keeps the simple "alias-only" shape.
            let in_schema = if !*emit_compound {
                Some(infer_with_props(input, props))
            } else {
                None
            };
            for item in items {
                let col = return_item_name(item);
                s.add(&col);
                if let (false, Some(in_s)) = (*emit_compound, in_schema.as_ref()) {
                    if let crate::cypher::ast::ExprKind::Variable(src) = &item.expr.kind {
                        let src_prefix = format!("{src}.");
                        let dst_prefix = format!("{col}.");
                        for (_, name) in in_s.iter() {
                            if let Some(rest) = name.strip_prefix(&src_prefix) {
                                s.add(format!("{dst_prefix}{rest}"));
                            }
                        }
                    }
                }
            }
            s
        }

        Aggregate {
            group_keys,
            aggregates,
            ..
        } => {
            let mut s = RecordSchema::new();
            for g in group_keys {
                s.add(expr_to_column_name(g));
            }
            for a in aggregates {
                s.add(aggregate_name(a));
            }
            s
        }

        CrossProduct { left, right, .. } => {
            let mut s = infer_with_props(left, props);
            extend_with(&mut s, &infer_with_props(right, props));
            s
        }

        CorrelatedJoin { input, right, .. } => {
            let mut s = infer_with_props(input, props);
            extend_with(&mut s, &infer_with_props(right, props));
            s
        }

        LeftOuterJoin { input, right, .. } => {
            let mut s = infer_with_props(input, props);
            extend_with(&mut s, &infer_with_props(right, props));
            s
        }

        Unwind { input, alias, .. } => {
            // Unwound items may be scalars, nodes, or edges; downstream
            // operators (`MATCH (a)-[r:T]->(b) UNWIND [a, b] AS x RETURN
            // x.name`, `UNWIND nodes(p) AS n RETURN n.name`, etc.) freely
            // pull `alias.<prop>` and metadata keys off the binding.
            // Reserve the union of node + edge metadata clusters and any
            // referenced properties so the slot path can decant the
            // post-unwind named record into the schema without dropping
            // user-visible columns. Cost is small (≤ 8 slots per alias)
            // and unreferenced slots cost nothing at runtime.
            let mut s = infer_with_props(input, props);
            add_node_metadata(&mut s, alias);
            add_edge_metadata(&mut s, alias);
            add_referenced_props(&mut s, alias, props);
            s
        }

        Call {
            input,
            yield_items,
            yield_star,
            ..
        } => {
            let mut s = infer_with_props(input, props);
            // YIELD * is resolved at exec time (we don't have the procedure
            // signature here); leave yielded slots out of the schema.
            if !*yield_star {
                for (col, alias) in yield_items {
                    s.add(alias.clone().unwrap_or_else(|| col.clone()));
                }
            }
            s
        }

        CreateNode {
            alias, properties, ..
        } => {
            let mut s = RecordSchema::new();
            if let Some(a) = alias {
                add_node_metadata(&mut s, a);
                add_referenced_props(&mut s, a, props);
                for k in properties.keys() {
                    s.add(format!("{a}.{k}"));
                }
            }
            s
        }

        CreateEdge {
            rel_alias,
            properties,
            ..
        } => {
            let mut s = RecordSchema::new();
            if let Some(ra) = rel_alias {
                add_edge_metadata(&mut s, ra);
                add_referenced_props(&mut s, ra, props);
                for k in properties.keys() {
                    s.add(format!("{ra}.{k}"));
                }
            }
            s
        }

        CreateSequence { ops } => {
            let mut s = RecordSchema::new();
            for o in ops {
                extend_with(&mut s, &infer_with_props(o, props));
            }
            s
        }

        MatchCreate { input, create_ops } => {
            let mut s = infer_with_props(input, props);
            for o in create_ops {
                extend_with(&mut s, &infer_with_props(o, props));
            }
            s
        }

        Merge { pattern, .. } => {
            let mut s = RecordSchema::new();
            populate_pattern_bindings(&mut s, pattern, props);
            s
        }

        MatchMerge {
            input,
            merge_pattern,
            ..
        } => {
            let mut s = infer_with_props(input, props);
            populate_pattern_bindings(&mut s, merge_pattern, props);
            s
        }

        MaterializePath {
            input, path_alias, ..
        } => {
            let mut s = infer_with_props(input, props);
            s.add(path_alias.as_str());
            s
        }

        ShortestPath {
            input,
            dst_alias,
            path_alias,
            ..
        } => {
            let mut s = infer_with_props(input, props);
            add_node_metadata(&mut s, dst_alias);
            add_referenced_props(&mut s, dst_alias, props);
            s.add(path_alias.as_str());
            s
        }

        Union { inputs, .. } => {
            // UNION enforces same shape across branches; the first branch
            // is canonical (parser/planner reject mismatched columns).
            inputs
                .first()
                .map(|o| infer_with_props(o, props))
                .unwrap_or_default()
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn add_node_metadata(s: &mut RecordSchema, alias: &str) {
    s.add(alias);
    s.add(format!("{alias}.__id"));
    s.add(format!("{alias}.__label"));
    s.add(format!("{alias}.__labels"));
}

fn add_edge_metadata(s: &mut RecordSchema, alias: &str) {
    s.add(alias);
    s.add(format!("{alias}.__src"));
    s.add(format!("{alias}.__dst"));
    s.add(format!("{alias}.__type"));
    s.add(format!("{alias}.__edge_seq"));
}

fn add_referenced_props(
    s: &mut RecordSchema,
    alias: &str,
    props: &HashMap<String, BTreeSet<String>>,
) {
    if let Some(set) = props.get(alias) {
        for p in set {
            s.add(format!("{alias}.{p}"));
        }
    }
}

fn extend_with(target: &mut RecordSchema, src: &RecordSchema) {
    for (_, name) in src.iter() {
        target.add(name.to_string());
    }
}

fn return_item_name(item: &ReturnItem) -> String {
    item.alias
        .clone()
        .unwrap_or_else(|| expr_to_column_name(&item.expr))
}

fn aggregate_name(a: &AggregateExpr) -> String {
    if let Some(alias) = &a.alias {
        return alias.clone();
    }
    if let Some(text) = &a.original_call_text {
        return text.clone();
    }
    // Fallback: function-name(arg) — column-name parity isn't critical here
    // because the planner always sets `original_call_text` on aggregates that
    // travel through Project; this branch only runs in synthetic tests.
    format!("{}({})", a.original_name, expr_to_column_name(&a.input))
}

fn populate_pattern_bindings(
    s: &mut RecordSchema,
    pattern: &Pattern,
    props: &HashMap<String, BTreeSet<String>>,
) {
    if let Some(p) = &pattern.path_variable {
        s.add(p.as_str());
    }
    for el in &pattern.elements {
        match el {
            PatternElement::Node(n) => {
                if let Some(v) = &n.variable {
                    add_node_metadata(s, v);
                    add_referenced_props(s, v, props);
                    for k in n.properties.keys() {
                        s.add(format!("{v}.{k}"));
                    }
                }
            }
            PatternElement::Relationship(r) => {
                if let Some(v) = &r.variable {
                    if r.var_length.is_some() {
                        s.add(v.as_str());
                    } else {
                        add_edge_metadata(s, v);
                        add_referenced_props(s, v, props);
                        for k in r.properties.keys() {
                            s.add(format!("{v}.{k}"));
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property-reference collection
//
// Walks the entire plan once, gathering every `alias.prop` reference into a
// per-alias set. Used so `Scan { alias }` knows which property slots to
// reserve even when the references live in downstream `Filter`/`Project`/
// `Aggregate` expressions.

/// Walk `op` and record every `alias.prop` reference into `out`.
pub fn collect_property_refs(op: &LogicalOp, out: &mut HashMap<String, BTreeSet<String>>) {
    use LogicalOp::*;
    match op {
        SingleRow | EmptyRow | Scan { .. } | IdLookup { .. } => {}

        IndexLookup {
            remaining_filters, ..
        } => {
            if let Some(f) = remaining_filters {
                walk_expr(f, out);
            }
        }

        FullTextLookup {
            term,
            remaining_filters,
            ..
        } => {
            walk_expr(term, out);
            if let Some(f) = remaining_filters {
                walk_expr(f, out);
            }
        }

        Expand {
            input,
            var_length_prop_filters,
            ..
        } => {
            collect_property_refs(input, out);
            for v in var_length_prop_filters.values() {
                walk_expr(v, out);
            }
        }

        Filter { input, predicate } => {
            collect_property_refs(input, out);
            walk_expr(predicate, out);
        }

        Project { input, items, .. } => {
            collect_property_refs(input, out);
            for i in items {
                walk_expr(&i.expr, out);
            }
        }

        Aggregate {
            input,
            group_keys,
            aggregates,
        } => {
            collect_property_refs(input, out);
            for g in group_keys {
                walk_expr(g, out);
            }
            for a in aggregates {
                walk_expr(&a.input, out);
                if let Some(e) = &a.extra_arg {
                    walk_expr(e, out);
                }
            }
        }

        Sort { input, items } => {
            collect_property_refs(input, out);
            for s in items {
                walk_expr(&s.expr, out);
            }
        }

        Distinct { input }
        | Skip { input, .. }
        | Limit { input, .. }
        | SetLabel { input, .. }
        | Remove { input, .. } => collect_property_refs(input, out),

        CreateNode { properties, .. } => {
            for v in properties.values() {
                walk_expr(v, out);
            }
        }

        CreateEdge { properties, .. } => {
            for v in properties.values() {
                walk_expr(v, out);
            }
        }

        CreateSequence { ops } => {
            for o in ops {
                collect_property_refs(o, out);
            }
        }

        MatchCreate { input, create_ops } => {
            collect_property_refs(input, out);
            for o in create_ops {
                collect_property_refs(o, out);
            }
        }

        CrossProduct { left, right, .. } => {
            collect_property_refs(left, out);
            collect_property_refs(right, out);
        }

        CorrelatedJoin { input, right, .. } => {
            collect_property_refs(input, out);
            collect_property_refs(right, out);
        }

        LeftOuterJoin {
            input,
            right,
            opt_filter,
            ..
        } => {
            collect_property_refs(input, out);
            collect_property_refs(right, out);
            if let Some(f) = opt_filter {
                walk_expr(f, out);
            }
        }

        Delete { input, exprs, .. } => {
            collect_property_refs(input, out);
            for e in exprs {
                walk_expr(e, out);
            }
        }

        SetProperty { input, assignments } => {
            collect_property_refs(input, out);
            for a in assignments {
                walk_expr(&a.value, out);
                // alias.property is also a written slot
                out.entry(a.variable.clone())
                    .or_default()
                    .insert(a.property.clone());
            }
        }

        SetProperties { input, value, .. } => {
            collect_property_refs(input, out);
            walk_expr(value, out);
        }

        Merge {
            pattern,
            on_create,
            on_match,
        } => {
            walk_pattern(pattern, out);
            for s in on_create.iter().chain(on_match.iter()) {
                walk_set_item(s, out);
            }
        }

        MatchMerge {
            input,
            merge_pattern,
            on_create,
            on_match,
        } => {
            collect_property_refs(input, out);
            walk_pattern(merge_pattern, out);
            for s in on_create.iter().chain(on_match.iter()) {
                walk_set_item(s, out);
            }
        }

        MaterializePath { input, .. } => collect_property_refs(input, out),

        Unwind { input, expr, .. } => {
            collect_property_refs(input, out);
            walk_expr(expr, out);
        }

        Call { input, args, .. } => {
            collect_property_refs(input, out);
            for a in args {
                walk_expr(a, out);
            }
        }

        ShortestPath { input, .. } => collect_property_refs(input, out),

        Union { inputs, .. } => {
            for i in inputs {
                collect_property_refs(i, out);
            }
        }
    }
}

fn walk_pattern(pattern: &Pattern, out: &mut HashMap<String, BTreeSet<String>>) {
    for el in &pattern.elements {
        match el {
            PatternElement::Node(n) => {
                if let Some(v) = &n.variable {
                    let entry = out.entry(v.clone()).or_default();
                    for k in n.properties.keys() {
                        entry.insert(k.clone());
                    }
                }
                for e in n.properties.values() {
                    walk_expr(e, out);
                }
            }
            PatternElement::Relationship(r) => {
                if let Some(v) = &r.variable {
                    let entry = out.entry(v.clone()).or_default();
                    for k in r.properties.keys() {
                        entry.insert(k.clone());
                    }
                }
                for e in r.properties.values() {
                    walk_expr(e, out);
                }
            }
        }
    }
}

fn walk_set_item(item: &SetItem, out: &mut HashMap<String, BTreeSet<String>>) {
    use SetItem::*;
    match item {
        Property(a) => {
            out.entry(a.variable.clone())
                .or_default()
                .insert(a.property.clone());
            walk_expr(&a.value, out);
        }
        MapOverwrite { value, .. } | MapMerge { value, .. } => walk_expr(value, out),
        Label { .. } => {}
    }
}

fn walk_expr(e: &Expr, out: &mut HashMap<String, BTreeSet<String>>) {
    match &e.kind {
        ExprKind::Property(var, prop) => {
            out.entry(var.clone()).or_default().insert(prop.clone());
        }
        ExprKind::DotAccess { expr, key } => {
            // alias.key — same slot shape as Property when base is Variable.
            if let ExprKind::Variable(var) = &expr.kind {
                out.entry(var.clone()).or_default().insert(key.clone());
            }
            walk_expr(expr, out);
        }
        ExprKind::Variable(_)
        | ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::Star
        | ExprKind::HasLabel(_, _) => {}
        ExprKind::BinaryOp { left, right, .. } => {
            walk_expr(left, out);
            walk_expr(right, out);
        }
        ExprKind::Not(x) | ExprKind::IsNull(x) | ExprKind::IsNotNull(x) => walk_expr(x, out),
        ExprKind::FunctionCall { args, .. } => {
            for a in args {
                walk_expr(a, out);
            }
        }
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            if let Some(o) = operand {
                walk_expr(o, out);
            }
            for (cond, res) in alternatives {
                walk_expr(cond, out);
                walk_expr(res, out);
            }
            if let Some(d) = default {
                walk_expr(d, out);
            }
        }
        ExprKind::List(xs) => {
            for x in xs {
                walk_expr(x, out);
            }
        }
        ExprKind::ListComprehension {
            list_expr,
            filter,
            map_expr,
            ..
        } => {
            walk_expr(list_expr, out);
            if let Some(f) = filter {
                walk_expr(f, out);
            }
            if let Some(m) = map_expr {
                walk_expr(m, out);
            }
        }
        ExprKind::PatternComprehension {
            pattern,
            where_clause,
            map_expr,
            ..
        } => {
            walk_pattern(pattern, out);
            if let Some(w) = where_clause {
                walk_expr(w, out);
            }
            walk_expr(map_expr, out);
        }
        ExprKind::Exists {
            patterns,
            where_clause,
        } => {
            for p in patterns {
                walk_pattern(p, out);
            }
            if let Some(w) = where_clause {
                walk_expr(w, out);
            }
        }
        ExprKind::ExistsSubquery(_stmt) => {
            // Subquery has its own scope — its property refs would attach
            // to a separate plan after planning, not this one. Skip.
        }
        ExprKind::MapLiteral(entries) => {
            for (_, v) in entries {
                walk_expr(v, out);
            }
        }
        ExprKind::Index { expr, index } => {
            walk_expr(expr, out);
            walk_expr(index, out);
        }
        ExprKind::Slice { expr, start, end } => {
            walk_expr(expr, out);
            if let Some(s) = start {
                walk_expr(s, out);
            }
            if let Some(e) = end {
                walk_expr(e, out);
            }
        }
        ExprKind::Quantifier {
            list_expr,
            predicate,
            ..
        } => {
            walk_expr(list_expr, out);
            walk_expr(predicate, out);
        }
        ExprKind::PatternPredicate(p) => walk_pattern(p, out),
    }
}

// Suppress an unused-import warning for the SortItem type — pulled in to
// keep the public surface honest if we later add per-item handling.
#[allow(dead_code)]
fn _kept_for_future_use(_: &SortItem) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{
        Expr, ExprKind, LiteralValue, NodePattern, PatternElement, RelDirection, RelPattern,
        ReturnItem, ShortestPathMode, SortItem,
    };
    use crate::cypher::ir::{AggregateExpr, AggregateFunction as IrAggFn, LogicalOp};
    use crate::types::Direction;
    use std::collections::HashMap;

    fn var(name: &str) -> Expr {
        Expr::synthetic(ExprKind::Variable(name.into()))
    }

    fn prop(v: &str, p: &str) -> Expr {
        Expr::synthetic(ExprKind::Property(v.into(), p.into()))
    }

    fn lit_int(n: i64) -> Expr {
        Expr::synthetic(ExprKind::Literal(LiteralValue::I64(n)))
    }

    #[test]
    fn empty_plan_produces_empty_schema() {
        assert_eq!(infer_schema(&LogicalOp::SingleRow).len(), 0);
        assert_eq!(infer_schema(&LogicalOp::EmptyRow).len(), 0);
    }

    #[test]
    fn scan_emits_metadata_only_when_no_props_referenced() {
        let s = infer_schema(&LogicalOp::Scan {
            label: "Person".into(),
            alias: "p".into(),
        });
        let names: Vec<&str> = s.iter().map(|(_, n)| n).collect();
        assert_eq!(names, vec!["p", "p.__id", "p.__label", "p.__labels"]);
    }

    #[test]
    fn filter_propagates_property_refs_back_to_scan() {
        let plan = LogicalOp::Filter {
            input: Box::new(LogicalOp::Scan {
                label: "Person".into(),
                alias: "p".into(),
            }),
            predicate: Expr::synthetic(ExprKind::BinaryOp {
                left: Box::new(prop("p", "age")),
                op: crate::cypher::ast::BinOp::Gt,
                right: Box::new(lit_int(18)),
            }),
        };
        let s = infer_schema(&plan);
        assert!(s.slot("p.age").is_some(), "p.age should be slotted");
        assert!(s.slot("p.__id").is_some());
    }

    #[test]
    fn project_replaces_input_schema() {
        let plan = LogicalOp::Project {
            input: Box::new(LogicalOp::Scan {
                label: "Person".into(),
                alias: "p".into(),
            }),
            items: vec![ReturnItem {
                expr: prop("p", "name"),
                alias: Some("nm".into()),
            }],
            emit_compound: true,
        };
        let s = infer_schema(&plan);
        assert_eq!(s.len(), 1);
        assert!(s.slot("nm").is_some());
        // Project replaces — input's "p", "p.__id" are not in output.
        assert!(s.slot("p").is_none());
        assert!(s.slot("p.__id").is_none());
    }

    #[test]
    fn project_uses_expr_column_name_when_no_alias() {
        let plan = LogicalOp::Project {
            input: Box::new(LogicalOp::Scan {
                label: "Person".into(),
                alias: "p".into(),
            }),
            items: vec![ReturnItem {
                expr: prop("p", "name"),
                alias: None,
            }],
            emit_compound: true,
        };
        let s = infer_schema(&plan);
        assert!(s.slot("p.name").is_some());
    }

    #[test]
    fn expand_appends_dst_and_rel_metadata() {
        let plan = LogicalOp::Expand {
            input: Box::new(LogicalOp::Scan {
                label: "Person".into(),
                alias: "a".into(),
            }),
            src_alias: "a".into(),
            dst_alias: "b".into(),
            rel_alias: Some("r".into()),
            edge_types: vec!["KNOWS".into()],
            direction: Direction::Outgoing,
            min_hops: 1,
            max_hops: 1,
            var_length: false,
            var_length_prop_filters: HashMap::new(),
            result_cap: None,
        };
        let s = infer_schema(&plan);
        // Input scan
        assert!(s.slot("a").is_some());
        // Dst metadata
        assert!(s.slot("b").is_some());
        assert!(s.slot("b.__id").is_some());
        // Rel metadata
        assert!(s.slot("r").is_some());
        assert!(s.slot("r.__src").is_some());
        assert!(s.slot("r.__type").is_some());
    }

    #[test]
    fn var_length_rel_gets_single_slot() {
        let plan = LogicalOp::Expand {
            input: Box::new(LogicalOp::Scan {
                label: "Person".into(),
                alias: "a".into(),
            }),
            src_alias: "a".into(),
            dst_alias: "b".into(),
            rel_alias: Some("path".into()),
            edge_types: vec!["KNOWS".into()],
            direction: Direction::Outgoing,
            min_hops: 1,
            max_hops: 5,
            var_length: true,
            var_length_prop_filters: HashMap::new(),
            result_cap: None,
        };
        let s = infer_schema(&plan);
        assert!(s.slot("path").is_some());
        // No __src/__dst/__type for var-length rel — it's a list of edges.
        assert!(s.slot("path.__src").is_none());
        assert!(s.slot("path.__type").is_none());
    }

    #[test]
    fn aggregate_emits_group_keys_and_aggregates() {
        let plan = LogicalOp::Aggregate {
            input: Box::new(LogicalOp::Scan {
                label: "Person".into(),
                alias: "p".into(),
            }),
            group_keys: vec![prop("p", "age")],
            aggregates: vec![AggregateExpr {
                function: IrAggFn::Count,
                input: Expr::synthetic(ExprKind::Star),
                alias: Some("ct".into()),
                distinct: false,
                extra_arg: None,
                original_name: "count".into(),
                original_call_text: Some("count(*)".into()),
            }],
        };
        let s = infer_schema(&plan);
        assert_eq!(s.len(), 2);
        assert!(s.slot("p.age").is_some());
        assert!(s.slot("ct").is_some());
    }

    #[test]
    fn pass_through_ops_preserve_schema() {
        let scan = LogicalOp::Scan {
            label: "Person".into(),
            alias: "p".into(),
        };
        let base = infer_schema(&scan);
        let with_distinct = infer_schema(&LogicalOp::Distinct {
            input: Box::new(scan.clone()),
        });
        let with_skip = infer_schema(&LogicalOp::Skip {
            input: Box::new(scan.clone()),
            count: 10,
        });
        let with_sort = infer_schema(&LogicalOp::Sort {
            input: Box::new(scan.clone()),
            items: vec![SortItem {
                expr: var("p"),
                descending: false,
            }],
        });
        assert_eq!(base.len(), with_distinct.len());
        assert_eq!(base.len(), with_skip.len());
        assert_eq!(base.len(), with_sort.len());
    }

    #[test]
    fn unwind_appends_alias() {
        let plan = LogicalOp::Unwind {
            input: Box::new(LogicalOp::SingleRow),
            expr: Expr::synthetic(ExprKind::List(vec![lit_int(1), lit_int(2)])),
            alias: "x".into(),
        };
        let s = infer_schema(&plan);
        // Unwind reserves node + edge metadata clusters around the alias
        // (the slot path needs them to decant Node/Edge bindings produced
        // by `UNWIND nodes(p) AS n` etc.). Concretely: `x`, `x.__id`,
        // `x.__label`, `x.__labels`, `x.__src`, `x.__dst`, `x.__type`,
        // `x.__edge_seq` — 8 slots when no extra properties are referenced.
        assert_eq!(s.len(), 8);
        assert_eq!(s.slot("x"), Some(super::super::record_v2::SlotId(0)));
        assert!(s.slot("x.__id").is_some());
        assert!(s.slot("x.__src").is_some());
    }

    #[test]
    fn cross_product_unions_left_and_right() {
        let plan = LogicalOp::CrossProduct {
            left: Box::new(LogicalOp::Scan {
                label: "A".into(),
                alias: "a".into(),
            }),
            right: Box::new(LogicalOp::Scan {
                label: "B".into(),
                alias: "b".into(),
            }),
            same_match: false,
        };
        let s = infer_schema(&plan);
        assert!(s.slot("a").is_some());
        assert!(s.slot("b").is_some());
        assert!(s.slot("a.__id").is_some());
        assert!(s.slot("b.__id").is_some());
    }

    #[test]
    fn slot_indices_are_contiguous_and_unique() {
        let plan = LogicalOp::Filter {
            input: Box::new(LogicalOp::Scan {
                label: "Person".into(),
                alias: "p".into(),
            }),
            predicate: Expr::synthetic(ExprKind::BinaryOp {
                left: Box::new(prop("p", "name")),
                op: crate::cypher::ast::BinOp::Eq,
                right: Box::new(Expr::synthetic(ExprKind::Literal(LiteralValue::String(
                    "Alice".into(),
                )))),
            }),
        };
        let s = infer_schema(&plan);
        let mut seen = std::collections::HashSet::new();
        for (slot, _) in s.iter() {
            assert!(seen.insert(slot), "slot {slot:?} appears twice");
            assert!(slot.index() < s.len(), "slot index out of range");
        }
        // All slots [0..len) present.
        assert_eq!(seen.len(), s.len());
    }

    #[test]
    fn property_refs_walked_across_clauses() {
        // Given the same plan, collect_property_refs sees `p.age` (Filter
        // predicate) and `p.name` (Project item) as both belonging to alias
        // `p`. The pre-pass runs on whatever subtree it's called with, so
        // when phase 3 attaches a schema to each operator, every operator's
        // pre-pass sees the references in its own subtree.
        let plan = LogicalOp::Project {
            input: Box::new(LogicalOp::Filter {
                input: Box::new(LogicalOp::Scan {
                    label: "Person".into(),
                    alias: "p".into(),
                }),
                predicate: Expr::synthetic(ExprKind::BinaryOp {
                    left: Box::new(prop("p", "age")),
                    op: crate::cypher::ast::BinOp::Gt,
                    right: Box::new(lit_int(18)),
                }),
            }),
            items: vec![ReturnItem {
                expr: prop("p", "name"),
                alias: None,
            }],
            emit_compound: true,
        };
        let mut refs: HashMap<String, BTreeSet<String>> = HashMap::new();
        collect_property_refs(&plan, &mut refs);
        let p_props = refs.get("p").expect("p has refs");
        assert!(p_props.contains("age"));
        assert!(p_props.contains("name"));
    }

    #[test]
    fn merge_pattern_emits_all_pattern_bindings() {
        // MERGE (a:Person {name: 'Alice'})-[r:KNOWS]->(b:Person)
        let pattern = Pattern {
            elements: vec![
                PatternElement::Node(NodePattern {
                    variable: Some("a".into()),
                    labels: vec!["Person".into()],
                    properties: {
                        let mut h = HashMap::new();
                        h.insert(
                            "name".into(),
                            Expr::synthetic(ExprKind::Literal(LiteralValue::String(
                                "Alice".into(),
                            ))),
                        );
                        h
                    },
                }),
                PatternElement::Relationship(RelPattern {
                    variable: Some("r".into()),
                    rel_types: vec!["KNOWS".into()],
                    properties: HashMap::new(),
                    direction: RelDirection::Outgoing,
                    var_length: None,
                }),
                PatternElement::Node(NodePattern {
                    variable: Some("b".into()),
                    labels: vec!["Person".into()],
                    properties: HashMap::new(),
                }),
            ],
            path_variable: None,
            shortest_path_mode: ShortestPathMode::None,
        };
        let plan = LogicalOp::Merge {
            pattern,
            on_create: vec![],
            on_match: vec![],
        };
        let s = infer_schema(&plan);
        assert!(s.slot("a").is_some());
        assert!(s.slot("a.name").is_some(), "inline pattern prop slotted");
        assert!(s.slot("r.__type").is_some());
        assert!(s.slot("b").is_some());
    }

    /// Plan a Cypher query against a fresh in-memory database and infer
    /// its schema. The planner doesn't need any data for these tests, just
    /// an initialized connection.
    fn infer_for(cypher: &str) -> RecordSchema {
        let db = crate::Database::open_memory().unwrap();
        let stmt = crate::cypher::parser::parse(cypher).unwrap();
        let plan = crate::cypher::planner::plan(db.connection(), &stmt).unwrap();
        infer_schema(&plan)
    }

    #[test]
    fn real_query_match_return_property() {
        let s = infer_for("MATCH (p:Person) RETURN p.name");
        assert_eq!(s.len(), 1);
        assert!(s.slot("p.name").is_some());
    }

    #[test]
    fn real_query_match_where_return() {
        let s = infer_for("MATCH (p:Person) WHERE p.age > 18 RETURN p.name AS nm");
        assert_eq!(s.len(), 1);
        assert!(s.slot("nm").is_some());
    }

    #[test]
    fn real_query_match_expand() {
        let s = infer_for("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, r.since, b.name");
        assert_eq!(s.len(), 3);
        assert!(s.slot("a.name").is_some());
        assert!(s.slot("r.since").is_some());
        assert!(s.slot("b.name").is_some());
    }

    #[test]
    fn real_query_aggregate() {
        let s = infer_for("MATCH (p:Person) RETURN p.age, count(*)");
        assert_eq!(s.len(), 2);
        assert!(s.slot("p.age").is_some());
        assert!(s.slot("count(*)").is_some());
    }

    #[test]
    fn real_query_with_chain() {
        let s = infer_for("MATCH (p:Person) WITH p.age AS yrs WHERE yrs > 18 RETURN yrs");
        assert_eq!(s.len(), 1);
        assert!(s.slot("yrs").is_some());
    }

    #[test]
    fn real_query_var_length_path() {
        let s = infer_for(
            "MATCH (a:Person)-[r:KNOWS*1..3]->(b:Person) RETURN a.name AS na, b.name AS nb",
        );
        assert!(s.slot("na").is_some());
        assert!(s.slot("nb").is_some());
    }

    #[test]
    fn real_query_create_node() {
        // CREATE has no output projection — output schema is whatever
        // bindings the matched/created variables produce.
        let s = infer_for("CREATE (p:Person {name: 'Alice'})");
        // Without RETURN, there's no Project — the operator chain ends with
        // the create and exposes the alias bindings.
        assert!(s.slot("p").is_some(), "alias slot present");
        assert!(s.slot("p.name").is_some(), "inline-prop slot present");
    }

    #[test]
    fn real_query_unwind() {
        let s = infer_for("UNWIND [1, 2, 3] AS x RETURN x");
        assert!(s.slot("x").is_some());
    }

    #[test]
    fn slot_indices_contiguous_on_real_plans() {
        for q in [
            "MATCH (p:Person) RETURN p.name",
            "MATCH (p:Person) WHERE p.age > 18 RETURN p.name",
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name",
            "MATCH (p:Person) RETURN p.age, count(*)",
            "UNWIND [1, 2, 3] AS x RETURN x",
        ] {
            let s = infer_for(q);
            let mut seen = std::collections::HashSet::new();
            for (slot, _) in s.iter() {
                assert!(
                    seen.insert(slot),
                    "duplicate slot in schema for query `{q}`"
                );
                assert!(
                    slot.index() < s.len(),
                    "slot index out of range for query `{q}`"
                );
            }
            assert_eq!(seen.len(), s.len(), "missing slot in [0..len) for `{q}`");
        }
    }

    #[test]
    fn project_with_aggregate_alias() {
        // count(*) AS ct
        let plan = LogicalOp::Aggregate {
            input: Box::new(LogicalOp::Scan {
                label: "Person".into(),
                alias: "p".into(),
            }),
            group_keys: vec![],
            aggregates: vec![AggregateExpr {
                function: IrAggFn::Count,
                input: Expr::synthetic(ExprKind::Star),
                alias: None,
                distinct: false,
                extra_arg: None,
                original_name: "count".into(),
                original_call_text: Some("count(*)".into()),
            }],
        };
        let s = infer_schema(&plan);
        assert_eq!(s.len(), 1);
        assert!(s.slot("count(*)").is_some());
    }
}
