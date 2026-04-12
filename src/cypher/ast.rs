use std::collections::HashMap;

/// Top-level Cypher statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Match(MatchStatement),
    Create(CreateStatement),
    MatchCreate(MatchCreateStatement),
    Delete(DeleteStatement),
    Set(SetStatement),
    Merge(MergeStatement),
}

/// MATCH ... WHERE ... RETURN ... ORDER BY ... LIMIT
#[derive(Debug, Clone, PartialEq)]
pub struct MatchStatement {
    pub patterns: Vec<Pattern>,
    pub optional_patterns: Vec<Vec<Pattern>>,
    pub where_clause: Option<Expr>,
    pub return_clause: ReturnClause,
    pub order_by: Vec<SortItem>,
    pub limit: Option<u64>,
}

/// CREATE (n:Label {props})-[:TYPE]->(m:Label)
#[derive(Debug, Clone, PartialEq)]
pub struct CreateStatement {
    pub patterns: Vec<Pattern>,
}

/// MATCH ... CREATE (a)-[:TYPE]->(b)
#[derive(Debug, Clone, PartialEq)]
pub struct MatchCreateStatement {
    pub patterns: Vec<Pattern>,
    pub where_clause: Option<Expr>,
    pub create_patterns: Vec<Pattern>,
}

/// MATCH ... DELETE n, m
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub patterns: Vec<Pattern>,
    pub where_clause: Option<Expr>,
    pub variables: Vec<String>,
}

/// MATCH ... SET n.prop = value
#[derive(Debug, Clone, PartialEq)]
pub struct SetStatement {
    pub patterns: Vec<Pattern>,
    pub where_clause: Option<Expr>,
    pub assignments: Vec<Assignment>,
}

/// MERGE (n:Label {props}) ON CREATE SET ... ON MATCH SET ...
#[derive(Debug, Clone, PartialEq)]
pub struct MergeStatement {
    pub pattern: Pattern,
    pub on_create: Vec<Assignment>,
    pub on_match: Vec<Assignment>,
}

/// A graph pattern: sequence of node and relationship elements.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub elements: Vec<PatternElement>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelPattern),
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub label: Option<String>,
    pub properties: HashMap<String, Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub variable: Option<String>,
    pub rel_type: Option<String>,
    pub direction: RelDirection,
    pub var_length: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDirection {
    Outgoing,  // -[]->)
    Incoming,  // <-[]-
    Undirected, // -[]-
}

/// RETURN clause with optional ORDER BY and LIMIT.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub items: Vec<ReturnItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortItem {
    pub expr: Expr,
    pub descending: bool,
}

/// Property assignment: n.prop = value
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub variable: String,
    pub property: String,
    pub value: Expr,
}

/// Expression AST node.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Literal value (string, int, float, bool, null).
    Literal(LiteralValue),
    /// Property access: variable.property
    Property(String, String),
    /// Variable reference.
    Variable(String),
    /// Binary operation: left op right
    BinaryOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    /// Unary NOT.
    Not(Box<Expr>),
    /// IS NULL check.
    IsNull(Box<Expr>),
    /// IS NOT NULL check.
    IsNotNull(Box<Expr>),
    /// Function call: name(args)
    FunctionCall {
        name: String,
        args: Vec<Expr>,
    },
    /// Wildcard * (used in count(*) and RETURN *)
    Star,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiteralValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
    And,
    Or,
    StartsWith,
    Contains,
}
