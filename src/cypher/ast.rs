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
    Unwind(UnwindStatement),
}

/// MATCH ... WHERE ... WITH ... RETURN ... ORDER BY ... LIMIT
#[derive(Debug, Clone, PartialEq)]
pub struct MatchStatement {
    pub patterns: Vec<Pattern>,
    pub optional_patterns: Vec<Vec<Pattern>>,
    pub where_clause: Option<Expr>,
    pub intermediate_clauses: Vec<IntermediateClause>,
    pub return_clause: ReturnClause,
    pub order_by: Vec<SortItem>,
    pub limit: Option<u64>,
}

/// WITH clause: intermediate projection/filter/aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    pub items: Vec<ReturnItem>,
    pub where_clause: Option<Expr>,
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

/// MATCH ... [DETACH] DELETE n, m
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub patterns: Vec<Pattern>,
    pub where_clause: Option<Expr>,
    pub detach: bool,
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

/// UNWIND expr AS alias [WHERE ...] RETURN ... / CREATE ...
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindStatement {
    pub expr: Expr,
    pub alias: String,
    pub body: UnwindBody,
}

/// What follows the UNWIND clause.
#[derive(Debug, Clone, PartialEq)]
pub enum UnwindBody {
    Return {
        where_clause: Option<Expr>,
        return_clause: ReturnClause,
        order_by: Vec<SortItem>,
        limit: Option<u64>,
    },
    Create {
        patterns: Vec<Pattern>,
    },
}

/// Intermediate clause (WITH or UNWIND) within a MATCH statement.
#[derive(Debug, Clone, PartialEq)]
pub enum IntermediateClause {
    With(WithClause),
    Unwind(UnwindClause),
}

/// UNWIND clause within a MATCH statement.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    pub expr: Expr,
    pub alias: String,
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
    /// CASE WHEN cond THEN result ... ELSE default END
    Case {
        alternatives: Vec<(Box<Expr>, Box<Expr>)>,
        default: Option<Box<Expr>>,
    },
    /// List literal: [expr, expr, ...]
    List(Vec<Expr>),
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
    EndsWith,
    Contains,
}
