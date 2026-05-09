//! Parameter resolution and pest error humanization.

use crate::types::{GraphError, Span, Value};

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
// Parameter resolution: substitute $name with literal values before planning.
// ---------------------------------------------------------------------------

/// Convert a Value to an Expr, handling all types including List and Map.
/// `span` is the source span of the parameter being substituted, so the
/// resulting literal carries the parameter's location for error messages.
pub(in crate::cypher::parser) fn value_to_expr(
    val: &Value,
    span: Span,
) -> crate::types::Result<Expr> {
    match val {
        Value::Null => Ok(Expr::new(ExprKind::Literal(LiteralValue::Null), span)),
        Value::Bool(b) => Ok(Expr::new(ExprKind::Literal(LiteralValue::Bool(*b)), span)),
        Value::I64(n) => Ok(Expr::new(ExprKind::Literal(LiteralValue::I64(*n)), span)),
        Value::F64(n) => Ok(Expr::new(ExprKind::Literal(LiteralValue::F64(*n)), span)),
        Value::String(s) => Ok(Expr::new(
            ExprKind::Literal(LiteralValue::String(s.clone())),
            span,
        )),
        Value::List(items) => {
            let exprs: crate::types::Result<Vec<Expr>> =
                items.iter().map(|v| value_to_expr(v, span)).collect();
            Ok(Expr::new(ExprKind::List(exprs?), span))
        }
        Value::Map(map) => {
            let pairs: crate::types::Result<Vec<(String, Expr)>> = map
                .iter()
                .map(|(k, v)| value_to_expr(v, span).map(|e| (k.clone(), e)))
                .collect();
            Ok(Expr::new(ExprKind::MapLiteral(pairs?), span))
        }
        _ => Err(GraphError::argument(
            crate::types::QueryPhase::SemanticAnalysis,
            "unsupported parameter type",
        )),
    }
}

pub(in crate::cypher::parser) fn resolve_expr(
    expr: &Expr,
    params: &HashMap<String, Value>,
) -> crate::types::Result<Expr> {
    let span = expr.span;
    match &expr.kind {
        ExprKind::Parameter(name) => {
            let val = params.get(name).ok_or_else(|| {
                GraphError::argument(
                    crate::types::QueryPhase::SemanticAnalysis,
                    format!("missing parameter: ${name}"),
                )
            })?;
            value_to_expr(val, span)
        }
        ExprKind::BinaryOp { left, op, right } => Ok(Expr::new(
            ExprKind::BinaryOp {
                left: Box::new(resolve_expr(left, params)?),
                op: *op,
                right: Box::new(resolve_expr(right, params)?),
            },
            span,
        )),
        ExprKind::Not(inner) => Ok(Expr::new(
            ExprKind::Not(Box::new(resolve_expr(inner, params)?)),
            span,
        )),
        ExprKind::IsNull(inner) => Ok(Expr::new(
            ExprKind::IsNull(Box::new(resolve_expr(inner, params)?)),
            span,
        )),
        ExprKind::IsNotNull(inner) => Ok(Expr::new(
            ExprKind::IsNotNull(Box::new(resolve_expr(inner, params)?)),
            span,
        )),
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => {
            let resolved: crate::types::Result<Vec<Expr>> =
                args.iter().map(|a| resolve_expr(a, params)).collect();
            Ok(Expr::new(
                ExprKind::FunctionCall {
                    name: name.clone(),
                    args: resolved?,
                    distinct: *distinct,
                    original_text: original_text.clone(),
                },
                span,
            ))
        }
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            let resolved_operand = operand
                .as_ref()
                .map(|o| resolve_expr(o, params).map(Box::new))
                .transpose()?;
            let mut resolved_alts = Vec::new();
            for (cond, result) in alternatives {
                resolved_alts.push((
                    Box::new(resolve_expr(cond, params)?),
                    Box::new(resolve_expr(result, params)?),
                ));
            }
            let resolved_default = default
                .as_ref()
                .map(|d| resolve_expr(d, params).map(Box::new))
                .transpose()?;
            Ok(Expr::new(
                ExprKind::Case {
                    operand: resolved_operand,
                    alternatives: resolved_alts,
                    default: resolved_default,
                },
                span,
            ))
        }
        ExprKind::List(items) => {
            let resolved: crate::types::Result<Vec<Expr>> =
                items.iter().map(|e| resolve_expr(e, params)).collect();
            Ok(Expr::new(ExprKind::List(resolved?), span))
        }
        ExprKind::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => Ok(Expr::new(
            ExprKind::ListComprehension {
                variable: variable.clone(),
                list_expr: Box::new(resolve_expr(list_expr, params)?),
                filter: filter
                    .as_ref()
                    .map(|f| resolve_expr(f, params).map(Box::new))
                    .transpose()?,
                map_expr: map_expr
                    .as_ref()
                    .map(|m| resolve_expr(m, params).map(Box::new))
                    .transpose()?,
            },
            span,
        )),
        ExprKind::PatternComprehension {
            path_variable,
            pattern,
            where_clause,
            map_expr,
        } => Ok(Expr::new(
            ExprKind::PatternComprehension {
                path_variable: path_variable.clone(),
                pattern: resolve_pattern(pattern, params)?,
                where_clause: where_clause
                    .as_ref()
                    .map(|w| resolve_expr(w, params).map(Box::new))
                    .transpose()?,
                map_expr: Box::new(resolve_expr(map_expr, params)?),
            },
            span,
        )),
        ExprKind::Quantifier {
            kind,
            variable,
            list_expr,
            predicate,
        } => Ok(Expr::new(
            ExprKind::Quantifier {
                kind: *kind,
                variable: variable.clone(),
                list_expr: Box::new(resolve_expr(list_expr, params)?),
                predicate: Box::new(resolve_expr(predicate, params)?),
            },
            span,
        )),
        ExprKind::Exists {
            patterns,
            where_clause,
        } => Ok(Expr::new(
            ExprKind::Exists {
                patterns: resolve_patterns(patterns, params)?,
                where_clause: where_clause
                    .as_ref()
                    .map(|w| resolve_expr(w, params).map(Box::new))
                    .transpose()?,
            },
            span,
        )),
        ExprKind::ExistsSubquery(stmt) => {
            // Parameters inside the subquery statement are resolved via
            // resolve_params which handles all statement types.
            Ok(Expr::new(
                ExprKind::ExistsSubquery(Box::new(resolve_params(stmt, params)?)),
                span,
            ))
        }
        ExprKind::MapLiteral(pairs) => {
            let resolved: crate::types::Result<Vec<(String, Expr)>> = pairs
                .iter()
                .map(|(k, v)| resolve_expr(v, params).map(|r| (k.clone(), r)))
                .collect();
            Ok(Expr::new(ExprKind::MapLiteral(resolved?), span))
        }
        ExprKind::Index { expr: e, index } => Ok(Expr::new(
            ExprKind::Index {
                expr: Box::new(resolve_expr(e, params)?),
                index: Box::new(resolve_expr(index, params)?),
            },
            span,
        )),
        ExprKind::DotAccess { expr: e, key } => Ok(Expr::new(
            ExprKind::DotAccess {
                expr: Box::new(resolve_expr(e, params)?),
                key: key.clone(),
            },
            span,
        )),
        ExprKind::Slice {
            expr: e,
            start,
            end,
        } => Ok(Expr::new(
            ExprKind::Slice {
                expr: Box::new(resolve_expr(e, params)?),
                start: start
                    .as_ref()
                    .map(|s| resolve_expr(s, params).map(Box::new))
                    .transpose()?,
                end: end
                    .as_ref()
                    .map(|e_val| resolve_expr(e_val, params).map(Box::new))
                    .transpose()?,
            },
            span,
        )),
        // Leaf nodes that contain no sub-expressions.
        ExprKind::Literal(_)
        | ExprKind::Property(_, _)
        | ExprKind::Variable(_)
        | ExprKind::HasLabel(_, _)
        | ExprKind::PatternPredicate(_)
        | ExprKind::Star => Ok(expr.clone()),
    }
}

pub(in crate::cypher::parser) fn resolve_props(
    properties: &HashMap<String, Expr>,
    params: &HashMap<String, Value>,
) -> crate::types::Result<HashMap<String, Expr>> {
    properties
        .iter()
        .map(|(k, v)| Ok((k.clone(), resolve_expr(v, params)?)))
        .collect()
}

pub(in crate::cypher::parser) fn resolve_pattern(
    pattern: &Pattern,
    params: &HashMap<String, Value>,
) -> crate::types::Result<Pattern> {
    let elements: crate::types::Result<Vec<PatternElement>> = pattern
        .elements
        .iter()
        .map(|el| match el {
            PatternElement::Node(n) => Ok(PatternElement::Node(NodePattern {
                variable: n.variable.clone(),
                labels: n.labels.clone(),
                properties: resolve_props(&n.properties, params)?,
            })),
            PatternElement::Relationship(r) => Ok(PatternElement::Relationship(RelPattern {
                variable: r.variable.clone(),
                rel_types: r.rel_types.clone(),
                properties: resolve_props(&r.properties, params)?,
                direction: r.direction,
                var_length: r.var_length,
            })),
        })
        .collect();
    Ok(Pattern {
        elements: elements?,
        path_variable: pattern.path_variable.clone(),
        shortest_path_mode: pattern.shortest_path_mode,
    })
}

pub(in crate::cypher::parser) fn resolve_patterns(
    patterns: &[Pattern],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<Pattern>> {
    patterns
        .iter()
        .map(|p| resolve_pattern(p, params))
        .collect()
}

pub(in crate::cypher::parser) fn resolve_optional_match(
    om: &OptionalMatch,
    params: &HashMap<String, Value>,
) -> crate::types::Result<OptionalMatch> {
    Ok(OptionalMatch {
        patterns: resolve_patterns(&om.patterns, params)?,
        where_clause: om
            .where_clause
            .as_ref()
            .map(|e| resolve_expr(e, params))
            .transpose()?,
    })
}

pub(in crate::cypher::parser) fn resolve_set_items(
    items: &[SetItem],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<SetItem>> {
    items
        .iter()
        .map(|item| match item {
            SetItem::Property(a) => Ok(SetItem::Property(Assignment {
                variable: a.variable.clone(),
                property: a.property.clone(),
                value: resolve_expr(&a.value, params)?,
            })),
            SetItem::Label { variable, labels } => Ok(SetItem::Label {
                variable: variable.clone(),
                labels: labels.clone(),
            }),
            SetItem::MapOverwrite { variable, value } => Ok(SetItem::MapOverwrite {
                variable: variable.clone(),
                value: resolve_expr(value, params)?,
            }),
            SetItem::MapMerge { variable, value } => Ok(SetItem::MapMerge {
                variable: variable.clone(),
                value: resolve_expr(value, params)?,
            }),
        })
        .collect()
}

pub(in crate::cypher::parser) fn resolve_return_items(
    items: &[ReturnItem],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<ReturnItem>> {
    items
        .iter()
        .map(|item| {
            Ok(ReturnItem {
                expr: resolve_expr(&item.expr, params)?,
                alias: item.alias.clone(),
            })
        })
        .collect()
}

pub(in crate::cypher::parser) fn resolve_sort_items(
    items: &[SortItem],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<SortItem>> {
    items
        .iter()
        .map(|item| {
            Ok(SortItem {
                expr: resolve_expr(&item.expr, params)?,
                descending: item.descending,
            })
        })
        .collect()
}

pub(in crate::cypher::parser) fn resolve_intermediate_clauses(
    clauses: &[IntermediateClause],
    params: &HashMap<String, Value>,
) -> crate::types::Result<Vec<IntermediateClause>> {
    clauses
        .iter()
        .map(|c| match c {
            IntermediateClause::With(w) => Ok(IntermediateClause::With(WithClause {
                items: resolve_return_items(&w.items, params)?,
                distinct: w.distinct,
                order_by: w.order_by.clone(),
                skip: w
                    .skip
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
                limit: w
                    .limit
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
                where_clause: w
                    .where_clause
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
            })),
            IntermediateClause::Unwind(u) => Ok(IntermediateClause::Unwind(UnwindClause {
                expr: resolve_expr(&u.expr, params)?,
                alias: u.alias.clone(),
            })),
            IntermediateClause::Match(m) => Ok(IntermediateClause::Match(IntermediateMatch {
                patterns: resolve_patterns(&m.patterns, params)?,
                optional_patterns: m
                    .optional_patterns
                    .iter()
                    .map(|om| resolve_optional_match(om, params))
                    .collect::<crate::types::Result<Vec<_>>>()?,
                where_clause: m
                    .where_clause
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
            })),
        })
        .collect()
}

type ResolvedReturn = (
    Option<ReturnClause>,
    Vec<SortItem>,
    Option<Expr>,
    Option<Expr>,
);

pub(in crate::cypher::parser) fn resolve_optional_return(
    return_clause: &Option<ReturnClause>,
    order_by: &[SortItem],
    skip: &Option<Expr>,
    limit: &Option<Expr>,
    params: &HashMap<String, Value>,
) -> crate::types::Result<ResolvedReturn> {
    let rc: Option<ReturnClause> = return_clause
        .as_ref()
        .map(|rc| -> crate::types::Result<ReturnClause> {
            Ok(ReturnClause {
                items: resolve_return_items(&rc.items, params)?,
                distinct: rc.distinct,
            })
        })
        .transpose()?;
    let ob = resolve_sort_items(order_by, params)?;
    let s = skip.as_ref().map(|e| resolve_expr(e, params)).transpose()?;
    let l = limit
        .as_ref()
        .map(|e| resolve_expr(e, params))
        .transpose()?;
    Ok((rc, ob, s, l))
}

/// Substitute all `$name` parameters in a parsed statement with literal values.
///
/// This must be called before planning so the planner can use literal values
/// for index selection decisions.
pub fn resolve_params(
    stmt: &Statement,
    params: &HashMap<String, Value>,
) -> crate::types::Result<Statement> {
    match stmt {
        Statement::Match(m) => Ok(Statement::Match(MatchStatement {
            patterns: resolve_patterns(&m.patterns, params)?,
            optional_patterns: m
                .optional_patterns
                .iter()
                .map(|om| resolve_optional_match(om, params))
                .collect::<crate::types::Result<Vec<_>>>()?,
            where_clause: m
                .where_clause
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            intermediate_clauses: resolve_intermediate_clauses(&m.intermediate_clauses, params)?,
            return_clause: ReturnClause {
                items: resolve_return_items(&m.return_clause.items, params)?,
                distinct: m.return_clause.distinct,
            },
            order_by: resolve_sort_items(&m.order_by, params)?,
            skip: m
                .skip
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            limit: m
                .limit
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
        })),
        Statement::Create(c) => {
            let (return_clause, order_by, skip, limit) =
                resolve_optional_return(&c.return_clause, &c.order_by, &c.skip, &c.limit, params)?;
            Ok(Statement::Create(CreateStatement {
                patterns: resolve_patterns(&c.patterns, params)?,
                return_clause,
                order_by,
                skip,
                limit,
            }))
        }
        Statement::MatchCreate(mc) => {
            let (return_clause, order_by, skip, limit) = resolve_optional_return(
                &mc.return_clause,
                &mc.order_by,
                &mc.skip,
                &mc.limit,
                params,
            )?;
            Ok(Statement::MatchCreate(MatchCreateStatement {
                patterns: resolve_patterns(&mc.patterns, params)?,
                where_clause: mc
                    .where_clause
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
                create_patterns: resolve_patterns(&mc.create_patterns, params)?,
                return_clause,
                order_by,
                skip,
                limit,
            }))
        }
        Statement::MatchMerge(mm) => {
            let (return_clause, order_by, skip, limit) = resolve_optional_return(
                &mm.return_clause,
                &mm.order_by,
                &mm.skip,
                &mm.limit,
                params,
            )?;
            Ok(Statement::MatchMerge(MatchMergeStatement {
                patterns: resolve_patterns(&mm.patterns, params)?,
                where_clause: mm
                    .where_clause
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
                merge_pattern: resolve_pattern(&mm.merge_pattern, params)?,
                on_create: resolve_set_items(&mm.on_create, params)?,
                on_match: resolve_set_items(&mm.on_match, params)?,
                return_clause,
                order_by,
                skip,
                limit,
            }))
        }
        Statement::Delete(d) => {
            let (return_clause, order_by, skip, limit) =
                resolve_optional_return(&d.return_clause, &d.order_by, &d.skip, &d.limit, params)?;
            Ok(Statement::Delete(DeleteStatement {
                patterns: resolve_patterns(&d.patterns, params)?,
                optional_patterns: d
                    .optional_patterns
                    .iter()
                    .map(|om| resolve_optional_match(om, params))
                    .collect::<crate::types::Result<Vec<_>>>()?,
                where_clause: d
                    .where_clause
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
                detach: d.detach,
                exprs: d
                    .exprs
                    .iter()
                    .map(|e| resolve_expr(e, params))
                    .collect::<crate::types::Result<Vec<_>>>()?,
                return_clause,
                order_by,
                skip,
                limit,
            }))
        }
        Statement::Set(s) => {
            let (return_clause, order_by, skip, limit) =
                resolve_optional_return(&s.return_clause, &s.order_by, &s.skip, &s.limit, params)?;
            Ok(Statement::Set(SetStatement {
                patterns: resolve_patterns(&s.patterns, params)?,
                optional_patterns: s
                    .optional_patterns
                    .iter()
                    .map(|om| resolve_optional_match(om, params))
                    .collect::<crate::types::Result<Vec<_>>>()?,
                where_clause: s
                    .where_clause
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
                items: resolve_set_items(&s.items, params)?,
                intermediate_clauses: resolve_intermediate_clauses(
                    &s.intermediate_clauses,
                    params,
                )?,
                return_clause,
                order_by,
                skip,
                limit,
            }))
        }
        Statement::Remove(r) => {
            let (return_clause, order_by, skip, limit) =
                resolve_optional_return(&r.return_clause, &r.order_by, &r.skip, &r.limit, params)?;
            Ok(Statement::Remove(RemoveStatement {
                patterns: resolve_patterns(&r.patterns, params)?,
                optional_patterns: r
                    .optional_patterns
                    .iter()
                    .map(|om| resolve_optional_match(om, params))
                    .collect::<crate::types::Result<Vec<_>>>()?,
                where_clause: r
                    .where_clause
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
                items: r.items.clone(),
                return_clause,
                order_by,
                skip,
                limit,
            }))
        }
        Statement::Merge(m) => {
            let (return_clause, order_by, skip, limit) =
                resolve_optional_return(&m.return_clause, &m.order_by, &m.skip, &m.limit, params)?;
            Ok(Statement::Merge(MergeStatement {
                pattern: resolve_pattern(&m.pattern, params)?,
                on_create: resolve_set_items(&m.on_create, params)?,
                on_match: resolve_set_items(&m.on_match, params)?,
                return_clause,
                order_by,
                skip,
                limit,
            }))
        }
        Statement::Unwind(u) => {
            let body = match &u.body {
                UnwindBody::Return {
                    where_clause,
                    intermediate_clauses,
                    return_clause,
                    order_by,
                    skip,
                    limit,
                } => UnwindBody::Return {
                    where_clause: where_clause
                        .as_ref()
                        .map(|e| resolve_expr(e, params))
                        .transpose()?,
                    intermediate_clauses: resolve_intermediate_clauses(
                        intermediate_clauses,
                        params,
                    )?,
                    return_clause: ReturnClause {
                        items: resolve_return_items(&return_clause.items, params)?,
                        distinct: return_clause.distinct,
                    },
                    order_by: resolve_sort_items(order_by, params)?,
                    skip: skip.as_ref().map(|e| resolve_expr(e, params)).transpose()?,
                    limit: limit
                        .as_ref()
                        .map(|e| resolve_expr(e, params))
                        .transpose()?,
                },
                UnwindBody::Create {
                    patterns,
                    intermediate_clauses,
                    return_clause,
                    order_by,
                    skip,
                    limit,
                } => {
                    let (rc, ob, s, l) =
                        resolve_optional_return(return_clause, order_by, skip, limit, params)?;
                    UnwindBody::Create {
                        patterns: resolve_patterns(patterns, params)?,
                        intermediate_clauses: resolve_intermediate_clauses(
                            intermediate_clauses,
                            params,
                        )?,
                        return_clause: rc,
                        order_by: ob,
                        skip: s,
                        limit: l,
                    }
                }
            };
            Ok(Statement::Unwind(UnwindStatement {
                expr: resolve_expr(&u.expr, params)?,
                alias: u.alias.clone(),
                body,
            }))
        }
        Statement::Return(r) => Ok(Statement::Return(ReturnStatement {
            return_clause: ReturnClause {
                items: resolve_return_items(&r.return_clause.items, params)?,
                distinct: r.return_clause.distinct,
            },
            order_by: resolve_sort_items(&r.order_by, params)?,
            skip: r
                .skip
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            limit: r
                .limit
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
        })),
        Statement::MultiClause(mc) => {
            let clauses = mc
                .clauses
                .iter()
                .map(|c| resolve_clause(c, params))
                .collect::<crate::types::Result<Vec<_>>>()?;
            Ok(Statement::MultiClause(MultiClauseStatement {
                clauses,
                return_clause: mc
                    .return_clause
                    .as_ref()
                    .map(|rc| {
                        Ok::<_, GraphError>(ReturnClause {
                            items: resolve_return_items(&rc.items, params)?,
                            distinct: rc.distinct,
                        })
                    })
                    .transpose()?,
                order_by: resolve_sort_items(&mc.order_by, params)?,
                skip: mc
                    .skip
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
                limit: mc
                    .limit
                    .as_ref()
                    .map(|e| resolve_expr(e, params))
                    .transpose()?,
            }))
        }
        Statement::Explain(inner) => {
            Ok(Statement::Explain(Box::new(resolve_params(inner, params)?)))
        }
        Statement::Union { statements, all } => {
            let resolved: crate::types::Result<Vec<Statement>> = statements
                .iter()
                .map(|s| resolve_params(s, params))
                .collect();
            Ok(Statement::Union {
                statements: resolved?,
                all: *all,
            })
        }
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
        } => {
            let resolved_args: crate::types::Result<Vec<_>> =
                args.iter().map(|a| resolve_expr(a, params)).collect();
            Ok(Statement::Call {
                procedure_name: procedure_name.clone(),
                args: resolved_args?,
                implicit_args: *implicit_args,
                yield_items: yield_items.clone(),
                yield_star: *yield_star,
                return_clause: return_clause
                    .as_ref()
                    .map(|rc| {
                        Ok::<_, GraphError>(ReturnClause {
                            items: resolve_return_items(&rc.items, params)?,
                            distinct: rc.distinct,
                        })
                    })
                    .transpose()?,
                order_by: resolve_sort_items(order_by, params)?,
                skip: skip
                    .as_ref()
                    .map(|e| resolve_expr(e, params).map(Box::new))
                    .transpose()?,
                limit: limit
                    .as_ref()
                    .map(|e| resolve_expr(e, params).map(Box::new))
                    .transpose()?,
            })
        }
    }
}

/// Resolve parameters in a multi-clause Clause.
pub(in crate::cypher::parser) fn resolve_clause(
    clause: &Clause,
    params: &HashMap<String, Value>,
) -> crate::types::Result<Clause> {
    match clause {
        Clause::Match {
            patterns,
            optional_patterns,
            where_clause,
        } => Ok(Clause::Match {
            patterns: resolve_patterns(patterns, params)?,
            optional_patterns: optional_patterns
                .iter()
                .map(|om| resolve_optional_match(om, params))
                .collect::<crate::types::Result<Vec<_>>>()?,
            where_clause: where_clause
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
        }),
        Clause::Create { patterns } => Ok(Clause::Create {
            patterns: resolve_patterns(patterns, params)?,
        }),
        Clause::Merge {
            pattern,
            on_create,
            on_match,
        } => Ok(Clause::Merge {
            pattern: resolve_pattern(pattern, params)?,
            on_create: resolve_set_items(on_create, params)?,
            on_match: resolve_set_items(on_match, params)?,
        }),
        Clause::With(with) => Ok(Clause::With(WithClause {
            items: resolve_return_items(&with.items, params)?,
            distinct: with.distinct,
            order_by: resolve_sort_items(&with.order_by, params)?,
            skip: with
                .skip
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            limit: with
                .limit
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
            where_clause: with
                .where_clause
                .as_ref()
                .map(|e| resolve_expr(e, params))
                .transpose()?,
        })),
        Clause::Unwind(uw) => Ok(Clause::Unwind(UnwindClause {
            expr: resolve_expr(&uw.expr, params)?,
            alias: uw.alias.clone(),
        })),
        Clause::Set { items } => Ok(Clause::Set {
            items: resolve_set_items(items, params)?,
        }),
        Clause::Remove { items } => Ok(Clause::Remove {
            items: items.clone(),
        }),
        Clause::Call {
            procedure_name,
            args,
            implicit_args,
            yield_items,
            yield_star,
        } => {
            let resolved_args: crate::types::Result<Vec<_>> =
                args.iter().map(|a| resolve_expr(a, params)).collect();
            Ok(Clause::Call {
                procedure_name: procedure_name.clone(),
                args: resolved_args?,
                implicit_args: *implicit_args,
                yield_items: yield_items.clone(),
                yield_star: *yield_star,
            })
        }
        Clause::Delete { exprs, detach } => Ok(Clause::Delete {
            exprs: exprs
                .iter()
                .map(|e| resolve_expr(e, params))
                .collect::<crate::types::Result<Vec<_>>>()?,
            detach: *detach,
        }),
    }
}
