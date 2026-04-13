use std::collections::HashMap;
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
    List(Vec<Value>),
    Path(Vec<NodeId>),
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
            (Value::Path(a), Value::Path(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

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
            Value::Path(nodes) => nodes.hash(state),
        }
    }
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
            Value::Path(nodes) => {
                write!(f, "<")?;
                for (i, id) in nodes.iter().enumerate() {
                    if i > 0 {
                        write!(f, "--")?;
                    }
                    write!(f, "({})", id.0)?;
                }
                write!(f, ">")
            }
        }
    }
}

/// Property map for nodes and edges.
pub type Properties = HashMap<String, Value>;

/// A node in the graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: NodeId,
    pub label: String,
    pub properties: Properties,
}

/// An edge in the graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub label: String,
    pub properties: Properties,
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
    pub label: String,
    pub properties: Properties,
}

/// All errors returned by graphdblite.
#[derive(Debug, Error)]
pub enum GraphError {
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("{0}")]
    ParseError(String),

    #[error("node not found")]
    NodeNotFound(NodeId),

    #[error("edge not found")]
    EdgeNotFound(NodeId, String, NodeId),

    #[error("cannot delete node because it still has edges; use DETACH DELETE to remove edges too")]
    HasEdges(NodeId),

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
fn value_byte_size(val: &Value) -> usize {
    match val {
        Value::Null | Value::Bool(_) | Value::I64(_) | Value::F64(_) => 8,
        Value::String(s) => s.len(),
        Value::List(items) => items.iter().map(value_byte_size).sum(),
        Value::Path(nodes) => nodes.len() * 8,
    }
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
