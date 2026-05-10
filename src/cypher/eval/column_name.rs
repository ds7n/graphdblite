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
        ExprKind::Case { .. } => "CASE".to_string(),
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
            };
            let prec = binop_precedence(op);
            let l = format_child_expr(left, prec, true);
            let r = format_child_expr(right, prec, false);
            format!("{l}{op_str}{r}")
        }
        ExprKind::IsNull(inner) => format!("{} IS NULL", expr_to_column_name(inner)),
        ExprKind::IsNotNull(inner) => format!("{} IS NOT NULL", expr_to_column_name(inner)),
        ExprKind::Not(inner) => format!("NOT {}", expr_to_column_name(inner)),
        ExprKind::PatternComprehension { .. } => "_expr".to_string(),
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
        _ => "_expr".to_string(),
    }
}
