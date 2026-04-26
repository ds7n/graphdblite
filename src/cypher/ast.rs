use std::collections::HashMap;

/// Top-level Cypher statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Match(MatchStatement),
    Create(CreateStatement),
    MatchCreate(MatchCreateStatement),
    MatchMerge(MatchMergeStatement),
    Delete(DeleteStatement),
    Set(SetStatement),
    Remove(RemoveStatement),
    Merge(MergeStatement),
    Unwind(UnwindStatement),
    /// Standalone RETURN (no preceding MATCH / CREATE).
    Return(ReturnStatement),
    /// Multi-clause statement: arbitrary sequences of MATCH/CREATE/MERGE/WITH/UNWIND/SET/REMOVE/DELETE.
    MultiClause(MultiClauseStatement),
    Explain(Box<Statement>),
    /// UNION [ALL] of multiple statements.
    Union {
        statements: Vec<Statement>,
        /// true = UNION ALL (keep duplicates), false = UNION (deduplicate).
        all: bool,
    },
}

/// A clause in a multi-clause statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match {
        patterns: Vec<Pattern>,
        optional_patterns: Vec<OptionalMatch>,
        where_clause: Option<Expr>,
    },
    Create {
        patterns: Vec<Pattern>,
    },
    Merge {
        pattern: Pattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    With(WithClause),
    Unwind(UnwindClause),
    Set {
        items: Vec<SetItem>,
    },
    Remove {
        items: Vec<RemoveItem>,
    },
    Delete {
        exprs: Vec<Expr>,
        detach: bool,
    },
}

/// Multi-clause statement: a sequence of arbitrary clauses with optional RETURN.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiClauseStatement {
    pub clauses: Vec<Clause>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// Standalone `RETURN expr [AS alias], ... [ORDER BY ...] [SKIP n] [LIMIT n]`
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnStatement {
    pub return_clause: ReturnClause,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// An OPTIONAL MATCH clause with its patterns and optional WHERE filter.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionalMatch {
    pub patterns: Vec<Pattern>,
    pub where_clause: Option<Expr>,
}

/// MATCH ... WHERE ... WITH ... RETURN ... ORDER BY ... LIMIT
#[derive(Debug, Clone, PartialEq)]
pub struct MatchStatement {
    pub patterns: Vec<Pattern>,
    pub optional_patterns: Vec<OptionalMatch>,
    pub where_clause: Option<Expr>,
    pub intermediate_clauses: Vec<IntermediateClause>,
    pub return_clause: ReturnClause,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// WITH clause: intermediate projection/filter/aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    pub items: Vec<ReturnItem>,
    pub distinct: bool,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
    pub where_clause: Option<Expr>,
}

/// CREATE (n:Label {props})-[:TYPE]->(m:Label) [RETURN ...]
#[derive(Debug, Clone, PartialEq)]
pub struct CreateStatement {
    pub patterns: Vec<Pattern>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// MATCH ... CREATE (a)-[:TYPE]->(b) [RETURN ...]
#[derive(Debug, Clone, PartialEq)]
pub struct MatchCreateStatement {
    pub patterns: Vec<Pattern>,
    pub where_clause: Option<Expr>,
    pub create_patterns: Vec<Pattern>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// MATCH ... [DETACH] DELETE n, m [RETURN ...]
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub patterns: Vec<Pattern>,
    pub optional_patterns: Vec<OptionalMatch>,
    pub where_clause: Option<Expr>,
    pub detach: bool,
    pub exprs: Vec<Expr>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// MATCH ... SET n.prop = value | n:Label | n = {map} | n += {map} [WITH ...] [RETURN ...]
#[derive(Debug, Clone, PartialEq)]
pub struct SetStatement {
    pub patterns: Vec<Pattern>,
    pub optional_patterns: Vec<OptionalMatch>,
    pub where_clause: Option<Expr>,
    pub items: Vec<SetItem>,
    pub intermediate_clauses: Vec<IntermediateClause>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// A single SET clause item.
#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    Property(Assignment),
    Label {
        variable: String,
        labels: Vec<String>,
    },
    MapOverwrite {
        variable: String,
        value: Expr,
    },
    MapMerge {
        variable: String,
        value: Expr,
    },
}

/// MATCH ... REMOVE n.prop, n:Label [RETURN ...]
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveStatement {
    pub patterns: Vec<Pattern>,
    pub optional_patterns: Vec<OptionalMatch>,
    pub where_clause: Option<Expr>,
    pub items: Vec<RemoveItem>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// An item to remove: property or label(s).
#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    Property {
        variable: String,
        property: String,
    },
    Label {
        variable: String,
        labels: Vec<String>,
    },
}

/// MATCH ... MERGE pattern ON CREATE SET ... ON MATCH SET ... [RETURN ...]
#[derive(Debug, Clone, PartialEq)]
pub struct MatchMergeStatement {
    pub patterns: Vec<Pattern>,
    pub where_clause: Option<Expr>,
    pub merge_pattern: Pattern,
    pub on_create: Vec<SetItem>,
    pub on_match: Vec<SetItem>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// MERGE (n:Label {props}) ON CREATE SET ... ON MATCH SET ... [RETURN ...]
#[derive(Debug, Clone, PartialEq)]
pub struct MergeStatement {
    pub pattern: Pattern,
    pub on_create: Vec<SetItem>,
    pub on_match: Vec<SetItem>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
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
        intermediate_clauses: Vec<IntermediateClause>,
        return_clause: ReturnClause,
        order_by: Vec<SortItem>,
        skip: Option<Expr>,
        limit: Option<Expr>,
    },
    Create {
        patterns: Vec<Pattern>,
        intermediate_clauses: Vec<IntermediateClause>,
        return_clause: Option<ReturnClause>,
        order_by: Vec<SortItem>,
        skip: Option<Expr>,
        limit: Option<Expr>,
    },
}

/// Intermediate clause (WITH, UNWIND, or MATCH) within a MATCH statement.
#[derive(Debug, Clone, PartialEq)]
pub enum IntermediateClause {
    With(WithClause),
    Unwind(UnwindClause),
    Match(IntermediateMatch),
}

/// MATCH/OPTIONAL MATCH clause appearing after a WITH.
#[derive(Debug, Clone, PartialEq)]
pub struct IntermediateMatch {
    pub patterns: Vec<Pattern>,
    pub optional_patterns: Vec<OptionalMatch>,
    pub where_clause: Option<Expr>,
}

/// UNWIND clause within a MATCH statement.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    pub expr: Expr,
    pub alias: String,
}

/// Whether a pattern is wrapped in shortestPath / allShortestPaths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortestPathMode {
    None,
    Single,
    All,
}

/// A graph pattern: sequence of node and relationship elements,
/// with optional path variable binding and shortest path mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub elements: Vec<PatternElement>,
    pub path_variable: Option<String>,
    pub shortest_path_mode: ShortestPathMode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelPattern),
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    pub properties: HashMap<String, Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub variable: Option<String>,
    pub rel_types: Vec<String>,
    pub properties: HashMap<String, Expr>,
    pub direction: RelDirection,
    pub var_length: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDirection {
    Outgoing,   // -[]->)
    Incoming,   // <-[]-
    Undirected, // -[]-
}

/// RETURN clause with optional ORDER BY and LIMIT.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub items: Vec<ReturnItem>,
    pub distinct: bool,
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
    /// Parameter reference: $name
    Parameter(String),
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
        distinct: bool,
        /// Original source text for column naming (preserves whitespace/case).
        original_text: Option<String>,
    },
    /// CASE [operand] WHEN cond THEN result ... ELSE default END
    Case {
        operand: Option<Box<Expr>>,
        alternatives: Vec<(Box<Expr>, Box<Expr>)>,
        default: Option<Box<Expr>>,
    },
    /// List literal: [expr, expr, ...]
    List(Vec<Expr>),
    /// List comprehension: [x IN list WHERE pred | expr]
    ListComprehension {
        variable: String,
        list_expr: Box<Expr>,
        filter: Option<Box<Expr>>,
        map_expr: Option<Box<Expr>>,
    },
    /// Pattern comprehension: [(p = )? pattern (WHERE pred)? | expr]
    PatternComprehension {
        path_variable: Option<String>,
        pattern: Pattern,
        where_clause: Option<Box<Expr>>,
        map_expr: Box<Expr>,
    },
    /// EXISTS { pattern [WHERE expr] } subquery predicate.
    Exists {
        patterns: Vec<Pattern>,
        where_clause: Option<Box<Expr>>,
    },
    /// EXISTS { MATCH ... [WITH ...] RETURN ... } full existential subquery.
    ExistsSubquery(Box<Statement>),
    /// Map literal: {key: expr, key2: expr2, ...}. Keys are preserved in
    /// source order for error messages; evaluation sorts them into a
    /// BTreeMap.
    MapLiteral(Vec<(String, Expr)>),
    /// List index: expr[index]
    Index { expr: Box<Expr>, index: Box<Expr> },
    /// Chained property access: expr.key (for m.a.b patterns)
    DotAccess { expr: Box<Expr>, key: String },
    /// List slice: expr[start..end] (either bound may be None)
    Slice {
        expr: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
    },
    /// Quantifier predicate: none/single/any/all(x IN list WHERE pred)
    Quantifier {
        kind: QuantifierKind,
        variable: String,
        list_expr: Box<Expr>,
        predicate: Box<Expr>,
    },
    /// Label predicate: n:Label (true if node has all specified labels).
    HasLabel(String, Vec<String>),
    /// Pattern predicate: (n)-[:REL]->(m) — true if the pattern matches.
    /// Used in WHERE clauses for existential checks.
    PatternPredicate(Pattern),
    /// Wildcard * (used in count(*) and RETURN *)
    Star,
}

/// Quantifier predicate kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantifierKind {
    None,
    Single,
    Any,
    All,
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
    Xor,
    StartsWith,
    EndsWith,
    Contains,
    In,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}
