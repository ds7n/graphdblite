use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

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
#[allow(missing_docs)]
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
                for (i, (edge, node)) in p.edges.iter().zip(p.nodes.iter().skip(1)).enumerate() {
                    // Determine if edge goes forward (src=prev node) or backward.
                    let prev_node = &p.nodes[i];
                    let forward = prev_node.id == edge.src;
                    if forward {
                        write!(f, "-[:{}", edge.label)?;
                    } else {
                        write!(f, "<-[:{}", edge.label)?;
                    }
                    if !edge.properties.is_empty() {
                        write!(f, " {{")?;
                        fmt_properties(&edge.properties, f)?;
                        write!(f, "}}")?;
                    }
                    if forward {
                        write!(f, "]->(")?;
                    } else {
                        write!(f, "]-(")?;
                    }
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
#[allow(missing_docs)]
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
#[allow(missing_docs)]
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
#[allow(missing_docs)]
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
#[allow(missing_docs)]
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
/// "an error should be raised at `<phase>`" can match precisely.
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

/// A source-text span (zero-indexed byte offsets, 1-indexed line/column).
///
/// Lightweight and parser-library-agnostic — pest spans are converted to this
/// shape at the parser/AST boundary so downstream code never sees pest types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub col: u32,
}

impl Span {
    /// A placeholder span for AST nodes synthesized by the planner (no source location).
    pub const fn synthetic() -> Self {
        Self {
            start: 0,
            end: 0,
            line: 0,
            col: 0,
        }
    }

    /// True if this span has no source location (was synthesized).
    pub fn is_synthetic(&self) -> bool {
        self.line == 0 && self.col == 0 && self.start == 0 && self.end == 0
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}:{}", self.line, self.col)
    }
}

/// Structured openCypher error code.
///
/// Encodes the specific category beyond the broader `QueryError` variant
/// (e.g. a `SemanticError` may have code `UndefinedVariable` or
/// `VariableTypeConflict`). Keeping this typed prevents drift across call
/// sites and lets the TCK harness match without parsing strings.
///
/// `#[non_exhaustive]` so adding new codes never breaks downstream `match`es.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum ErrorCode {
    /// Generic / no specific code.
    Other,
    AmbiguousAggregationExpression,
    CreatingVarLength,
    DeleteConnectedNode,
    DeletedEntityAccess,
    DifferentColumnsInUnion,
    InvalidAggregation,
    InvalidArgumentPassingMode,
    InvalidArgumentType,
    InvalidArgumentValue,
    InvalidClauseComposition,
    InvalidDelete,
    InvalidNumberOfArguments,
    InvalidPropertyType,
    InvalidUnicodeLiteral,
    MapElementAccessByNonString,
    MergeReadOwnWrites,
    MissingParameter,
    NegativeIntegerArgument,
    NoExpressionAlias,
    NoSingleRelationshipType,
    NonConstantExpression,
    NumberOutOfRange,
    ProcedureNotFound,
    RequiresDirectedRelationship,
    UndefinedVariable,
    UnknownFunction,
    UnexpectedSyntax,
    VariableAlreadyBound,
    VariableTypeConflict,
}

impl ErrorCode {
    /// The exact openCypher code name (e.g. `"UndefinedVariable"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::Other => "Other",
            ErrorCode::AmbiguousAggregationExpression => "AmbiguousAggregationExpression",
            ErrorCode::CreatingVarLength => "CreatingVarLength",
            ErrorCode::DeleteConnectedNode => "DeleteConnectedNode",
            ErrorCode::DeletedEntityAccess => "DeletedEntityAccess",
            ErrorCode::DifferentColumnsInUnion => "DifferentColumnsInUnion",
            ErrorCode::InvalidAggregation => "InvalidAggregation",
            ErrorCode::InvalidArgumentPassingMode => "InvalidArgumentPassingMode",
            ErrorCode::InvalidArgumentType => "InvalidArgumentType",
            ErrorCode::InvalidArgumentValue => "InvalidArgumentValue",
            ErrorCode::InvalidClauseComposition => "InvalidClauseComposition",
            ErrorCode::InvalidDelete => "InvalidDelete",
            ErrorCode::InvalidNumberOfArguments => "InvalidNumberOfArguments",
            ErrorCode::InvalidPropertyType => "InvalidPropertyType",
            ErrorCode::InvalidUnicodeLiteral => "InvalidUnicodeLiteral",
            ErrorCode::MapElementAccessByNonString => "MapElementAccessByNonString",
            ErrorCode::MergeReadOwnWrites => "MergeReadOwnWrites",
            ErrorCode::MissingParameter => "MissingParameter",
            ErrorCode::NegativeIntegerArgument => "NegativeIntegerArgument",
            ErrorCode::NoExpressionAlias => "NoExpressionAlias",
            ErrorCode::NoSingleRelationshipType => "NoSingleRelationshipType",
            ErrorCode::NonConstantExpression => "NonConstantExpression",
            ErrorCode::NumberOutOfRange => "NumberOutOfRange",
            ErrorCode::ProcedureNotFound => "ProcedureNotFound",
            ErrorCode::RequiresDirectedRelationship => "RequiresDirectedRelationship",
            ErrorCode::UndefinedVariable => "UndefinedVariable",
            ErrorCode::UnknownFunction => "UnknownFunction",
            ErrorCode::UnexpectedSyntax => "UnexpectedSyntax",
            ErrorCode::VariableAlreadyBound => "VariableAlreadyBound",
            ErrorCode::VariableTypeConflict => "VariableTypeConflict",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors raised while processing a Cypher query.
///
/// Categorized along the openCypher error taxonomy (SyntaxError, TypeError,
/// SemanticError, etc.) so that TCK conformance scenarios can distinguish
/// error kinds without resorting to string matching.
///
/// All variants carry the same shape: `phase`, `code`, `message`, `hint`,
/// `span`. The variant tag itself is the broad openCypher kind; `code`
/// narrows it further. `hint` and `span` are optional — populate when they
/// add value (e.g. parser errors fill `span`; "did-you-mean" fills `hint`).
#[derive(Debug)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum QueryError {
    SyntaxError {
        phase: QueryPhase,
        code: ErrorCode,
        message: String,
        hint: Option<String>,
        span: Option<Span>,
    },
    TypeError {
        phase: QueryPhase,
        code: ErrorCode,
        message: String,
        hint: Option<String>,
        span: Option<Span>,
    },
    SemanticError {
        phase: QueryPhase,
        code: ErrorCode,
        message: String,
        hint: Option<String>,
        span: Option<Span>,
    },
    EntityNotFound {
        phase: QueryPhase,
        code: ErrorCode,
        message: String,
        hint: Option<String>,
        span: Option<Span>,
    },
    ArgumentError {
        phase: QueryPhase,
        code: ErrorCode,
        message: String,
        hint: Option<String>,
        span: Option<Span>,
    },
    ArithmeticError {
        phase: QueryPhase,
        code: ErrorCode,
        message: String,
        hint: Option<String>,
        span: Option<Span>,
    },
    ConstraintViolation {
        phase: QueryPhase,
        code: ErrorCode,
        message: String,
        hint: Option<String>,
        span: Option<Span>,
    },
    ProcedureError {
        phase: QueryPhase,
        code: ErrorCode,
        message: String,
        hint: Option<String>,
        span: Option<Span>,
    },
}

/// Helper to extract the common fields shared by every `QueryError` variant.
macro_rules! query_error_fields {
    ($self:expr) => {
        match $self {
            QueryError::SyntaxError {
                phase,
                code,
                message,
                hint,
                span,
            }
            | QueryError::TypeError {
                phase,
                code,
                message,
                hint,
                span,
            }
            | QueryError::SemanticError {
                phase,
                code,
                message,
                hint,
                span,
            }
            | QueryError::EntityNotFound {
                phase,
                code,
                message,
                hint,
                span,
            }
            | QueryError::ArgumentError {
                phase,
                code,
                message,
                hint,
                span,
            }
            | QueryError::ArithmeticError {
                phase,
                code,
                message,
                hint,
                span,
            }
            | QueryError::ConstraintViolation {
                phase,
                code,
                message,
                hint,
                span,
            }
            | QueryError::ProcedureError {
                phase,
                code,
                message,
                hint,
                span,
            } => (phase, code, message, hint, span),
        }
    };
}

impl QueryError {
    /// The phase at which this error was raised.
    pub fn phase(&self) -> QueryPhase {
        let (phase, _, _, _, _) = query_error_fields!(self);
        *phase
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

    /// Structured error code.
    pub fn code(&self) -> ErrorCode {
        let (_, code, _, _, _) = query_error_fields!(self);
        *code
    }

    /// Free-form human message.
    pub fn message(&self) -> &str {
        let (_, _, message, _, _) = query_error_fields!(self);
        message.as_str()
    }

    /// Actionable hint, if any.
    pub fn hint(&self) -> Option<&str> {
        let (_, _, _, hint, _) = query_error_fields!(self);
        hint.as_deref()
    }

    /// Source-text span, if known.
    pub fn span(&self) -> Option<Span> {
        let (_, _, _, _, span) = query_error_fields!(self);
        *span
    }

    /// Replace the hint on this error.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        let (_, _, _, hint_field, _) = query_error_fields!(&mut self);
        *hint_field = Some(hint.into());
        self
    }

    /// Replace the span on this error. Synthetic spans (planner-rewritten nodes)
    /// are ignored so error messages don't display "line 0:0".
    pub fn with_span(mut self, new_span: Span) -> Self {
        if new_span.is_synthetic() {
            return self;
        }
        let (_, _, _, _, span_field) = query_error_fields!(&mut self);
        *span_field = Some(new_span);
        self
    }

    /// Replace the structured code on this error.
    pub fn with_code(mut self, new_code: ErrorCode) -> Self {
        let (_, code_field, _, _, _) = query_error_fields!(&mut self);
        *code_field = new_code;
        self
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (phase, code, message, hint, span) = query_error_fields!(self);
        match span {
            Some(s) => write!(f, "{}({}) at {}: {}", self.kind(), code, s, message)?,
            None => write!(f, "{}({}) at {}: {}", self.kind(), code, phase, message)?,
        }
        if let Some(h) = hint {
            write!(f, "\n  hint: {h}")?;
        }
        Ok(())
    }
}

impl std::error::Error for QueryError {}

/// All errors returned by graphdblite.
///
/// Every variant (except `Query`, which delegates to `QueryError`'s own hint)
/// carries a `hint: Option<String>` for actionable suggestions.
#[derive(Debug)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum GraphError {
    Storage {
        source: rusqlite::Error,
        hint: Option<String>,
    },
    Serialization {
        context: String,
        source: String,
        hint: Option<String>,
    },
    /// Cypher query processing error (parse/semantic/runtime).
    Query(QueryError),
    NodeNotFound {
        id: NodeId,
        hint: Option<String>,
    },
    EdgeNotFound {
        src: NodeId,
        label: String,
        dst: NodeId,
        hint: Option<String>,
    },
    HasEdges {
        id: NodeId,
        hint: Option<String>,
    },
    /// Internal transaction / concurrency error.
    Transaction {
        message: String,
        hint: Option<String>,
    },
    IndexAlreadyExists {
        label: String,
        property: String,
        hint: Option<String>,
    },
    IndexNotFound {
        label: String,
        property: String,
        hint: Option<String>,
    },
    InvalidName {
        name: String,
        hint: Option<String>,
    },
    SizeLimit {
        what: String,
        limit: usize,
        actual: usize,
        hint: Option<String>,
    },
    SchemaMismatch {
        found: u64,
        supported: u64,
        hint: Option<String>,
    },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Storage { source, hint } => {
                write!(f, "storage error: {source}")?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::Serialization {
                context,
                source,
                hint,
            } => {
                write!(f, "serialization error ({context}): {source}")?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::Query(q) => q.fmt(f),
            GraphError::NodeNotFound { id, hint } => {
                write!(f, "node not found: {id}")?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::EdgeNotFound {
                src,
                label,
                dst,
                hint,
            } => {
                write!(f, "edge not found: {src} -[:{label}]-> {dst}")?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::HasEdges { id, hint } => {
                write!(
                    f,
                    "cannot delete node {id} because it still has edges; use DETACH DELETE to remove edges too"
                )?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::Transaction { message, hint } => {
                write!(f, "transaction error: {message}")?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::IndexAlreadyExists {
                label,
                property,
                hint,
            } => {
                write!(f, "index already exists: {label}.{property}")?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::IndexNotFound {
                label,
                property,
                hint,
            } => {
                write!(f, "index not found: {label}.{property}")?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::InvalidName { name, hint } => {
                write!(
                    f,
                    "invalid name '{name}': must contain only ASCII letters, digits, or underscores"
                )?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::SizeLimit {
                what,
                limit,
                actual,
                hint,
            } => {
                write!(
                    f,
                    "{what} ({actual} bytes) exceeds maximum of {limit} bytes"
                )?;
                fmt_hint(f, hint.as_deref())
            }
            GraphError::SchemaMismatch {
                found,
                supported,
                hint,
            } => {
                write!(
                    f,
                    "schema version mismatch: database is v{found}, this library supports up to v{supported}"
                )?;
                fmt_hint(f, hint.as_deref())
            }
        }
    }
}

fn fmt_hint(f: &mut fmt::Formatter<'_>, hint: Option<&str>) -> fmt::Result {
    if let Some(h) = hint {
        write!(f, "\n  hint: {h}")?;
    }
    Ok(())
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GraphError::Storage { source, .. } => Some(source),
            GraphError::Query(q) => Some(q),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for GraphError {
    fn from(source: rusqlite::Error) -> Self {
        GraphError::Storage { source, hint: None }
    }
}

impl From<QueryError> for GraphError {
    fn from(error: QueryError) -> Self {
        GraphError::Query(error)
    }
}

impl GraphError {
    // -- Simple, code-less constructors (kept for brevity at call sites
    //    where no specific openCypher code applies).

    /// Convenience constructor for syntax errors at the parse phase.
    pub fn syntax(message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::SyntaxError {
            phase: QueryPhase::Parse,
            code: ErrorCode::Other,
            message: message.into(),
            hint: None,
            span: None,
        })
    }

    /// Convenience constructor for semantic errors at the analysis phase.
    pub fn semantic(message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::SemanticError {
            phase: QueryPhase::SemanticAnalysis,
            code: ErrorCode::Other,
            message: message.into(),
            hint: None,
            span: None,
        })
    }

    /// Convenience constructor for runtime constraint violations.
    pub fn constraint(message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::ConstraintViolation {
            phase: QueryPhase::Runtime,
            code: ErrorCode::Other,
            message: message.into(),
            hint: None,
            span: None,
        })
    }

    /// Convenience constructor for type errors.
    pub fn type_error(phase: QueryPhase, message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::TypeError {
            phase,
            code: ErrorCode::Other,
            message: message.into(),
            hint: None,
            span: None,
        })
    }

    /// Convenience constructor for argument errors (e.g. missing parameters).
    pub fn argument(phase: QueryPhase, message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::ArgumentError {
            phase,
            code: ErrorCode::Other,
            message: message.into(),
            hint: None,
            span: None,
        })
    }

    // -- Structured helpers (preferred — pin down the openCypher code).

    /// Reference to a name not bound in the current scope.
    pub fn undefined_variable(name: impl fmt::Display) -> Self {
        GraphError::Query(QueryError::SemanticError {
            phase: QueryPhase::SemanticAnalysis,
            code: ErrorCode::UndefinedVariable,
            message: format!("variable `{name}` is not defined"),
            hint: None,
            span: None,
        })
    }

    /// Procedure name not registered in the procedure registry.
    pub fn procedure_not_found(name: impl fmt::Display) -> Self {
        GraphError::Query(QueryError::ProcedureError {
            phase: QueryPhase::SemanticAnalysis,
            code: ErrorCode::ProcedureNotFound,
            message: format!("unknown procedure `{name}`"),
            hint: None,
            span: None,
        })
    }

    /// Function called with a value of an unsupported type.
    pub fn invalid_argument_type(
        phase: QueryPhase,
        function: impl fmt::Display,
        got: impl fmt::Display,
    ) -> Self {
        GraphError::Query(QueryError::TypeError {
            phase,
            code: ErrorCode::InvalidArgumentType,
            message: format!("{got} is not a valid argument type for {function}"),
            hint: None,
            span: None,
        })
    }

    /// Value passed to a function is out of the supported range.
    pub fn invalid_argument_value(
        phase: QueryPhase,
        function: impl fmt::Display,
        message: impl fmt::Display,
    ) -> Self {
        GraphError::Query(QueryError::ArgumentError {
            phase,
            code: ErrorCode::InvalidArgumentValue,
            message: format!("{function}: {message}"),
            hint: None,
            span: None,
        })
    }

    /// Numeric overflow / value outside the representable range.
    pub fn number_out_of_range(phase: QueryPhase, message: impl Into<String>) -> Self {
        GraphError::Query(QueryError::ArithmeticError {
            phase,
            code: ErrorCode::NumberOutOfRange,
            message: message.into(),
            hint: None,
            span: None,
        })
    }

    /// Lower-level builder for arbitrary query errors with a known code.
    pub fn query(phase: QueryPhase, code: ErrorCode, message: impl Into<String>) -> Self {
        let message = message.into();
        let mk = |kind: fn(QueryPhase, ErrorCode, String) -> QueryError| -> Self {
            GraphError::Query(kind(phase, code, message))
        };
        // Choose the variant from the code's natural openCypher kind.
        match code {
            ErrorCode::InvalidUnicodeLiteral
            | ErrorCode::InvalidClauseComposition
            | ErrorCode::UnexpectedSyntax => mk(QueryError::syntax_with),
            ErrorCode::InvalidArgumentType
            | ErrorCode::InvalidPropertyType
            | ErrorCode::MapElementAccessByNonString
            | ErrorCode::DeletedEntityAccess => mk(QueryError::type_with),
            ErrorCode::UndefinedVariable
            | ErrorCode::VariableAlreadyBound
            | ErrorCode::VariableTypeConflict
            | ErrorCode::AmbiguousAggregationExpression
            | ErrorCode::CreatingVarLength
            | ErrorCode::DifferentColumnsInUnion
            | ErrorCode::InvalidAggregation
            | ErrorCode::InvalidArgumentPassingMode
            | ErrorCode::InvalidArgumentValue
            | ErrorCode::InvalidDelete
            | ErrorCode::InvalidNumberOfArguments
            | ErrorCode::NegativeIntegerArgument
            | ErrorCode::NoExpressionAlias
            | ErrorCode::NoSingleRelationshipType
            | ErrorCode::NonConstantExpression
            | ErrorCode::RequiresDirectedRelationship => mk(QueryError::semantic_with),
            ErrorCode::MissingParameter => mk(QueryError::argument_with),
            ErrorCode::NumberOutOfRange => mk(QueryError::arithmetic_with),
            ErrorCode::DeleteConnectedNode | ErrorCode::MergeReadOwnWrites => {
                mk(QueryError::constraint_with)
            }
            ErrorCode::ProcedureNotFound => mk(QueryError::procedure_with),
            ErrorCode::UnknownFunction => mk(QueryError::syntax_with),
            ErrorCode::Other => mk(QueryError::semantic_with),
        }
    }

    /// Attach a hint to a query error in place. No-op for non-Query variants
    /// that already have a `hint` field — call the variant constructor with
    /// the hint instead.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        match &mut self {
            GraphError::Query(q) => {
                let (_, _, _, h, _) = query_error_fields!(q);
                *h = Some(hint.into());
            }
            GraphError::Storage { hint: h, .. }
            | GraphError::Serialization { hint: h, .. }
            | GraphError::NodeNotFound { hint: h, .. }
            | GraphError::EdgeNotFound { hint: h, .. }
            | GraphError::HasEdges { hint: h, .. }
            | GraphError::Transaction { hint: h, .. }
            | GraphError::IndexAlreadyExists { hint: h, .. }
            | GraphError::IndexNotFound { hint: h, .. }
            | GraphError::InvalidName { hint: h, .. }
            | GraphError::SizeLimit { hint: h, .. }
            | GraphError::SchemaMismatch { hint: h, .. } => *h = Some(hint.into()),
        }
        self
    }

    /// Attach a structured code to a query error. No-op for non-Query variants.
    pub fn with_code(mut self, code: ErrorCode) -> Self {
        if let GraphError::Query(q) = &mut self {
            let (_, c, _, _, _) = query_error_fields!(q);
            *c = code;
        }
        self
    }

    /// Attach a source-text span to a query error. No-op for non-Query variants
    /// or for synthetic spans (planner-rewritten nodes).
    pub fn with_span(self, span: Span) -> Self {
        if span.is_synthetic() {
            return self;
        }
        match self {
            GraphError::Query(q) => GraphError::Query(q.with_span(span)),
            other => other,
        }
    }

    /// Wrap an internal serialization failure with a context tag.
    pub fn serialization(context: impl Into<String>, source: impl fmt::Display) -> Self {
        GraphError::Serialization {
            context: context.into(),
            source: source.to_string(),
            hint: None,
        }
    }

    /// Wrap an internal transaction failure.
    pub fn transaction(message: impl Into<String>) -> Self {
        GraphError::Transaction {
            message: message.into(),
            hint: None,
        }
    }
}

// -- Internal QueryError constructors used by the dispatch table above. --
impl QueryError {
    fn syntax_with(phase: QueryPhase, code: ErrorCode, message: String) -> Self {
        QueryError::SyntaxError {
            phase,
            code,
            message,
            hint: None,
            span: None,
        }
    }
    fn type_with(phase: QueryPhase, code: ErrorCode, message: String) -> Self {
        QueryError::TypeError {
            phase,
            code,
            message,
            hint: None,
            span: None,
        }
    }
    fn semantic_with(phase: QueryPhase, code: ErrorCode, message: String) -> Self {
        QueryError::SemanticError {
            phase,
            code,
            message,
            hint: None,
            span: None,
        }
    }
    fn argument_with(phase: QueryPhase, code: ErrorCode, message: String) -> Self {
        QueryError::ArgumentError {
            phase,
            code,
            message,
            hint: None,
            span: None,
        }
    }
    fn arithmetic_with(phase: QueryPhase, code: ErrorCode, message: String) -> Self {
        QueryError::ArithmeticError {
            phase,
            code,
            message,
            hint: None,
            span: None,
        }
    }
    fn constraint_with(phase: QueryPhase, code: ErrorCode, message: String) -> Self {
        QueryError::ConstraintViolation {
            phase,
            code,
            message,
            hint: None,
            span: None,
        }
    }
    fn procedure_with(phase: QueryPhase, code: ErrorCode, message: String) -> Self {
        QueryError::ProcedureError {
            phase,
            code,
            message,
            hint: None,
            span: None,
        }
    }
}

/// Convenience alias: `Result<T, GraphError>`.
pub type Result<T> = std::result::Result<T, GraphError>;

/// Validate that a name (label, property key) contains only `[A-Za-z0-9_]`.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(GraphError::InvalidName {
            name: name.to_string(),
            hint: None,
        });
    }
    Ok(())
}

/// Validate name length against a configured maximum.
pub fn validate_name_length(name: &str, max_bytes: usize) -> Result<()> {
    if name.len() > max_bytes {
        return Err(GraphError::SizeLimit {
            what: format!("name '{}...'", &name[..max_bytes.min(32)]),
            limit: max_bytes,
            actual: name.len(),
            hint: None,
        });
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
            return Err(GraphError::SizeLimit {
                what: format!("property '{key}' value"),
                limit: max_value_bytes,
                actual: size,
                hint: None,
            });
        }
    }
    Ok(())
}
