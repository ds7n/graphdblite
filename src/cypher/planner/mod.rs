use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::ir::*;
use crate::index;
use crate::types::Value;

/// Global counter for unique anonymous variable aliases across all plan_single_pattern calls.
static ANON_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Suggest the closest in-scope name for a misspelled identifier.
///
/// Returns the closest candidate within Levenshtein distance ≤ 2 (or ≤ 1
/// for short names), ignoring case. Returns `None` if nothing close enough
/// is found — callers should not blindly attach a suggestion that's only
/// vaguely similar.
pub fn plan(conn: &Connection, stmt: &Statement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_inner(conn, stmt, false)?;
    apply_post_passes(conn, &mut op);
    Ok(op)
}

/// Run the standard post-planning rewrites in order. Kept as a single
/// helper so every plan entry point (`plan`, `plan_with_procedures`,
/// `plan_subquery`) applies the same set without drift.
pub(in crate::cypher::planner) fn apply_post_passes(conn: &Connection, op: &mut LogicalOp) {
    push_limit_into_var_length_expand(op);
    rewrite_id_filter_to_lookup(op);
    rewrite_text_filter_to_fts(conn, op);
}

/// Optimization: rewrite `Filter(Scan { label: "", alias }, id(alias) = X)`
/// into `IdLookup { alias, value: X }`. Critical for `WHERE id(n) = $x`
/// usage (e.g. the Python binding's `batch_create_edges` helper, which
/// otherwise full-scans the entire node table per UNWIND row). Recurses
/// into all child operators so nested patterns benefit too.
///
/// Only rewrites the unlabeled-scan form for simplicity. A labeled
/// `MATCH (a:Foo) WHERE id(a) = $x` is a much rarer pattern and we leave
/// it on the existing Scan+Filter path; adding it would need a "verify
/// label after lookup" wrapper that this pass doesn't yet emit.
pub(in crate::cypher::planner) fn rewrite_id_filter_to_lookup(op: &mut LogicalOp) {
    // First recurse so inner subtrees are optimized before we try to
    // pattern-match this node.
    walk_children_mut(op, rewrite_id_filter_to_lookup);

    if !matches!(op, LogicalOp::Filter { .. }) {
        return;
    }

    // Take ownership of the Filter so we can rebuild freely.
    let placeholder = LogicalOp::SingleRow;
    let LogicalOp::Filter { input, predicate } = std::mem::replace(op, placeholder) else {
        unreachable!()
    };

    // Case 1 — non-correlated: Filter(Scan{"", alias}, id(alias) = expr).
    if let LogicalOp::Scan { label, alias } = input.as_ref() {
        if label.is_empty() {
            if let Some(value_expr) = extract_id_eq_alias(&predicate, alias) {
                if !expr_references_var(&value_expr, alias) {
                    *op = LogicalOp::IdLookup {
                        alias: alias.clone(),
                        value_expr,
                    };
                    return;
                }
            }
        }
    }

    // Case 2 — correlated: Filter(CorrelatedJoin{ input, right: Scan{"", alias} }, id(alias) = expr).
    // The WHERE filter sits *outside* the join in multi-clause statements
    // (UNWIND + MATCH ... WHERE id(a) = row.s + MATCH ...). Push the
    // id-predicate down into the right side as an IdLookup, drop the Filter.
    //
    // Safe because `id(alias) = expr`:
    //   - constrains only `alias` (a binding produced by the right side)
    //   - uses `expr` which references only outer/input-side bindings (we
    //     check `expr_references_var(&value_expr, alias)` to enforce this)
    if let LogicalOp::CorrelatedJoin { right, .. } = input.as_ref() {
        if let LogicalOp::Scan { label, alias } = right.as_ref() {
            if label.is_empty() {
                if let Some(value_expr) = extract_id_eq_alias(&predicate, alias) {
                    if !expr_references_var(&value_expr, alias) {
                        let alias = alias.clone();
                        // Rebuild: replace right with IdLookup, drop Filter.
                        let LogicalOp::CorrelatedJoin {
                            input: join_input,
                            same_match,
                            ..
                        } = *input
                        else {
                            unreachable!("matched above")
                        };
                        *op = LogicalOp::CorrelatedJoin {
                            input: join_input,
                            right: Box::new(LogicalOp::IdLookup { alias, value_expr }),
                            same_match,
                        };
                        return;
                    }
                }
            }
        }
    }

    // No rewrite applied — restore the Filter we took ownership of.
    *op = LogicalOp::Filter { input, predicate };
}

/// If `predicate` is `id(alias) = <expr>` (in either operand order),
/// return the other side as an `Expr`. The caller decides whether the
/// expression is safe to hoist (e.g. doesn't reference the alias itself).
fn extract_id_eq_alias(predicate: &Expr, alias: &str) -> Option<Expr> {
    let (left, right) = match &predicate.kind {
        ExprKind::BinaryOp {
            left,
            op: BinOp::Eq,
            right,
        } => (left.as_ref(), right.as_ref()),
        _ => return None,
    };

    if matches_id_of_alias(left, alias) {
        return Some(right.clone());
    }
    if matches_id_of_alias(right, alias) {
        return Some(left.clone());
    }
    None
}

fn matches_id_of_alias(expr: &Expr, alias: &str) -> bool {
    let ExprKind::FunctionCall { name, args, .. } = &expr.kind else {
        return false;
    };
    if !name.eq_ignore_ascii_case("id") || args.len() != 1 {
        return false;
    }
    matches!(&args[0].kind, ExprKind::Variable(v) if v == alias)
}

/// Optimization: rewrite `Filter(Scan{label, alias}, alias.prop OP term)`
/// into `FullTextLookup` when an FTS index exists on `(label, prop)`
/// and `OP` is one of `CONTAINS` / `STARTS WITH` / `ENDS WITH`.
///
/// Handles conjunction: when the filter is `A AND B AND ...`, picks
/// the first FTS-eligible conjunct, rebuilds the rest as a residual
/// filter carried inside the `FullTextLookup`.
///
/// Only handles the non-correlated form: `Filter(Scan{label, alias}, ...)`.
/// The correlated form is handled in Task 14.
pub(in crate::cypher::planner) fn rewrite_text_filter_to_fts(
    conn: &Connection,
    op: &mut LogicalOp,
) {
    // Recurse first so inner subtrees are optimized before pattern-matching.
    match op {
        LogicalOp::Expand { input, .. }
        | LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Aggregate { input, .. }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Distinct { input }
        | LogicalOp::Skip { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::MatchCreate { input, .. }
        | LogicalOp::Delete { input, .. }
        | LogicalOp::SetProperty { input, .. }
        | LogicalOp::SetLabel { input, .. }
        | LogicalOp::SetProperties { input, .. }
        | LogicalOp::Remove { input, .. }
        | LogicalOp::MatchMerge { input, .. }
        | LogicalOp::MaterializePath { input, .. }
        | LogicalOp::Unwind { input, .. }
        | LogicalOp::Call { input, .. }
        | LogicalOp::ShortestPath { input, .. } => rewrite_text_filter_to_fts(conn, input),
        LogicalOp::CrossProduct { left, right, .. } => {
            rewrite_text_filter_to_fts(conn, left);
            rewrite_text_filter_to_fts(conn, right);
        }
        LogicalOp::CorrelatedJoin { input, right, .. }
        | LogicalOp::LeftOuterJoin { input, right, .. } => {
            rewrite_text_filter_to_fts(conn, input);
            rewrite_text_filter_to_fts(conn, right);
        }
        LogicalOp::Union { inputs, .. } => {
            for inp in inputs {
                rewrite_text_filter_to_fts(conn, inp);
            }
        }
        LogicalOp::CreateSequence { ops } => {
            for inner in ops {
                rewrite_text_filter_to_fts(conn, inner);
            }
        }
        LogicalOp::SingleRow
        | LogicalOp::Scan { .. }
        | LogicalOp::IndexLookup { .. }
        | LogicalOp::IdLookup { .. }
        | LogicalOp::FullTextLookup { .. }
        | LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::Merge { .. }
        | LogicalOp::EmptyRow => {}
    }

    if !matches!(op, LogicalOp::Filter { .. }) {
        return;
    }

    // Take ownership of the Filter to potentially rebuild.
    let placeholder = LogicalOp::SingleRow;
    let LogicalOp::Filter { input, predicate } = std::mem::replace(op, placeholder) else {
        unreachable!()
    };

    // Non-correlated: Filter(Scan{label, alias}, alias.prop OP term).
    if let LogicalOp::Scan { label, alias } = input.as_ref() {
        if !label.is_empty() {
            if let Some((prop, fts_op, term, residual)) = extract_fts_predicate(&predicate, alias) {
                if fts_index_exists(conn, label, &prop) {
                    *op = LogicalOp::FullTextLookup {
                        label: label.clone(),
                        alias: alias.clone(),
                        property: prop,
                        op: fts_op,
                        term,
                        remaining_filters: residual,
                    };
                    return;
                }
            }
        }
    }

    // Correlated: Filter(CorrelatedJoin{ input, right: Scan{label, alias} },
    //                    alias.prop OP term).
    // Mirrors the IdLookup correlated case — push the FTS lookup into the
    // right side of the join, dropping the outer Filter.
    if let LogicalOp::CorrelatedJoin { right, .. } = input.as_ref() {
        if let LogicalOp::Scan { label, alias } = right.as_ref() {
            if !label.is_empty() {
                if let Some((prop, fts_op, term, residual)) =
                    extract_fts_predicate(&predicate, alias)
                {
                    if fts_index_exists(conn, label, &prop) {
                        let label = label.clone();
                        let alias = alias.clone();
                        let LogicalOp::CorrelatedJoin {
                            input: join_input,
                            same_match,
                            ..
                        } = *input
                        else {
                            unreachable!("matched above")
                        };
                        let new_right = LogicalOp::FullTextLookup {
                            label,
                            alias,
                            property: prop,
                            op: fts_op,
                            term,
                            remaining_filters: residual,
                        };
                        *op = LogicalOp::CorrelatedJoin {
                            input: join_input,
                            right: Box::new(new_right),
                            same_match,
                        };
                        return;
                    }
                }
            }
        }
    }

    // No rewrite applied — restore the Filter we took ownership of.
    *op = LogicalOp::Filter { input, predicate };
}

/// Check if an FTS index exists on `(label, property)` by querying sqlite_master.
fn fts_index_exists(conn: &Connection, label: &str, property: &str) -> bool {
    crate::fts::list_fulltext_indexes_for_label(conn, label)
        .map(|v| v.iter().any(|(_, p)| p == property))
        .unwrap_or(false)
}

/// Try to extract an FTS-eligible predicate from `predicate` (or a
/// top-level conjunction). Returns `(property, fts_op, term, residual)`.
/// The residual carries any conjuncts not consumed by the rewrite.
fn extract_fts_predicate(
    predicate: &Expr,
    alias: &str,
) -> Option<(String, crate::cypher::ir::FullTextOp, Expr, Option<Expr>)> {
    let conjuncts = flatten_top_level_and(predicate);
    for (idx, c) in conjuncts.iter().enumerate() {
        if let Some((prop, fts_op, term)) = match_fts_binop(c, alias) {
            let residual_parts: Vec<&Expr> = conjuncts
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .map(|(_, e)| *e)
                .collect();
            let residual = rebuild_and(&residual_parts);
            return Some((prop, fts_op, term, residual));
        }
    }
    None
}

/// Match an FTS binary op: `alias.prop CONTAINS|STARTS WITH|ENDS WITH term`.
fn match_fts_binop(e: &Expr, alias: &str) -> Option<(String, crate::cypher::ir::FullTextOp, Expr)> {
    use crate::cypher::ir::FullTextOp;
    let ExprKind::BinaryOp { left, op, right } = &e.kind else {
        return None;
    };
    let fts_op = match op {
        BinOp::Contains => FullTextOp::Contains,
        BinOp::StartsWith => FullTextOp::StartsWith,
        BinOp::EndsWith => FullTextOp::EndsWith,
        _ => return None,
    };
    // ExprKind::Property is a tuple (variable_name, property_name).
    let ExprKind::Property(var, prop) = &left.kind else {
        return None;
    };
    if var != alias {
        return None;
    }
    Some((prop.clone(), fts_op, (**right).clone()))
}

/// Flatten a top-level `AND` chain into individual conjuncts.
fn flatten_top_level_and(e: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        if let ExprKind::BinaryOp {
            left,
            op: BinOp::And,
            right,
        } = &e.kind
        {
            walk(left, out);
            walk(right, out);
        } else {
            out.push(e);
        }
    }
    walk(e, &mut out);
    out
}

/// Rebuild an `AND` chain from a slice of expression references.
/// Returns `None` if the slice is empty.
fn rebuild_and(parts: &[&Expr]) -> Option<Expr> {
    let mut iter = parts.iter().copied().cloned();
    let first = iter.next()?;
    Some(iter.fold(first, |acc, e| {
        Expr::synthetic(ExprKind::BinaryOp {
            left: Box::new(acc),
            op: BinOp::And,
            right: Box::new(e),
        })
    }))
}

/// Return true if `expr` references the named variable anywhere in its tree.
/// Used to reject self-referential id-lookups like `WHERE id(a) = id(a)`.
fn expr_references_var(expr: &Expr, name: &str) -> bool {
    use ExprKind::*;
    match &expr.kind {
        Variable(v) => v == name,
        Property(v, _) => v == name,
        BinaryOp { left, right, .. } => {
            expr_references_var(left, name) || expr_references_var(right, name)
        }
        Not(inner) | IsNull(inner) | IsNotNull(inner) => expr_references_var(inner, name),
        FunctionCall { args, .. } => args.iter().any(|a| expr_references_var(a, name)),
        Case {
            operand,
            alternatives,
            default,
        } => {
            operand
                .as_deref()
                .is_some_and(|e| expr_references_var(e, name))
                || alternatives
                    .iter()
                    .any(|(c, r)| expr_references_var(c, name) || expr_references_var(r, name))
                || default
                    .as_deref()
                    .is_some_and(|e| expr_references_var(e, name))
        }
        List(items) => items.iter().any(|e| expr_references_var(e, name)),
        _ => false,
    }
}

/// Optimization: push a `LIMIT N` cap down into a var-length `Expand` when the
/// chain between them is row-preserving.
///
/// Safe pattern: `Limit { count: N, input: chain }` where `chain` is zero or
/// more `Project` (always 1:1) wrapping a single `Expand { var_length: true }`.
/// Anything else (Sort, Distinct, Filter, Aggregate, CrossProduct, another
/// Expand) breaks the equivalence — `LIMIT` and Expand row counts diverge.
///
/// Recurses into all child operators so nested patterns are still optimized.
pub(in crate::cypher::planner) fn push_limit_into_var_length_expand(op: &mut LogicalOp) {
    if let LogicalOp::Limit { input, count } = op {
        if let Some(LogicalOp::Expand { result_cap, .. }) = find_pushdown_target(input) {
            // Take the tighter of any existing cap and the new one.
            let new_cap = match *result_cap {
                Some(existing) => existing.min(*count),
                None => *count,
            };
            *result_cap = Some(new_cap);
        }
    }
    // Recurse into children so nested Limit/Expand chains (e.g. inside a
    // CorrelatedJoin's right side) also get the pushdown.
    walk_children_mut(op, push_limit_into_var_length_expand);
}

/// Returns a mutable reference to the var-length Expand directly reachable from
/// `op` through only Project (or empty) wrappers, or `None` if any disqualifying
/// operator is in the chain.
pub(in crate::cypher::planner) fn find_pushdown_target(
    op: &mut LogicalOp,
) -> Option<&mut LogicalOp> {
    match op {
        LogicalOp::Project { input, .. } => find_pushdown_target(input),
        LogicalOp::Expand { var_length, .. } if *var_length => Some(op),
        _ => None,
    }
}

/// Apply `f` to each direct child operator (skipping non-LogicalOp fields).
pub(in crate::cypher::planner) fn walk_children_mut(op: &mut LogicalOp, f: fn(&mut LogicalOp)) {
    match op {
        LogicalOp::Expand { input, .. }
        | LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Aggregate { input, .. }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Distinct { input }
        | LogicalOp::Skip { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::MatchCreate { input, .. }
        | LogicalOp::Delete { input, .. }
        | LogicalOp::SetProperty { input, .. }
        | LogicalOp::SetLabel { input, .. }
        | LogicalOp::SetProperties { input, .. }
        | LogicalOp::Remove { input, .. }
        | LogicalOp::MatchMerge { input, .. }
        | LogicalOp::MaterializePath { input, .. }
        | LogicalOp::Unwind { input, .. }
        | LogicalOp::Call { input, .. }
        | LogicalOp::ShortestPath { input, .. } => f(input),
        LogicalOp::CrossProduct { left, right, .. } => {
            f(left);
            f(right);
        }
        LogicalOp::CorrelatedJoin { input, right, .. }
        | LogicalOp::LeftOuterJoin { input, right, .. } => {
            f(input);
            f(right);
        }
        LogicalOp::Union { inputs, .. } => {
            for inp in inputs {
                f(inp);
            }
        }
        LogicalOp::CreateSequence { ops } => {
            for inner in ops {
                f(inner);
            }
        }
        LogicalOp::SingleRow
        | LogicalOp::Scan { .. }
        | LogicalOp::IndexLookup { .. }
        | LogicalOp::IdLookup { .. }
        | LogicalOp::FullTextLookup { .. }
        | LogicalOp::CreateNode { .. }
        | LogicalOp::CreateEdge { .. }
        | LogicalOp::Merge { .. }
        | LogicalOp::EmptyRow => {}
    }
}

/// Plan a statement that may be inside a subquery (EXISTS).
/// Subquery context disables certain validations that require full scope.
pub fn plan_subquery(conn: &Connection, stmt: &Statement) -> crate::types::Result<LogicalOp> {
    let mut op = plan_inner(conn, stmt, true)?;
    apply_post_passes(conn, &mut op);
    Ok(op)
}

/// Plan a statement with a procedure registry for CALL validation.
pub fn plan_with_procedures(
    conn: &Connection,
    stmt: &Statement,
    procedures: &crate::cypher::procedure::ProcedureRegistry,
    params: Option<&std::collections::HashMap<String, Value>>,
) -> crate::types::Result<LogicalOp> {
    match stmt {
        Statement::Call {
            procedure_name,
            args,
            implicit_args,
            yield_items,
            yield_star,
            return_clause,
            order_by,
            skip,
            limit,
        } => plan_call(
            conn,
            procedure_name,
            args,
            *implicit_args,
            yield_items.as_deref(),
            *yield_star,
            return_clause.as_ref(),
            order_by,
            skip.as_deref(),
            limit.as_deref(),
            procedures,
            params,
        ),
        Statement::Explain(inner) => plan_with_procedures(conn, inner, procedures, params),
        _ => plan(conn, stmt),
    }
    .map(|mut op| {
        apply_post_passes(conn, &mut op);
        op
    })
}

#[cfg(test)]
mod limit_pushdown_tests {
    use super::*;
    use crate::cypher::parser;
    use rusqlite::Connection;

    fn plan_query(query: &str) -> LogicalOp {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        let stmt = parser::parse(query).unwrap();
        plan(&conn, &stmt).unwrap()
    }

    fn find_var_length_expand(op: &LogicalOp) -> Option<&LogicalOp> {
        match op {
            LogicalOp::Expand {
                var_length: true, ..
            } => Some(op),
            LogicalOp::Limit { input, .. }
            | LogicalOp::Project { input, .. }
            | LogicalOp::Filter { input, .. }
            | LogicalOp::Sort { input, .. }
            | LogicalOp::Distinct { input } => find_var_length_expand(input),
            _ => None,
        }
    }

    #[test]
    fn pushdown_applies_to_simple_var_length_with_limit() {
        let plan = plan_query("MATCH (a)-[*1..3]->(b) RETURN b LIMIT 10");
        let expand = find_var_length_expand(&plan).expect("expected var-length Expand");
        let LogicalOp::Expand { result_cap, .. } = expand else {
            unreachable!()
        };
        assert_eq!(*result_cap, Some(10));
    }

    #[test]
    fn pushdown_skipped_when_sort_intervenes() {
        let plan = plan_query("MATCH (a)-[*1..3]->(b) RETURN b ORDER BY b LIMIT 10");
        let expand = find_var_length_expand(&plan).expect("expected var-length Expand");
        let LogicalOp::Expand { result_cap, .. } = expand else {
            unreachable!()
        };
        assert_eq!(
            *result_cap, None,
            "Sort between Limit and Expand should block pushdown"
        );
    }

    #[test]
    fn pushdown_skipped_for_fixed_length_expand() {
        let plan = plan_query("MATCH (a)-[r]->(b) RETURN b LIMIT 10");
        // Fixed-length Expand is not a pushdown target; just verify no crash.
        match &plan {
            LogicalOp::Limit { input, count } => {
                assert_eq!(*count, 10);
                // Walk down — any Expand we find should have result_cap=None.
                fn check(op: &LogicalOp) {
                    if let LogicalOp::Expand { result_cap, .. } = op {
                        assert_eq!(*result_cap, None);
                    }
                    match op {
                        LogicalOp::Project { input, .. }
                        | LogicalOp::Expand { input, .. }
                        | LogicalOp::Filter { input, .. } => check(input),
                        _ => {}
                    }
                }
                check(input);
            }
            _ => panic!("expected Limit at top, got {:?}", plan.op_name()),
        }
    }
}

#[cfg(test)]
mod plan_tests {
    use super::plan;
    use crate::cypher::ir::*;
    use crate::cypher::parser::parse;
    use crate::types::Direction;
    use rusqlite::Connection;

    fn plan_query(q: &str) -> LogicalOp {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        let stmt = parse(q).unwrap();
        plan(&conn, &stmt).unwrap()
    }

    #[test]
    fn plan_simple_scan() {
        let op = plan_query("MATCH (n:Person) RETURN n");
        match op {
            LogicalOp::Project { input, items, .. } => {
                assert_eq!(items.len(), 1);
                match *input {
                    LogicalOp::Scan {
                        ref label,
                        ref alias,
                    } => {
                        assert_eq!(label, "Person");
                        assert_eq!(alias, "n");
                    }
                    _ => panic!("expected Scan, got {input:?}"),
                }
            }
            _ => panic!("expected Project, got {op:?}"),
        }
    }

    #[test]
    fn plan_scan_with_expand() {
        let op = plan_query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b");
        match op {
            LogicalOp::Project { input, .. } => match *input {
                LogicalOp::Filter { input, .. } => match *input {
                    LogicalOp::Expand {
                        ref src_alias,
                        ref dst_alias,
                        ref edge_types,
                        direction,
                        min_hops,
                        max_hops,
                        ..
                    } => {
                        assert_eq!(src_alias, "a");
                        assert_eq!(dst_alias, "b");
                        assert_eq!(edge_types.first().map(|s| s.as_str()), Some("KNOWS"));
                        assert_eq!(direction, Direction::Outgoing);
                        assert_eq!(min_hops, 1);
                        assert_eq!(max_hops, 1);
                    }
                    _ => panic!("expected Expand"),
                },
                _ => panic!("expected Filter for destination label"),
            },
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn plan_variable_length_expand() {
        let op = plan_query("MATCH (a)-[:CALLS*1..5]->(b) RETURN b");
        match op {
            LogicalOp::Project { input, .. } => match *input {
                LogicalOp::Expand {
                    min_hops, max_hops, ..
                } => {
                    assert_eq!(min_hops, 1);
                    assert_eq!(max_hops, 5);
                }
                _ => panic!("expected Expand"),
            },
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn plan_with_filter() {
        let op = plan_query("MATCH (n:Person) WHERE n.age = 30 RETURN n");
        match op {
            LogicalOp::Project { input, .. } => match *input {
                LogicalOp::Filter { .. } => {}
                _ => panic!("expected Filter"),
            },
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn plan_with_aggregate() {
        let op = plan_query("MATCH (n:Person) RETURN count(*) AS cnt");
        match op {
            LogicalOp::Project { input, .. } => match *input {
                LogicalOp::Aggregate { ref aggregates, .. } => {
                    assert_eq!(aggregates.len(), 1);
                    assert_eq!(aggregates[0].function, AggregateFunction::Count);
                }
                _ => panic!("expected Aggregate"),
            },
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn plan_with_order_by_and_limit() {
        let op = plan_query("MATCH (n:Person) RETURN n.name ORDER BY n.name LIMIT 5");
        match op {
            LogicalOp::Limit { input, count } => {
                assert_eq!(count, 5);
                match *input {
                    LogicalOp::Project { input, .. } => match *input {
                        LogicalOp::Sort { .. } => {}
                        _ => panic!("expected Sort"),
                    },
                    _ => panic!("expected Project"),
                }
            }
            _ => panic!("expected Limit"),
        }
    }

    #[test]
    fn plan_create_node() {
        let op = plan_query("CREATE (n:Person {name: 'Alice'})");
        match op {
            LogicalOp::CreateNode {
                labels,
                alias,
                properties,
            } => {
                assert_eq!(labels, vec!["Person".to_string()]);
                assert_eq!(alias.as_deref(), Some("n"));
                assert_eq!(properties.len(), 1);
            }
            _ => panic!("expected CreateNode, got {op:?}"),
        }
    }

    #[test]
    fn plan_create_edge() {
        let op = plan_query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})");
        match op {
            LogicalOp::CreateSequence { ref ops } => {
                assert_eq!(ops.len(), 3);
                assert!(matches!(ops[0], LogicalOp::CreateNode { .. }));
                assert!(matches!(ops[1], LogicalOp::CreateNode { .. }));
                assert!(matches!(ops[2], LogicalOp::CreateEdge { .. }));
            }
            _ => panic!("expected CreateSequence, got {op:?}"),
        }
    }

    #[test]
    fn plan_delete() {
        let op = plan_query("MATCH (n:Person) WHERE n.name = 'Alice' DELETE n");
        match op {
            LogicalOp::Delete { exprs, .. } => {
                assert_eq!(exprs.len(), 1);
                assert!(matches!(
                    &exprs[0].kind,
                    crate::cypher::ast::ExprKind::Variable(v) if v == "n"
                ));
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn plan_set_property() {
        let op = plan_query("MATCH (n:Person) WHERE n.name = 'Alice' SET n.age = 31");
        match op {
            LogicalOp::SetProperty { assignments, .. } => {
                assert_eq!(assignments.len(), 1);
                assert_eq!(assignments[0].property, "age");
            }
            _ => panic!("expected SetProperty"),
        }
    }

    #[test]
    fn plan_merge() {
        let op = plan_query(
            "MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.seen = true",
        );
        match op {
            LogicalOp::Merge {
                on_create,
                on_match,
                ..
            } => {
                assert_eq!(on_create.len(), 1);
                assert_eq!(on_match.len(), 1);
            }
            _ => panic!("expected Merge"),
        }
    }
}

// === planner split: submodule declarations ===

mod helpers;
mod multi;
mod pattern;
mod statement;
mod validation;

pub use pattern::plan_patterns;

// Sibling fns called from mod.rs's public planners.
use statement::{plan_call, plan_inner};
