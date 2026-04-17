//! Parse TCK expected-value cells (written in Cypher literal syntax) and
//! compare them against actual `Value`s returned by graphdblite.
//!
//! This parser is intentionally independent of `src/cypher/parser.rs` so the
//! harness can't be fooled by bugs in the production parser — any agreement
//! between the two is real conformance, not accidental sameness.
//!
//! Supported expected-cell forms:
//!
//! - `null`
//! - booleans: `true`, `false`
//! - integers: `42`, `-7`
//! - floats: `3.14`, `-0.5`
//! - strings: `'hello'` (single quotes, openCypher convention)
//! - lists: `[1, 2, 3]`, `[]`
//! - maps: `{a: 1, b: 'x'}`, `{}`
//! - node patterns: `(:Label {name: 'Alice'})`, `(:Label)`
//! - edge patterns: `[:TYPE {weight: 2}]`, `[:TYPE]`
//!
//! Path patterns (`<(:A)-[:R]->(:B)>`) are not yet supported — Phase 4 scope.

use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, bail, Result};
use graphdblite::{Record, Value};

// ------------------------------ public API --------------------------------

/// Parse a single expected cell (a Gherkin table value) into a graphdblite
/// `Value` for comparison.
pub fn parse_expected(cell: &str) -> Result<Value> {
    let mut p = Parser::new(cell.trim());
    let v = p.parse_value()?;
    p.skip_ws();
    if !p.at_end() {
        bail!("trailing characters in expected cell: {cell:?}");
    }
    Ok(v)
}

/// Compare a batch of actual result records against expected rows. Each
/// expected row is a `Vec<Value>` in column order matching `columns`.
///
/// When `ordered` is true, comparison is positional; otherwise actual and
/// expected are treated as multisets.
pub fn compare_result(
    actual: &[Record],
    columns: &[String],
    expected_rows: &[Vec<Value>],
    ordered: bool,
) -> Result<()> {
    let actual_rows = project_rows(actual, columns);

    if actual_rows.len() != expected_rows.len() {
        bail!(
            "row count mismatch: expected {}, got {}\nexpected={:?}\nactual={:?}",
            expected_rows.len(),
            actual_rows.len(),
            expected_rows,
            actual_rows
        );
    }

    if ordered {
        for (i, (got, want)) in actual_rows.iter().zip(expected_rows.iter()).enumerate() {
            if !rows_equal(got, want) {
                bail!(
                    "row {i} mismatch\n expected: {:?}\n   actual: {:?}",
                    want,
                    got
                );
            }
        }
    } else {
        let mut used = vec![false; actual_rows.len()];
        for want in expected_rows {
            let mut found = false;
            for (i, got) in actual_rows.iter().enumerate() {
                if !used[i] && rows_equal(got, want) {
                    used[i] = true;
                    found = true;
                    break;
                }
            }
            if !found {
                bail!(
                    "expected row not found in actual:\n expected: {:?}\n   actual: {:?}",
                    want,
                    actual_rows
                );
            }
        }
    }
    Ok(())
}

// --------------------------- row equality ---------------------------------

fn project_rows(records: &[Record], columns: &[String]) -> Vec<Vec<Value>> {
    records
        .iter()
        .map(|rec| {
            columns
                .iter()
                .map(|col| rec.get(col).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect()
}

fn rows_equal(got: &[Value], want: &[Value]) -> bool {
    if got.len() != want.len() {
        return false;
    }
    got.iter().zip(want.iter()).all(|(g, w)| value_equal(g, w))
}

/// Equality that ignores node IDs — TCK node literals are identified
/// structurally (label + properties), since the scenario author has no way
/// to know which internal ID the implementation will assign.
fn value_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Node(na), Value::Node(nb)) => {
            na.labels == nb.labels && properties_equal(&na.properties, &nb.properties)
        }
        (Value::Edge(ea), Value::Edge(eb)) => {
            ea.label == eb.label && properties_equal(&ea.properties, &eb.properties)
        }
        (Value::Path(pa), Value::Path(pb)) => {
            pa.nodes.len() == pb.nodes.len()
                && pa.edges.len() == pb.edges.len()
                && pa
                    .nodes
                    .iter()
                    .zip(pb.nodes.iter())
                    .all(|(a, b)| value_equal(&Value::Node(a.clone()), &Value::Node(b.clone())))
                && pa
                    .edges
                    .iter()
                    .zip(pb.edges.iter())
                    .all(|(a, b)| value_equal(&Value::Edge(a.clone()), &Value::Edge(b.clone())))
        }
        (Value::List(la), Value::List(lb)) => {
            la.len() == lb.len() && la.iter().zip(lb.iter()).all(|(x, y)| value_equal(x, y))
        }
        (Value::Map(ma), Value::Map(mb)) => {
            ma.len() == mb.len()
                && ma
                    .iter()
                    .all(|(k, v)| mb.get(k).is_some_and(|w| value_equal(v, w)))
        }
        _ => a == b,
    }
}

fn properties_equal(a: &HashMap<String, Value>, b: &HashMap<String, Value>) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(k, v)| b.get(k).is_some_and(|w| value_equal(v, w)))
}

// ----------------------------- parser -------------------------------------

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance(c.len_utf8());
            } else {
                break;
            }
        }
    }

    fn consume(&mut self, tag: &str) -> bool {
        if self.rest().starts_with(tag) {
            self.advance(tag.len());
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tag: &str) -> Result<()> {
        if self.consume(tag) {
            Ok(())
        } else {
            bail!(
                "expected {tag:?} at position {} (rest={:?})",
                self.pos,
                self.rest()
            )
        }
    }

    fn parse_value(&mut self) -> Result<Value> {
        self.skip_ws();
        match self.peek() {
            None => Err(anyhow!("unexpected end of expected cell")),
            Some('n') if self.rest().starts_with("null") => {
                self.advance(4);
                Ok(Value::Null)
            }
            Some('t') if self.rest().starts_with("true") => {
                self.advance(4);
                Ok(Value::Bool(true))
            }
            Some('f') if self.rest().starts_with("false") => {
                self.advance(5);
                Ok(Value::Bool(false))
            }
            Some('\'') => self.parse_string(),
            Some('[') => {
                // Either a list [ ... ] or an edge pattern [:TYPE ...].
                let after_bracket = self.rest()[1..].trim_start();
                if after_bracket.starts_with(':') {
                    self.parse_edge()
                } else {
                    self.parse_list()
                }
            }
            Some('{') => self.parse_map(),
            Some('(') => self.parse_node(),
            Some('<') => self.parse_path(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(anyhow!(
                "unexpected character {c:?} at position {}",
                self.pos
            )),
        }
    }

    fn parse_string(&mut self) -> Result<Value> {
        self.expect("'")?;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c == '\'' {
                self.advance(1);
                return Ok(Value::String(out));
            }
            if c == '\\' {
                self.advance(1);
                match self.peek() {
                    Some('n') => {
                        out.push('\n');
                        self.advance(1);
                    }
                    Some('t') => {
                        out.push('\t');
                        self.advance(1);
                    }
                    Some('\'') => {
                        out.push('\'');
                        self.advance(1);
                    }
                    Some('\\') => {
                        out.push('\\');
                        self.advance(1);
                    }
                    Some(other) => {
                        out.push(other);
                        self.advance(other.len_utf8());
                    }
                    None => bail!("unterminated escape in string literal"),
                }
                continue;
            }
            out.push(c);
            self.advance(c.len_utf8());
        }
        bail!("unterminated string literal")
    }

    fn parse_number(&mut self) -> Result<Value> {
        let start = self.pos;
        if self.consume("-") {
            // allowed leading sign
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance(1);
            } else {
                break;
            }
        }
        let mut is_float = false;
        if self.peek() == Some('.') {
            is_float = true;
            self.advance(1);
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance(1);
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            is_float = true;
            self.advance(1);
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.advance(1);
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance(1);
                } else {
                    break;
                }
            }
        }
        let text = &self.src[start..self.pos];
        if is_float {
            Ok(Value::F64(
                text.parse().map_err(|e| anyhow!("float parse: {e}"))?,
            ))
        } else {
            Ok(Value::I64(
                text.parse().map_err(|e| anyhow!("int parse: {e}"))?,
            ))
        }
    }

    fn parse_list(&mut self) -> Result<Value> {
        self.expect("[")?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.consume("]") {
            return Ok(Value::List(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            if self.consume("]") {
                return Ok(Value::List(items));
            }
            self.expect(",")?;
        }
    }

    fn parse_map(&mut self) -> Result<Value> {
        self.expect("{")?;
        let mut entries: BTreeMap<String, Value> = BTreeMap::new();
        self.skip_ws();
        if self.consume("}") {
            return Ok(Value::Map(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_ident()?;
            self.skip_ws();
            self.expect(":")?;
            let val = self.parse_value()?;
            entries.insert(key, val);
            self.skip_ws();
            if self.consume("}") {
                return Ok(Value::Map(entries));
            }
            self.expect(",")?;
        }
    }

    fn parse_ident(&mut self) -> Result<String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                self.advance(c.len_utf8());
            } else {
                break;
            }
        }
        if start == self.pos {
            bail!("expected identifier at position {}", self.pos);
        }
        Ok(self.src[start..self.pos].to_string())
    }

    /// Parse a node pattern `(:Label {k: v})` or `(:Label)` or `(:A:B)`.
    fn parse_node(&mut self) -> Result<Value> {
        use graphdblite::{Node, NodeId};

        self.expect("(")?;
        self.skip_ws();
        // Optional leading variable name before `:`. We ignore it for equality.
        while let Some(c) = self.peek() {
            if c == ':' || c == ')' || c == '{' {
                break;
            }
            self.advance(c.len_utf8());
        }

        let mut labels = Vec::new();
        while self.consume(":") {
            labels.push(self.parse_ident()?);
            self.skip_ws();
        }
        labels.sort();

        let mut properties = HashMap::new();
        if self.peek() == Some('{') {
            if let Value::Map(m) = self.parse_map()? {
                for (k, v) in m {
                    properties.insert(k, v);
                }
            }
            self.skip_ws();
        }
        self.expect(")")?;

        Ok(Value::Node(Node {
            id: NodeId(0), // ID is ignored by structural equality.
            labels,
            properties,
        }))
    }

    /// Parse an edge pattern `[:TYPE {k: v}]` or `[:TYPE]`.
    fn parse_edge(&mut self) -> Result<Value> {
        use graphdblite::{Edge, NodeId};

        self.expect("[")?;
        self.skip_ws();
        while let Some(c) = self.peek() {
            if c == ':' || c == ']' || c == '{' {
                break;
            }
            self.advance(c.len_utf8());
        }

        let mut label = String::new();
        if self.consume(":") {
            label = self.parse_ident()?;
            self.skip_ws();
        }

        let mut properties = HashMap::new();
        if self.peek() == Some('{') {
            if let Value::Map(m) = self.parse_map()? {
                for (k, v) in m {
                    properties.insert(k, v);
                }
            }
            self.skip_ws();
        }
        self.expect("]")?;

        Ok(Value::Edge(Edge {
            src: NodeId(0), // endpoints are ignored by structural equality
            dst: NodeId(0),
            label,
            properties,
        }))
    }

    /// Parse a path literal `<(n1)-[:TYPE]->(n2)>`.
    fn parse_path(&mut self) -> Result<Value> {
        use graphdblite::PathValue;

        self.expect("<")?;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();

        self.skip_ws();
        // Parse first node.
        if self.peek() == Some('(') {
            if let Value::Node(n) = self.parse_node()? {
                nodes.push(n);
            }
        }

        // Parse subsequent -[:TYPE]->(node) segments.
        loop {
            self.skip_ws();
            if self.peek() == Some('>') && !self.rest().starts_with(">-") {
                // End of path.
                break;
            }
            if self.at_end() {
                break;
            }
            // Expect a relationship pattern like -[:TYPE]-> or <-[:TYPE]-
            if self.consume("-") {
                // Forward: -[:TYPE]-> or -[]->(
                if let Value::Edge(e) = self.parse_edge()? {
                    edges.push(e);
                }
                self.expect("->")?;
            } else if self.consume("<-") {
                if let Value::Edge(e) = self.parse_edge()? {
                    edges.push(e);
                }
                self.expect("-")?;
            } else {
                break;
            }
            self.skip_ws();
            if self.peek() == Some('(') {
                if let Value::Node(n) = self.parse_node()? {
                    nodes.push(n);
                }
            }
        }

        self.expect(">")?;
        Ok(Value::Path(PathValue { nodes, edges }))
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::parse_expected;

    #[test]
    fn scalars() {
        assert_eq!(parse_expected("null").unwrap(), Value::Null);
        assert_eq!(parse_expected("true").unwrap(), Value::Bool(true));
        assert_eq!(parse_expected("42").unwrap(), Value::I64(42));
        assert_eq!(parse_expected("-7").unwrap(), Value::I64(-7));
        assert_eq!(parse_expected("3.14").unwrap(), Value::F64(3.14));
        assert_eq!(
            parse_expected("'hello'").unwrap(),
            Value::String("hello".into())
        );
    }

    #[test]
    fn lists_and_maps() {
        assert_eq!(
            parse_expected("[1, 2, 3]").unwrap(),
            Value::List(vec![Value::I64(1), Value::I64(2), Value::I64(3)])
        );
        assert_eq!(parse_expected("[]").unwrap(), Value::List(vec![]));
        let m = parse_expected("{a: 1, b: 'x'}").unwrap();
        if let Value::Map(map) = m {
            assert_eq!(map.len(), 2);
            assert_eq!(map.get("a"), Some(&Value::I64(1)));
        } else {
            panic!("expected map");
        }
    }

    #[test]
    fn node_pattern() {
        let n = parse_expected("(:Person {name: 'Alice', age: 30})").unwrap();
        if let Value::Node(node) = n {
            assert_eq!(node.labels, vec!["Person".to_string()]);
            assert_eq!(
                node.properties.get("name"),
                Some(&Value::String("Alice".into()))
            );
        } else {
            panic!("expected node");
        }
    }

    #[test]
    fn multi_label_node() {
        let n = parse_expected("(:A:B)").unwrap();
        if let Value::Node(node) = n {
            assert_eq!(node.labels, vec!["A".to_string(), "B".to_string()]);
        } else {
            panic!("expected node");
        }
    }

    #[test]
    fn path_literal() {
        let p = parse_expected("<(:A)-[:R]->(:B)>").unwrap();
        if let Value::Path(path) = p {
            assert_eq!(path.nodes.len(), 2);
            assert_eq!(path.edges.len(), 1);
            assert_eq!(path.nodes[0].labels, vec!["A".to_string()]);
            assert_eq!(path.nodes[1].labels, vec!["B".to_string()]);
            assert_eq!(path.edges[0].label, "R");
        } else {
            panic!("expected path");
        }
    }
}
