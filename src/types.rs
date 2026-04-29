use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Unique identifier for a node in the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.0)
    }
}

impl NodeId {
    /// Encode as 8-byte big-endian for B-tree key ordering.
    pub fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    /// Decode from 8-byte big-endian.
    pub fn from_be_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_be_bytes(bytes))
    }
}

/// Dynamic property value stored on nodes and edges.
///
/// The `Node`, `Edge`, and `Path` variants are runtime-only — they appear in
/// query result records but must never be written into stored properties
/// (openCypher forbids it and storage paths rely on the property-value subset
/// being scalar-or-collection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
    List(Vec<Value>),
    /// A full node value (id + label + properties). Runtime-only.
    Node(Node),
    /// A full edge value (src + dst + label + properties). Runtime-only.
    Edge(Edge),
    /// A path with full node and edge contents. Runtime-only.
    Path(PathValue),
    /// Ordered string-keyed map (BTreeMap gives deterministic iteration and
    /// hashing regardless of insertion order).
    Map(BTreeMap<String, Value>),
    /// Temporal types.
    Date(crate::temporal::CypherDate),
    LocalTime(crate::temporal::CypherLocalTime),
    Time(crate::temporal::CypherTime),
    LocalDateTime(crate::temporal::CypherLocalDateTime),
    DateTime(crate::temporal::CypherDateTime),
    Duration(crate::temporal::CypherDuration),
}

/// NaN-safe equality: two NaN values are considered equal (bit-equal comparison).
/// This satisfies the `Eq` reflexivity contract that `derive(PartialEq)` would violate.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::I64(a), Value::I64(b)) => a == b,
            (Value::F64(a), Value::F64(b)) => a.to_bits() == b.to_bits(),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Node(a), Value::Node(b)) => a == b,
            (Value::Edge(a), Value::Edge(b)) => a == b,
            (Value::Path(a), Value::Path(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Date(a), Value::Date(b)) => a == b,
            (Value::LocalTime(a), Value::LocalTime(b)) => a == b,
            (Value::Time(a), Value::Time(b)) => a == b,
            (Value::LocalDateTime(a), Value::LocalDateTime(b)) => a == b,
            (Value::DateTime(a), Value::DateTime(b)) => a == b,
            (Value::Duration(a), Value::Duration(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

/// Hash a `Properties` (HashMap) deterministically by sorting keys.
fn hash_properties<H: Hasher>(props: &Properties, state: &mut H) {
    let mut entries: Vec<(&String, &Value)> = props.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries.len().hash(state);
    for (k, v) in entries {
        k.hash(state);
        v.hash(state);
    }
}

fn hash_node<H: Hasher>(n: &Node, state: &mut H) {
    n.id.hash(state);
    n.labels.hash(state);
    hash_properties(&n.properties, state);
}

fn hash_edge<H: Hasher>(e: &Edge, state: &mut H) {
    e.src.hash(state);
    e.dst.hash(state);
    e.label.hash(state);
    hash_properties(&e.properties, state);
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Null => {}
            Value::Bool(b) => b.hash(state),
            Value::I64(n) => n.hash(state),
            Value::F64(f) => f.to_bits().hash(state),
            Value::String(s) => s.hash(state),
            Value::List(items) => items.hash(state),
            Value::Node(n) => hash_node(n, state),
            Value::Edge(e) => hash_edge(e, state),
            Value::Path(p) => {
                p.nodes.len().hash(state);
                for n in &p.nodes {
                    hash_node(n, state);
                }
                p.edges.len().hash(state);
                for e in &p.edges {
                    hash_edge(e, state);
                }
            }
            Value::Map(map) => {
                // BTreeMap already iterates in key order — stable hash.
                for (k, v) in map {
                    k.hash(state);
                    v.hash(state);
                }
            }
            Value::Date(d) => d.hash(state),
            Value::LocalTime(t) => t.hash(state),
            Value::Time(t) => t.hash(state),
            Value::LocalDateTime(dt) => dt.hash(state),
            Value::DateTime(dt) => dt.hash(state),
            Value::Duration(d) => d.hash(state),
        }
    }
}

/// Format a `Properties` map deterministically (keys sorted) into a `Display` sink.
fn fmt_properties(props: &Properties, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut entries: Vec<(&String, &Value)> = props.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (i, (k, v)) in entries.into_iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{k}: {v}")?;
    }
    Ok(())
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::I64(n) => write!(f, "{n}"),
            Value::F64(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "\"{s}\""),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Node(n) => {
                write!(f, "(")?;
                for lbl in &n.labels {
                    write!(f, ":{lbl}")?;
                }
                if !n.properties.is_empty() {
                    write!(f, " {{")?;
                    fmt_properties(&n.properties, f)?;
                    write!(f, "}}")?;
                }
                write!(f, ")")
            }
            Value::Edge(e) => {
                write!(f, "[:{} {{", e.label)?;
                fmt_properties(&e.properties, f)?;
                write!(f, "}}]")
            }
            Value::Path(p) => {
                write!(f, "<")?;
                if let Some(first) = p.nodes.first() {
                    write!(f, "(")?;
                    for lbl in &first.labels {
                        write!(f, ":{lbl}")?;
                    }
                    if !first.properties.is_empty() {
                        if first.labels.is_empty() {
                            write!(f, "{{")?;
                        } else {
                            write!(f, " {{")?;
                        }
                        fmt_properties(&first.properties, f)?;
                        write!(f, "}}")?;
                    }
                    write!(f, ")")?;
                }
                for (edge, node) in p.edges.iter().zip(p.nodes.iter().skip(1)) {
                    write!(f, "-[:{}", edge.label)?;
                    if !edge.properties.is_empty() {
                        write!(f, " {{")?;
                        fmt_properties(&edge.properties, f)?;
                        write!(f, "}}")?;
                    }
                    write!(f, "]->(")?;
                    for lbl in &node.labels {
                        write!(f, ":{lbl}")?;
                    }
                    if !node.properties.is_empty() {
                        if node.labels.is_empty() {
                            write!(f, "{{")?;
                        } else {
                            write!(f, " {{")?;
                        }
                        fmt_properties(&node.properties, f)?;
                        write!(f, "}}")?;
                    }
                    write!(f, ")")?;
                }
                write!(f, ">")
            }
            Value::Map(map) => {
                write!(f, "{{")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            Value::Date(d) => write!(f, "{d}"),
            Value::LocalTime(t) => write!(f, "{t}"),
            Value::Time(t) => write!(f, "{t}"),
            Value::LocalDateTime(dt) => write!(f, "{dt}"),
            Value::DateTime(dt) => write!(f, "{dt}"),
            Value::Duration(d) => write!(f, "{d}"),
        }
    }
}

/// Property map for nodes and edges.
pub type Properties = HashMap<String, Value>;

/// A node in the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    /// Node labels (sorted). Empty vec for unlabeled nodes.
    /// Backward-compat: deserializes from either `label: String` or `labels: Vec`.
    #[serde(default, alias = "label", deserialize_with = "deserialize_labels")]
    pub labels: Vec<String>,
    pub properties: Properties,
}

/// An edge in the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub label: String,
    pub properties: Properties,
}

/// A path through the graph: a sequence of nodes linked by edges.
///
/// Invariant: `nodes.len() == edges.len() + 1`. An edge at index `i` connects
/// the node at index `i` to the node at index `i + 1`. A path with a single
/// node (and zero edges) represents a length-zero path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathValue {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl PathValue {
    /// Construct a zero-length path (single node, no edges).
    pub fn single(node: Node) -> Self {
        Self {
            nodes: vec![node],
            edges: Vec::new(),
        }
    }

    /// Number of edges in the path.
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// A zero-length path (single node only).
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// Direction for edge traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

/// Serializable node record stored in the `nodes` table.
#[derive(Serialize, Deserialize)]
pub(crate) struct NodeRecord {
    #[serde(default, alias = "label", deserialize_with = "deserialize_labels")]
    pub labels: Vec<String>,
    pub properties: Properties,
}

/// Deserialize labels from either a single string ("A") or a vec (["A", "B"]).
/// This handles backward compat with the old `label: String` format.
fn deserialize_labels<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct LabelsVisitor;

    impl<'de> de::Visitor<'de> for LabelsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a string or list of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Vec<String>, E> {
            if v.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![v.to_string()])
            }
        }

        fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Vec<String>, E> {
            if v.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![v])
            }
        }

        fn visit_seq<A: de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Vec<String>, A::Error> {
            let mut labels = Vec::new();
            while let Some(s) = seq.next_element()? {
                labels.push(s);
            }
            Ok(labels)
        }
    }

    deserializer.deserialize_any(LabelsVisitor)
}

/// Phase of query processing at which an error was raised.
///
/// Aligns with openCypher's error model so TCK scenarios that assert
/// "an error should be raised at <phase>" can match precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryPhase {
    /// Lexing / parsing the Cypher source text.
    Parse,
    /// After parsing, before execution: variable binding, pattern validation,
    /// type inference, parameter substitution.
    SemanticAnalysis,
    /// During plan execution.
    Runtime,
}

impl fmt::Display for QueryPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryPhase::Parse => write!(f, "parse"),
            QueryPhase::SemanticAnalysis => write!(f, "semantic analysis"),
            QueryPhase::Runtime => write!(f, "runtime"),
        }
    }
}

/// Errors raised while processing a Cypher query.
///
/// Categorized along the openCypher error taxonomy (SyntaxError, TypeError,
/// SemanticError, etc.) so that TCK conformance scenarios can distinguish
/// error kinds without resorting to string matching.
#[derive(Debug, Error)]
pub enum QueryError {
    #[error("syntax error at {phase}: {message}")]
    SyntaxError { phase: QueryPhase, message: String },

    #[error("type error at {phase}: {message}")]
    TypeError { phase: QueryPhase, message: String },

    #[error("semantic error at {phase}: {message}")]
    SemanticError { phase: QueryPhase, message: String },

    #[error("entity not found at {phase}: {message}")]
    EntityNotFound { phase: QueryPhase, message: String },

    #[error("argument error at {phase}: {message}")]
    ArgumentError { phase: QueryPhase, message: String },

    #[error("arithmetic error at {phase}: {message}")]
    ArithmeticError { phase: QueryPhase, message: String },

    #[error("constraint violation at {phase}: {message}")]
    ConstraintViolation { phase: QueryPhase, message: String },

    #[error("procedure error at {phase}: {message}")]
    ProcedureError { phase: QueryPhase, message: String },
}

impl QueryError {
    /// The phase at which this error was raised.
    pub fn phase(&self) -> QueryPhase {
        match self {
            QueryError::SyntaxError { phase, .. }
            | QueryError::TypeError { phase, .. }
            | QueryError::SemanticError { phase, .. }
            | QueryError::EntityNotFound { phase, .. }
            | QueryError::ArgumentError { phase, .. }
            | QueryError::ArithmeticError { phase, .. }
            | QueryError::ConstraintViolation { phase, .. }
            | QueryError::ProcedureError { phase, .. } => *phase,
        }
    }

    /// Short tag identifying the error kind (matches openCypher category names).
    pub fn kind(&self) -> &'static str {
        match self {
            QueryError::SyntaxError { .. } => "SyntaxError",
            QueryError::TypeError { .. } => "TypeError",
            QueryError::SemanticError { .. } => "SemanticError",
            QueryError::EntityNotFound { .. } => "EntityNotFound",
            QueryError::ArgumentError { .. } => "ArgumentError",
            QueryError::ArithmeticError { .. } => "ArithmeticError",
            QueryError::ConstraintViolation { .. } => "ConstraintViolation",
            QueryError::ProcedureError { .. } => "ProcedureError",
        }
    }
}

/// All errors returned by graphdblite.
#[derive(Debug, Error)]
pub enum GraphError {
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    /// Cypher query processing error (parse/semantic/runtime).
    #[error("{0}")]
    Query(#[from] QueryError),

    #[error("node not found")]
    NodeNotFound(NodeId),

    #[error("edge not found")]
    EdgeNotFound(NodeId, String, NodeId),

    #[error(
        "cannot delete node because it still has edges; use DETACH DELETE to remove edges too"
    )]
    HasEdges(NodeId),

    /// Internal transaction/concurrency errors (not Cypher query errors).
    #[error("transaction error: {0}")]
    Transaction(String),

    #[error("index already exists: {0}.{1}")]
    IndexAlreadyExists(String, String),

    #[error("index not found: {0}.{1}")]
    IndexNotFound(String, String),

    #[error("invalid name '{0}': must contain only ASCII letters, digits, or underscores")]
    InvalidName(String),

    #[error("{0}")]
    SizeLimit(String),

    #[error("schema version mismatch: database is v{0}, this library supports up to v{1}")]
    SchemaMismatch(u64, u64),
}

impl GraphError {
    /// Convenience constructor for syntax errors at the parse phase.
    pub fn syntax(message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::SyntaxError {
            phase: QueryPhase::Parse,
            message: message.into(),
        })
    }

    /// Convenience constructor for semantic errors at the analysis phase.
    pub fn semantic(message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::SemanticError {
            phase: QueryPhase::SemanticAnalysis,
            message: message.into(),
        })
    }

    /// Convenience constructor for runtime constraint violations.
    pub fn constraint(message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::ConstraintViolation {
            phase: QueryPhase::Runtime,
            message: message.into(),
        })
    }

    /// Convenience constructor for type errors.
    pub fn type_error(phase: QueryPhase, message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::TypeError {
            phase,
            message: message.into(),
        })
    }

    /// Convenience constructor for argument errors (e.g. missing parameters).
    pub fn argument(phase: QueryPhase, message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::ArgumentError {
            phase,
            message: message.into(),
        })
    }
}

pub type Result<T> = std::result::Result<T, GraphError>;

/// Validate that a name (label, property key) contains only `[A-Za-z0-9_]`.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(GraphError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// Validate name length against a configured maximum.
pub fn validate_name_length(name: &str, max_bytes: usize) -> Result<()> {
    if name.len() > max_bytes {
        return Err(GraphError::SizeLimit(format!(
            "name '{}...' exceeds maximum length of {max_bytes} bytes",
            &name[..max_bytes.min(32)]
        )));
    }
    Ok(())
}

/// Estimate the byte size of a property value.
///
/// Note: `Node`, `Edge`, and `Path` are runtime-only variants and should never
/// appear in stored properties, but we compute a sensible size for them
/// anyway so this helper stays total.
fn value_byte_size(val: &Value) -> usize {
    match val {
        Value::Null | Value::Bool(_) | Value::I64(_) | Value::F64(_) => 8,
        Value::String(s) => s.len(),
        Value::List(items) => items.iter().map(value_byte_size).sum(),
        Value::Node(n) => node_byte_size(n),
        Value::Edge(e) => edge_byte_size(e),
        Value::Path(p) => {
            p.nodes.iter().map(node_byte_size).sum::<usize>()
                + p.edges.iter().map(edge_byte_size).sum::<usize>()
        }
        Value::Map(map) => map.iter().map(|(k, v)| k.len() + value_byte_size(v)).sum(),
        Value::Date(_) => 12,
        Value::LocalTime(_) => 16,
        Value::Time(_) => 20,
        Value::LocalDateTime(_) => 20,
        Value::DateTime(_) => 28,
        Value::Duration(_) => 32,
    }
}

fn node_byte_size(n: &Node) -> usize {
    8 + n.labels.iter().map(|l| l.len()).sum::<usize>()
        + n.properties
            .iter()
            .map(|(k, v)| k.len() + value_byte_size(v))
            .sum::<usize>()
}

fn edge_byte_size(e: &Edge) -> usize {
    16 + e.label.len()
        + e.properties
            .iter()
            .map(|(k, v)| k.len() + value_byte_size(v))
            .sum::<usize>()
}

/// Validate that all property values are within size limits.
pub fn validate_properties(
    label: &str,
    properties: &Properties,
    max_name_bytes: usize,
    max_value_bytes: usize,
) -> Result<()> {
    validate_name_length(label, max_name_bytes)?;
    for (key, val) in properties {
        validate_name_length(key, max_name_bytes)?;
        let size = value_byte_size(val);
        if size > max_value_bytes {
            return Err(GraphError::SizeLimit(format!(
                "property '{key}' value ({size} bytes) exceeds maximum of {max_value_bytes} bytes"
            )));
        }
    }
    Ok(())
}
