use std::collections::HashMap;
use std::fmt;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::I64(n) => write!(f, "{n}"),
            Value::F64(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "\"{s}\""),
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

    #[error("node not found: {0}")]
    NodeNotFound(NodeId),

    #[error("edge not found: {0} -[{1}]-> {2}")]
    EdgeNotFound(NodeId, String, NodeId),

    #[error("transaction error: {0}")]
    Transaction(String),

    #[error("index already exists: {0}.{1}")]
    IndexAlreadyExists(String, String),

    #[error("index not found: {0}.{1}")]
    IndexNotFound(String, String),
}

pub type Result<T> = std::result::Result<T, GraphError>;
