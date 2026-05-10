//! Pest error humanization and `$param` reference validation.
//!
//! Phase 2 of `plans/plan-cache.md` removed the AST-rewriting parameter
//! substitution that used to run before planning — the planner now treats
//! `ExprKind::Parameter` as opaque and the executor resolves it at eval
//! time via `eval::ParamScope`. This module retains a no-alloc validation
//! walker so missing `$name` references still fail fast with a
//! `SemanticAnalysis` error.

use crate::types::{GraphError, Result, Value};

use super::*;

pub(in crate::cypher::parser) fn humanize_rule_name(rule: &str) -> &str {
    match rule {
        "statement" | "union_stmt" | "single_stmt" | "explain_stmt" => {
            "a Cypher statement (MATCH, CREATE, DELETE, MERGE, EXPLAIN ...)"
        }
        "union_op" => "UNION or UNION ALL",
        "expr" | "add_expr" | "mul_expr" | "atom_expr" => {
            "an expression (property, literal, or function call)"
        }
        "bool_expr" | "bool_primary" | "bool_factor" | "bool_term" | "xor_term" => "a condition",
        "comparison" => "a comparison (=, <>, <, >, <=, >=)",
        "ident" => "an identifier",
        "pattern" | "pattern_list" | "pattern_item" => "a graph pattern like (n:Label)",
        "path_pattern" | "shortest_path_fn" | "all_shortest_paths_fn" => {
            "a path pattern like p = shortestPath((a)-[*]->(b))"
        }
        "node_pattern" => "a node pattern like (n:Label {prop: value})",
        "rel_pattern" | "rel_right" | "rel_left" | "rel_undirected" => {
            "a relationship pattern like -[:TYPE]->"
        }
        "distinct_keyword" => "DISTINCT",
        "return_clause" => "a RETURN clause",
        "return_items" | "return_item" => "a RETURN expression",
        "where_clause" => "a WHERE clause",
        "with_clause" => "a WITH clause",
        "order_by_clause" => "an ORDER BY clause",
        "skip_clause" => "a SKIP clause",
        "limit_clause" => "a LIMIT clause",
        "literal" => "a value (string, number, boolean, or null)",
        "integer_literal" | "integer" => "an integer",
        "float_literal" => "a number",
        "string_literal" => "a string (e.g. 'hello')",
        "property_access" => "a property access like n.name",
        "property_map" => "a property map like {name: 'Alice'}",
        "label_spec" => "a label like :Person",
        "function_call" => "a function call like count(*)",
        "case_when_clause" => "a WHEN condition",
        "case_else_clause" => "an ELSE value",
        "case_expr" => "a CASE expression",
        "exists_subquery" => "an EXISTS { } subquery",
        "exists_full_subquery" => "an EXISTS { MATCH ... } subquery",
        "pattern_comprehension" => "a pattern comprehension like [(n)-->() | expr]",
        "list_comprehension" => "a list comprehension like [x IN list | expr]",
        "comp_op" => "a comparison operator (=, <>, <, >)",
        "alias" => "an alias (AS name)",
        "assignment" | "assignment_list" | "property_assignment_list" => {
            "a property assignment like n.prop = value"
        }
        "set_item" | "set_label" | "set_map" | "set_map_merge" => {
            "a SET item (property, label, or map)"
        }
        "detach_keyword" => "DETACH",
        "EOI" => "end of query",
        "symbolic_name" => "a name (e.g. Person, alice, my_label)",
        "multi_create_clause" => "a CREATE clause",
        "multi_merge_clause" => "a MERGE clause",
        "multi_unwind_clause" | "unwind_clause" => "an UNWIND clause",
        "multi_call_clause" => "a CALL clause",
        "multi_set_clause" => "a SET clause",
        "multi_remove_clause" => "a REMOVE clause",
        "optional_match_clause" => "an OPTIONAL MATCH clause",
        "delete_clause" => "a DELETE clause",
        "match_clause" => "a MATCH clause",
        _ => rule,
    }
}

/// Convert a pest parse error into a human-friendly error message.
pub(in crate::cypher::parser) fn humanize_pest_error(err: pest::error::Error<Rule>) -> String {
    let renamed = err.renamed_rules(|rule| humanize_rule_name(&format!("{rule:?}")).to_string());
    format!("{renamed}")
}

// ---------------------------------------------------------------------------
// Parameter reference validation (no-alloc walker).
// ---------------------------------------------------------------------------

#[inline]
fn check_param(name: &str, params: &HashMap<String, Value>) -> Result<()> {
    if params.contains_key(name) {
        Ok(())
    } else {
        Err(GraphError::argument(
            crate::types::QueryPhase::SemanticAnalysis,
            format!("missing parameter: ${name}"),
        ))
    }
}

fn validate_expr(expr: &Expr, params: &HashMap<String, Value>) -> Result<()> {
    match &expr.kind {
        ExprKind::Parameter(name) => check_param(name, params),
        ExprKind::BinaryOp { left, right, .. } => {
            validate_expr(left, params)?;
            validate_expr(right, params)
        }
        ExprKind::Not(inner) | ExprKind::IsNull(inner) | ExprKind::IsNotNull(inner) => {
            validate_expr(inner, params)
        }
        ExprKind::FunctionCall { args, .. } => {
            for a in args {
                validate_expr(a, params)?;
            }
            Ok(())
        }
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            if let Some(o) = operand {
                validate_expr(o, params)?;
            }
            for (cond, result) in alternatives {
                validate_expr(cond, params)?;
                validate_expr(result, params)?;
            }
            if let Some(d) = default {
                validate_expr(d, params)?;
            }
            Ok(())
        }
        ExprKind::List(items) => {
            for e in items {
                validate_expr(e, params)?;
            }
            Ok(())
        }
        ExprKind::ListComprehension {
            list_expr,
            filter,
            map_expr,
            ..
        } => {
            validate_expr(list_expr, params)?;
            if let Some(f) = filter {
                validate_expr(f, params)?;
            }
            if let Some(m) = map_expr {
                validate_expr(m, params)?;
            }
            Ok(())
        }
        ExprKind::PatternComprehension {
            pattern,
            where_clause,
            map_expr,
            ..
        } => {
            validate_pattern(pattern, params)?;
            if let Some(w) = where_clause {
                validate_expr(w, params)?;
            }
            validate_expr(map_expr, params)
        }
        ExprKind::Quantifier {
            list_expr,
            predicate,
            ..
        } => {
            validate_expr(list_expr, params)?;
            validate_expr(predicate, params)
        }
        ExprKind::Exists {
            patterns,
            where_clause,
        } => {
            for p in patterns {
                validate_pattern(p, params)?;
            }
            if let Some(w) = where_clause {
                validate_expr(w, params)?;
            }
            Ok(())
        }
        ExprKind::ExistsSubquery(stmt) => validate_params(stmt, params),
        ExprKind::MapLiteral(pairs) => {
            for (_, v) in pairs {
                validate_expr(v, params)?;
            }
            Ok(())
        }
        ExprKind::Index { expr: e, index } => {
            validate_expr(e, params)?;
            validate_expr(index, params)
        }
        ExprKind::DotAccess { expr: e, .. } => validate_expr(e, params),
        ExprKind::Slice {
            expr: e,
            start,
            end,
        } => {
            validate_expr(e, params)?;
            if let Some(s) = start {
                validate_expr(s, params)?;
            }
            if let Some(en) = end {
                validate_expr(en, params)?;
            }
            Ok(())
        }
        ExprKind::Literal(_)
        | ExprKind::Property(_, _)
        | ExprKind::Variable(_)
        | ExprKind::HasLabel(_, _)
        | ExprKind::PatternPredicate(_)
        | ExprKind::Star => Ok(()),
    }
}

fn validate_props(
    properties: &HashMap<String, Expr>,
    params: &HashMap<String, Value>,
) -> Result<()> {
    for v in properties.values() {
        validate_expr(v, params)?;
    }
    Ok(())
}

fn validate_pattern(pattern: &Pattern, params: &HashMap<String, Value>) -> Result<()> {
    for el in &pattern.elements {
        match el {
            PatternElement::Node(n) => validate_props(&n.properties, params)?,
            PatternElement::Relationship(r) => validate_props(&r.properties, params)?,
        }
    }
    Ok(())
}

fn validate_patterns(patterns: &[Pattern], params: &HashMap<String, Value>) -> Result<()> {
    for p in patterns {
        validate_pattern(p, params)?;
    }
    Ok(())
}

fn validate_optional_match(om: &OptionalMatch, params: &HashMap<String, Value>) -> Result<()> {
    validate_patterns(&om.patterns, params)?;
    if let Some(e) = &om.where_clause {
        validate_expr(e, params)?;
    }
    Ok(())
}

fn validate_optional_matches(oms: &[OptionalMatch], params: &HashMap<String, Value>) -> Result<()> {
    for om in oms {
        validate_optional_match(om, params)?;
    }
    Ok(())
}

fn validate_set_items(items: &[SetItem], params: &HashMap<String, Value>) -> Result<()> {
    for item in items {
        match item {
            SetItem::Property(a) => validate_expr(&a.value, params)?,
            SetItem::MapOverwrite { value, .. } | SetItem::MapMerge { value, .. } => {
                validate_expr(value, params)?
            }
            SetItem::Label { .. } => {}
        }
    }
    Ok(())
}

fn validate_return_items(items: &[ReturnItem], params: &HashMap<String, Value>) -> Result<()> {
    for item in items {
        validate_expr(&item.expr, params)?;
    }
    Ok(())
}

fn validate_sort_items(items: &[SortItem], params: &HashMap<String, Value>) -> Result<()> {
    for item in items {
        validate_expr(&item.expr, params)?;
    }
    Ok(())
}

fn validate_return_tail(
    return_clause: &Option<ReturnClause>,
    order_by: &[SortItem],
    skip: &Option<Expr>,
    limit: &Option<Expr>,
    params: &HashMap<String, Value>,
) -> Result<()> {
    if let Some(rc) = return_clause {
        validate_return_items(&rc.items, params)?;
    }
    validate_sort_items(order_by, params)?;
    if let Some(e) = skip {
        validate_expr(e, params)?;
    }
    if let Some(e) = limit {
        validate_expr(e, params)?;
    }
    Ok(())
}

fn validate_intermediate_clauses(
    clauses: &[IntermediateClause],
    params: &HashMap<String, Value>,
) -> Result<()> {
    for c in clauses {
        match c {
            IntermediateClause::With(w) => {
                validate_return_items(&w.items, params)?;
                validate_sort_items(&w.order_by, params)?;
                if let Some(e) = &w.skip {
                    validate_expr(e, params)?;
                }
                if let Some(e) = &w.limit {
                    validate_expr(e, params)?;
                }
                if let Some(e) = &w.where_clause {
                    validate_expr(e, params)?;
                }
            }
            IntermediateClause::Unwind(u) => validate_expr(&u.expr, params)?,
            IntermediateClause::Match(m) => {
                validate_patterns(&m.patterns, params)?;
                validate_optional_matches(&m.optional_patterns, params)?;
                if let Some(e) = &m.where_clause {
                    validate_expr(e, params)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_clause(clause: &Clause, params: &HashMap<String, Value>) -> Result<()> {
    match clause {
        Clause::Match {
            patterns,
            optional_patterns,
            where_clause,
        } => {
            validate_patterns(patterns, params)?;
            validate_optional_matches(optional_patterns, params)?;
            if let Some(e) = where_clause {
                validate_expr(e, params)?;
            }
            Ok(())
        }
        Clause::Create { patterns } => validate_patterns(patterns, params),
        Clause::Merge {
            pattern,
            on_create,
            on_match,
        } => {
            validate_pattern(pattern, params)?;
            validate_set_items(on_create, params)?;
            validate_set_items(on_match, params)
        }
        Clause::With(w) => {
            validate_return_items(&w.items, params)?;
            validate_sort_items(&w.order_by, params)?;
            if let Some(e) = &w.skip {
                validate_expr(e, params)?;
            }
            if let Some(e) = &w.limit {
                validate_expr(e, params)?;
            }
            if let Some(e) = &w.where_clause {
                validate_expr(e, params)?;
            }
            Ok(())
        }
        Clause::Unwind(u) => validate_expr(&u.expr, params),
        Clause::Set { items } => validate_set_items(items, params),
        Clause::Remove { .. } => Ok(()),
        Clause::Call { args, .. } => {
            for a in args {
                validate_expr(a, params)?;
            }
            Ok(())
        }
        Clause::Delete { exprs, .. } => {
            for e in exprs {
                validate_expr(e, params)?;
            }
            Ok(())
        }
    }
}

/// Validate that every `$name` reference in `stmt` has a matching entry in `params`.
///
/// This is a no-alloc walker — it never clones the AST. Run before planning so
/// queries with missing parameters fail fast at SemanticAnalysis even when no
/// row would actually evaluate the reference. Parameter values themselves are
/// resolved at eval time via `eval::ParamScope` (see `plans/plan-cache.md`).
pub fn validate_params(stmt: &Statement, params: &HashMap<String, Value>) -> Result<()> {
    match stmt {
        Statement::Match(m) => {
            validate_patterns(&m.patterns, params)?;
            validate_optional_matches(&m.optional_patterns, params)?;
            if let Some(e) = &m.where_clause {
                validate_expr(e, params)?;
            }
            validate_intermediate_clauses(&m.intermediate_clauses, params)?;
            validate_return_items(&m.return_clause.items, params)?;
            validate_sort_items(&m.order_by, params)?;
            if let Some(e) = &m.skip {
                validate_expr(e, params)?;
            }
            if let Some(e) = &m.limit {
                validate_expr(e, params)?;
            }
            Ok(())
        }
        Statement::Create(c) => {
            validate_patterns(&c.patterns, params)?;
            validate_return_tail(&c.return_clause, &c.order_by, &c.skip, &c.limit, params)
        }
        Statement::MatchCreate(mc) => {
            validate_patterns(&mc.patterns, params)?;
            if let Some(e) = &mc.where_clause {
                validate_expr(e, params)?;
            }
            validate_patterns(&mc.create_patterns, params)?;
            validate_return_tail(&mc.return_clause, &mc.order_by, &mc.skip, &mc.limit, params)
        }
        Statement::MatchMerge(mm) => {
            validate_patterns(&mm.patterns, params)?;
            if let Some(e) = &mm.where_clause {
                validate_expr(e, params)?;
            }
            validate_pattern(&mm.merge_pattern, params)?;
            validate_set_items(&mm.on_create, params)?;
            validate_set_items(&mm.on_match, params)?;
            validate_return_tail(&mm.return_clause, &mm.order_by, &mm.skip, &mm.limit, params)
        }
        Statement::Delete(d) => {
            validate_patterns(&d.patterns, params)?;
            validate_optional_matches(&d.optional_patterns, params)?;
            if let Some(e) = &d.where_clause {
                validate_expr(e, params)?;
            }
            for e in &d.exprs {
                validate_expr(e, params)?;
            }
            validate_return_tail(&d.return_clause, &d.order_by, &d.skip, &d.limit, params)
        }
        Statement::Set(s) => {
            validate_patterns(&s.patterns, params)?;
            validate_optional_matches(&s.optional_patterns, params)?;
            if let Some(e) = &s.where_clause {
                validate_expr(e, params)?;
            }
            validate_set_items(&s.items, params)?;
            validate_intermediate_clauses(&s.intermediate_clauses, params)?;
            validate_return_tail(&s.return_clause, &s.order_by, &s.skip, &s.limit, params)
        }
        Statement::Remove(r) => {
            validate_patterns(&r.patterns, params)?;
            validate_optional_matches(&r.optional_patterns, params)?;
            if let Some(e) = &r.where_clause {
                validate_expr(e, params)?;
            }
            validate_return_tail(&r.return_clause, &r.order_by, &r.skip, &r.limit, params)
        }
        Statement::Merge(m) => {
            validate_pattern(&m.pattern, params)?;
            validate_set_items(&m.on_create, params)?;
            validate_set_items(&m.on_match, params)?;
            validate_return_tail(&m.return_clause, &m.order_by, &m.skip, &m.limit, params)
        }
        Statement::Unwind(u) => {
            validate_expr(&u.expr, params)?;
            match &u.body {
                UnwindBody::Return {
                    where_clause,
                    intermediate_clauses,
                    return_clause,
                    order_by,
                    skip,
                    limit,
                } => {
                    if let Some(e) = where_clause {
                        validate_expr(e, params)?;
                    }
                    validate_intermediate_clauses(intermediate_clauses, params)?;
                    validate_return_items(&return_clause.items, params)?;
                    validate_sort_items(order_by, params)?;
                    if let Some(e) = skip {
                        validate_expr(e, params)?;
                    }
                    if let Some(e) = limit {
                        validate_expr(e, params)?;
                    }
                    Ok(())
                }
                UnwindBody::Create {
                    patterns,
                    intermediate_clauses,
                    return_clause,
                    order_by,
                    skip,
                    limit,
                } => {
                    validate_patterns(patterns, params)?;
                    validate_intermediate_clauses(intermediate_clauses, params)?;
                    validate_return_tail(return_clause, order_by, skip, limit, params)
                }
            }
        }
        Statement::Return(r) => {
            validate_return_items(&r.return_clause.items, params)?;
            validate_sort_items(&r.order_by, params)?;
            if let Some(e) = &r.skip {
                validate_expr(e, params)?;
            }
            if let Some(e) = &r.limit {
                validate_expr(e, params)?;
            }
            Ok(())
        }
        Statement::MultiClause(mc) => {
            for c in &mc.clauses {
                validate_clause(c, params)?;
            }
            if let Some(rc) = &mc.return_clause {
                validate_return_items(&rc.items, params)?;
            }
            validate_sort_items(&mc.order_by, params)?;
            if let Some(e) = &mc.skip {
                validate_expr(e, params)?;
            }
            if let Some(e) = &mc.limit {
                validate_expr(e, params)?;
            }
            Ok(())
        }
        Statement::Explain(inner) => validate_params(inner, params),
        Statement::Union { statements, .. } => {
            for s in statements {
                validate_params(s, params)?;
            }
            Ok(())
        }
        Statement::Call {
            args,
            return_clause,
            order_by,
            skip,
            limit,
            ..
        } => {
            for a in args {
                validate_expr(a, params)?;
            }
            if let Some(rc) = return_clause {
                validate_return_items(&rc.items, params)?;
            }
            validate_sort_items(order_by, params)?;
            if let Some(e) = skip {
                validate_expr(e, params)?;
            }
            if let Some(e) = limit {
                validate_expr(e, params)?;
            }
            Ok(())
        }
    }
}
