//! Expression → RETURN-projection column name + precedence helpers.

use crate::cypher::ast::*;

use super::*;

pub(in crate::cypher::eval) fn format_child_expr(
    expr: &Expr,
    parent_prec: u8,
    is_left: bool,
) -> String {
    let needs_parens = if let ExprKind::BinaryOp { op, .. } = &expr.kind {
        let child_prec = binop_precedence(op);
        // Parenthesize if child has lower precedence, or same precedence
        // on the right side (to preserve left-to-right grouping).
        child_prec < parent_prec || (child_prec == parent_prec && !is_left)
    } else {
        false
    };
    let s = expr_to_column_name(expr);
    if needs_parens {
        format!("({s})")
    } else {
        s
    }
}

/// Resolve an expression to a column name for RETURN projections.
pub fn expr_to_column_name(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::Property(var, prop) => format!("{var}.{prop}"),
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
            original_text,
        } => {
            if let Some(text) = original_text {
                return text.clone();
            }
            let dist_prefix = if *distinct { "DISTINCT " } else { "" };
            if args.is_empty() || matches!(args[0].kind, ExprKind::Star) {
                format!("{name}(*)")
            } else {
                let arg_names: Vec<String> = args.iter().map(expr_to_column_name).collect();
                format!("{name}({dist_prefix}{})", arg_names.join(", "))
            }
        }
        ExprKind::Star => "*".to_string(),
        ExprKind::Literal(lit) => match lit {
            LiteralValue::Null => "null".to_string(),
            LiteralValue::Bool(b) => b.to_string(),
            LiteralValue::I64(n) => n.to_string(),
            LiteralValue::F64(n) => n.to_string(),
            LiteralValue::String(s) => format!("'{s}'"),
        },
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            let mut out = String::from("CASE");
            if let Some(o) = operand {
                out.push_str(&format!(" {}", expr_to_column_name(o)));
            }
            for (when, then) in alternatives {
                out.push_str(&format!(
                    " WHEN {} THEN {}",
                    expr_to_column_name(when),
                    expr_to_column_name(then)
                ));
            }
            if let Some(d) = default {
                out.push_str(&format!(" ELSE {}", expr_to_column_name(d)));
            }
            out.push_str(" END");
            out
        }
        ExprKind::List(items) => {
            let inner: Vec<String> = items.iter().map(expr_to_column_name).collect();
            format!("[{}]", inner.join(", "))
        }
        ExprKind::Index { expr, index } => {
            format!(
                "{}[{}]",
                expr_to_column_name(expr),
                expr_to_column_name(index)
            )
        }
        ExprKind::DotAccess { expr, key } => {
            let base = expr_to_column_name(expr);
            let needs_parens = !matches!(
                &expr.as_ref().kind,
                ExprKind::Variable(_) | ExprKind::DotAccess { .. } | ExprKind::Property(..)
            );
            if needs_parens {
                format!("({base}).{key}")
            } else {
                format!("{base}.{key}")
            }
        }
        ExprKind::Slice { expr, start, end } => {
            let s = start
                .as_ref()
                .map(|e| expr_to_column_name(e))
                .unwrap_or_default();
            let e = end
                .as_ref()
                .map(|e| expr_to_column_name(e))
                .unwrap_or_default();
            format!("{}[{}..{}]", expr_to_column_name(expr), s, e)
        }
        ExprKind::BinaryOp { left, op, right } => {
            let op_str = match op {
                BinOp::Add => " + ",
                BinOp::Sub => " - ",
                BinOp::Mul => " * ",
                BinOp::Div => " / ",
                BinOp::Mod => " % ",
                BinOp::Pow => " ^ ",
                BinOp::Eq => " = ",
                BinOp::Neq => " <> ",
                BinOp::Lt => " < ",
                BinOp::Gt => " > ",
                BinOp::Lte => " <= ",
                BinOp::Gte => " >= ",
                BinOp::And => " AND ",
                BinOp::Or => " OR ",
                BinOp::Xor => " XOR ",
                BinOp::In => " IN ",
                BinOp::StartsWith => " STARTS WITH ",
                BinOp::EndsWith => " ENDS WITH ",
                BinOp::Contains => " CONTAINS ",
                BinOp::RegexMatch => " =~ ",
            };
            let prec = binop_precedence(op);
            let l = format_child_expr(left, prec, true);
            let r = format_child_expr(right, prec, false);
            format!("{l}{op_str}{r}")
        }
        ExprKind::IsNull(inner) => format!("{} IS NULL", expr_to_column_name(inner)),
        ExprKind::IsNotNull(inner) => format!("{} IS NOT NULL", expr_to_column_name(inner)),
        ExprKind::Not(inner) => format!("NOT {}", expr_to_column_name(inner)),
        ExprKind::PatternComprehension {
            path_variable,
            pattern,
            where_clause,
            map_expr,
        } => {
            let mut out = String::from("[");
            if let Some(p) = path_variable {
                out.push_str(&format!("{p} = "));
            }
            out.push_str(&pattern_to_string(pattern));
            if let Some(w) = where_clause {
                out.push_str(&format!(" WHERE {}", expr_to_column_name(w)));
            }
            out.push_str(&format!(" | {}]", expr_to_column_name(map_expr)));
            out
        }
        ExprKind::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => {
            let mut out = format!("[{variable} IN {}", expr_to_column_name(list_expr));
            if let Some(f) = filter {
                out.push_str(&format!(" WHERE {}", expr_to_column_name(f)));
            }
            if let Some(m) = map_expr {
                out.push_str(&format!(" | {}", expr_to_column_name(m)));
            }
            out.push(']');
            out
        }
        ExprKind::Quantifier {
            kind,
            variable,
            list_expr,
            predicate,
        } => {
            let name = match kind {
                QuantifierKind::None => "none",
                QuantifierKind::Single => "single",
                QuantifierKind::Any => "any",
                QuantifierKind::All => "all",
            };
            format!(
                "{name}({variable} IN {} WHERE {})",
                expr_to_column_name(list_expr),
                expr_to_column_name(predicate)
            )
        }
        ExprKind::PatternPredicate(pattern) => pattern_to_string(pattern),
        ExprKind::Exists {
            patterns,
            where_clause,
        } => {
            let pats: Vec<String> = patterns.iter().map(pattern_to_string).collect();
            let mut out = format!("EXISTS {{ {}", pats.join(", "));
            if let Some(w) = where_clause {
                out.push_str(&format!(" WHERE {}", expr_to_column_name(w)));
            }
            out.push_str(" }");
            out
        }
        ExprKind::ExistsSubquery(_) => "EXISTS { ... }".to_string(),
        ExprKind::Parameter(name) => format!("${name}"),
        ExprKind::HasLabel(var, labels) => {
            let label_str: Vec<String> = labels.iter().map(|l| format!(":{l}")).collect();
            format!("({var}{})", label_str.join(""))
        }
        ExprKind::MapLiteral(pairs) => {
            let inner: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{k}: {}", expr_to_column_name(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// Render a pattern as Cypher text, for column names of expressions that
/// contain one (`[(a)-[:R]->(b) | b]`, `(a)-->(b)`).
fn pattern_to_string(pattern: &Pattern) -> String {
    let mut out = String::new();
    if let Some(p) = &pattern.path_variable {
        out.push_str(&format!("{p} = "));
    }
    for element in &pattern.elements {
        match element {
            PatternElement::Node(n) => {
                out.push('(');
                out.push_str(n.variable.as_deref().unwrap_or(""));
                for label in &n.labels {
                    out.push_str(&format!(":{label}"));
                }
                out.push_str(&property_map_to_string(&n.properties));
                out.push(')');
            }
            PatternElement::Relationship(r) => {
                let (left, right) = match r.direction {
                    RelDirection::Outgoing => ("-", "->"),
                    RelDirection::Incoming => ("<-", "-"),
                    RelDirection::Undirected => ("-", "-"),
                };
                let mut detail = r.variable.clone().unwrap_or_default();
                if !r.rel_types.is_empty() {
                    detail.push_str(&format!(":{}", r.rel_types.join("|")));
                }
                match r.var_length {
                    Some((lo, hi)) if lo == hi => detail.push_str(&format!("*{lo}")),
                    Some((lo, hi)) => detail.push_str(&format!("*{lo}..{hi}")),
                    None => {}
                }
                detail.push_str(&property_map_to_string(&r.properties));
                if detail.is_empty() {
                    out.push_str(&format!("{left}{right}"));
                } else {
                    out.push_str(&format!("{left}[{detail}]{right}"));
                }
            }
        }
    }
    out
}

/// Render a pattern property map as ` {k: v, ...}` (empty string when empty).
/// Keys are sorted: the AST keeps them in a hash map, so source order is lost.
fn property_map_to_string(props: &std::collections::HashMap<String, Expr>) -> String {
    if props.is_empty() {
        return String::new();
    }
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    let inner: Vec<String> = keys
        .into_iter()
        .map(|k| format!("{k}: {}", expr_to_column_name(&props[k])))
        .collect();
    format!(" {{{}}}", inner.join(", "))
}
