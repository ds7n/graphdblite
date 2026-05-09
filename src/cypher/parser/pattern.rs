//! Pattern parsing — patterns, node and relationship patterns, var-length specs, property maps.

use crate::types::{ErrorCode, GraphError, QueryPhase};

use super::expr::*;
use super::*;

pub(in crate::cypher::parser) fn parse_pattern_list(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Vec<Pattern>> {
    pair.into_inner()
        .filter(|p| matches!(p.as_rule(), Rule::pattern_item))
        .map(parse_pattern_item)
        .collect()
}

pub(in crate::cypher::parser) fn parse_pattern_item(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Pattern> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::path_pattern => parse_path_pattern(inner),
        Rule::pattern => parse_pattern(inner),
        _ => Err(GraphError::syntax(format!(
            "unexpected pattern item: {:?}",
            inner.as_rule()
        ))),
    }
}

pub(in crate::cypher::parser) fn parse_path_pattern(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Pattern> {
    let mut path_variable = None;
    let mut pattern = None;
    let mut mode = ShortestPathMode::None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => path_variable = Some(strip_backticks(inner.as_str()).to_string()),
            Rule::shortest_path_fn => {
                mode = ShortestPathMode::Single;
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::pattern {
                        pattern = Some(parse_pattern(child)?);
                    }
                }
            }
            Rule::all_shortest_paths_fn => {
                mode = ShortestPathMode::All;
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::pattern {
                        pattern = Some(parse_pattern(child)?);
                    }
                }
            }
            Rule::pattern => {
                pattern = Some(parse_pattern(inner)?);
            }
            _ => {}
        }
    }

    let mut pat = pattern
        .ok_or_else(|| GraphError::syntax("missing pattern in path assignment".to_string()))?;
    pat.path_variable = path_variable;
    pat.shortest_path_mode = mode;
    Ok(pat)
}

pub(in crate::cypher::parser) fn parse_pattern(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Pattern> {
    parse_pattern_inner(pair)
}

pub(in crate::cypher::parser) fn parse_pattern_inner(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<Pattern> {
    let mut elements = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::node_pattern => elements.push(PatternElement::Node(parse_node_pattern(inner)?)),
            Rule::rel_pattern => {
                elements.push(PatternElement::Relationship(parse_rel_pattern(inner)?))
            }
            _ => {}
        }
    }
    Ok(Pattern {
        elements,
        path_variable: None,
        shortest_path_mode: ShortestPathMode::None,
    })
}

pub(in crate::cypher::parser) fn parse_node_pattern(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<NodePattern> {
    let mut variable = None;
    let mut labels = Vec::new();
    let mut properties = HashMap::new();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => variable = Some(strip_backticks(inner.as_str()).to_string()),
            Rule::label_spec => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::symbolic_name {
                        labels.push(child.as_str().to_string());
                    }
                }
            }
            Rule::property_map => properties = parse_property_map(inner)?,
            _ => {}
        }
    }

    Ok(NodePattern {
        variable,
        labels,
        properties,
    })
}

pub(in crate::cypher::parser) fn parse_rel_pattern(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<RelPattern> {
    let inner = pair.into_inner().next().unwrap();
    let direction = match inner.as_rule() {
        Rule::rel_right | Rule::rel_right_bare => RelDirection::Outgoing,
        Rule::rel_left | Rule::rel_left_bare => RelDirection::Incoming,
        Rule::rel_undirected | Rule::rel_undirected_bare | Rule::rel_both | Rule::rel_both_bare => {
            RelDirection::Undirected
        }
        _ => unreachable!(),
    };

    let mut variable = None;
    let mut rel_types = Vec::new();
    let mut properties = HashMap::new();
    let mut var_length = None;

    for child in inner.into_inner() {
        if child.as_rule() == Rule::rel_detail {
            for detail in child.into_inner() {
                match detail.as_rule() {
                    Rule::ident => variable = Some(strip_backticks(detail.as_str()).to_string()),
                    Rule::rel_type_spec => {
                        for rt in detail.into_inner() {
                            if rt.as_rule() == Rule::symbolic_name {
                                let t = rt.as_str().to_string();
                                if !rel_types.contains(&t) {
                                    rel_types.push(t);
                                }
                            }
                        }
                    }
                    Rule::property_map => {
                        properties = parse_property_map(detail)?;
                    }
                    Rule::var_length => {
                        let (range, var_props) = parse_var_length(detail)?;
                        var_length = Some(range);
                        // Merge property filters from inside var_length
                        // (e.g. [:TYPE* {key: val}]).
                        if !var_props.is_empty() {
                            properties.extend(var_props);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(RelPattern {
        variable,
        rel_types,
        properties,
        direction,
        var_length,
    })
}

/// Parse a `u32` hop bound from a var-length pattern (e.g. the `4` in `*1..4`).
///
/// Rejects values that overflow `u32` (security finding H3) so a malformed
/// pattern can't panic the parser. The hop-count *cap* (security finding H1)
/// is enforced at plan-validation time against `Config::max_traversal_depth`,
/// not here — that lets the limit be configurable rather than baked into the
/// grammar. Var-length traversal (`src/edge.rs::traverse_paths`) clones the
/// path and visited-edge set per branch, so cost grows roughly as
/// `O(branching_factor ^ max_hops)`; an unbounded depth lets `*1..1000000000`
/// OOM the host, hence the per-database cap.
pub(in crate::cypher::parser) fn parse_var_length_bound(s: &str) -> crate::types::Result<u32> {
    s.parse::<u32>().map_err(|_| {
        GraphError::query(
            QueryPhase::Parse,
            ErrorCode::NumberOutOfRange,
            format!("var-length hop count `{s}` is out of range (must fit in u32)"),
        )
    })
}

pub(in crate::cypher::parser) fn parse_var_length(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<((u32, u32), HashMap<String, Expr>)> {
    let mut range = None;
    let mut props = HashMap::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::int_range => {
                // int_range = { integer? ~ ".." ~ integer? }
                // Use raw text to distinguish "1.." from "..3" when only one integer present.
                let raw = inner.as_str().trim();
                let nums: Vec<u32> = inner
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::integer)
                    .map(|p| parse_var_length_bound(p.as_str()))
                    .collect::<crate::types::Result<_>>()?;
                range = Some(match nums.len() {
                    2 => (nums[0], nums[1]),
                    1 if raw.starts_with("..") => (1, nums[0]),
                    1 => (nums[0], 50),
                    _ => (1, 50),
                });
            }
            Rule::fixed_length => {
                let n = parse_var_length_bound(inner.as_str())?;
                range = Some((n, n));
            }
            Rule::property_map => {
                props = parse_property_map(inner)?;
            }
            _ => {}
        }
    }
    // Bare * with no range — default to 1..50 to prevent unbounded traversal.
    Ok((range.unwrap_or((1, 50)), props))
}

pub(in crate::cypher::parser) fn parse_property_map(
    pair: pest::iterators::Pair<Rule>,
) -> crate::types::Result<HashMap<String, Expr>> {
    let mut map = HashMap::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::property_pair {
            let mut children = inner.into_inner();
            let key = strip_backticks(children.next().unwrap().as_str()).to_string();
            let value = parse_expr(children.next().unwrap())?;
            map.insert(key, value);
        }
    }
    Ok(map)
}

// === Clause parsing ===
