use std::collections::HashMap;

use crate::cypher::ast::{Assignment, Expr, LiteralValue, Pattern, ReturnItem, SortItem};
use crate::types::Direction;

/// Logical query plan operator. Language-agnostic IR that the executor consumes.
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalOp {
    /// Scan all nodes with a label.
    Scan {
        label: String,
        alias: String,
    },

    /// Index-based lookup: use a secondary index instead of a full label scan.
    IndexLookup {
        label: String,
        alias: String,
        property: String,
        value: LiteralValue,
        /// Remaining inline property filters not covered by the index.
        remaining_filters: Option<Expr>,
    },

    /// Expand along edges from a source node.
    Expand {
        input: Box<LogicalOp>,
        src_alias: String,
        dst_alias: String,
        edge_type: Option<String>,
        direction: Direction,
        min_hops: u32,
        max_hops: u32,
    },

    /// Cross-product of two pipelines (for multi-pattern MATCH).
    CrossProduct {
        left: Box<LogicalOp>,
        right: Box<LogicalOp>,
    },

    /// Filter records by a predicate.
    Filter {
        input: Box<LogicalOp>,
        predicate: Expr,
    },

    /// Project (RETURN) columns from the record stream.
    Project {
        input: Box<LogicalOp>,
        items: Vec<ReturnItem>,
    },

    /// Aggregate records.
    Aggregate {
        input: Box<LogicalOp>,
        group_keys: Vec<Expr>,
        aggregates: Vec<AggregateExpr>,
    },

    /// Sort records.
    Sort {
        input: Box<LogicalOp>,
        items: Vec<SortItem>,
    },

    /// Limit the number of output records.
    Limit {
        input: Box<LogicalOp>,
        count: u64,
    },

    /// Create a node.
    CreateNode {
        label: Option<String>,
        alias: Option<String>,
        properties: HashMap<String, Expr>,
    },

    /// Create an edge between two already-bound variables.
    CreateEdge {
        src_alias: String,
        dst_alias: String,
        edge_type: String,
        properties: HashMap<String, Expr>,
    },

    /// Sequence of create operations (for multi-pattern CREATE).
    CreateSequence {
        ops: Vec<LogicalOp>,
    },

    /// MATCH ... CREATE: run input pipeline, then create edges/nodes using bound variables.
    MatchCreate {
        input: Box<LogicalOp>,
        create_ops: Vec<LogicalOp>,
    },

    /// Delete nodes/edges bound to variables.
    Delete {
        input: Box<LogicalOp>,
        variables: Vec<String>,
        detach: bool,
    },

    /// Set properties on nodes/edges.
    SetProperty {
        input: Box<LogicalOp>,
        assignments: Vec<Assignment>,
    },

    /// Merge: match-or-create pattern.
    Merge {
        pattern: Pattern,
        on_create: Vec<Assignment>,
        on_match: Vec<Assignment>,
    },

    /// Left outer join: for each input record, attempt right side; emit NULLs if no match.
    LeftOuterJoin {
        input: Box<LogicalOp>,
        right: Box<LogicalOp>,
        /// Aliases from the optional pattern that should be NULL-filled on no match.
        optional_aliases: Vec<String>,
    },

    /// Produce a single empty record (used as starting input for scans).
    EmptyRow,
}

/// An aggregate expression within an Aggregate operator.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateExpr {
    pub function: AggregateFunction,
    pub input: Expr,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
}
