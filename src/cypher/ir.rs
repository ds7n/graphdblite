use std::collections::HashMap;

use crate::cypher::ast::{
    Assignment, Expr, LiteralValue, Pattern, RemoveItem, ReturnItem, SortItem,
};
use crate::types::Direction;

/// Logical query plan operator. Language-agnostic IR that the executor consumes.
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalOp {
    /// Emit a single empty row (input for standalone `RETURN` expressions).
    SingleRow,

    /// Scan all nodes with a label.
    Scan { label: String, alias: String },

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
        /// Relationship variable name (e.g. `r` in `[r:TYPE]`).
        rel_alias: Option<String>,
        /// Edge type(s) to match. Empty = any type. Multiple = match any of them.
        edge_types: Vec<String>,
        direction: Direction,
        min_hops: u32,
        max_hops: u32,
        /// True when the pattern uses `[*]` or `[*min..max]` syntax.
        /// Even `[*1..1]` is var-length (rel variable binds to a list).
        var_length: bool,
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

    /// Project (RETURN / WITH) columns from the record stream.
    ///
    /// `emit_compound` is `true` for the terminal RETURN so bare-variable
    /// projections yield `Value::Node` / `Value::Edge` values. Intermediate
    /// projections (WITH clauses) leave `emit_compound = false` so downstream
    /// operators continue to see the flat `var.prop` / `var.__id` shape they
    /// rely on for joins, ORDER BY, further pattern matching, etc.
    Project {
        input: Box<LogicalOp>,
        items: Vec<ReturnItem>,
        emit_compound: bool,
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

    /// Remove duplicate records.
    Distinct { input: Box<LogicalOp> },

    /// Skip the first N records.
    Skip { input: Box<LogicalOp>, count: u64 },

    /// Limit the number of output records.
    Limit { input: Box<LogicalOp>, count: u64 },

    /// Create a node.
    CreateNode {
        labels: Vec<String>,
        alias: Option<String>,
        properties: HashMap<String, Expr>,
    },

    /// Create an edge between two already-bound variables.
    CreateEdge {
        src_alias: String,
        dst_alias: String,
        edge_type: String,
        rel_alias: Option<String>,
        properties: HashMap<String, Expr>,
    },

    /// Sequence of create operations (for multi-pattern CREATE).
    CreateSequence { ops: Vec<LogicalOp> },

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

    /// Add labels to a node.
    SetLabel {
        input: Box<LogicalOp>,
        variable: String,
        labels: Vec<String>,
    },

    /// Set all properties on a node (overwrite or merge).
    SetProperties {
        input: Box<LogicalOp>,
        variable: String,
        value: Expr,
        merge: bool,
    },

    /// Remove properties/labels from nodes/edges.
    Remove {
        input: Box<LogicalOp>,
        items: Vec<RemoveItem>,
    },

    /// Merge: match-or-create pattern.
    Merge {
        pattern: Pattern,
        on_create: Vec<Assignment>,
        on_match: Vec<Assignment>,
    },

    /// MATCH ... MERGE: merge a pattern using bound variables from MATCH pipeline.
    MatchMerge {
        input: Box<LogicalOp>,
        merge_pattern: Pattern,
        on_create: Vec<Assignment>,
        on_match: Vec<Assignment>,
    },

    /// Build a Path value from the matched pattern elements and store it
    /// in the record under the given alias. `node_aliases` and `rel_aliases`
    /// list the variables in traversal order.
    MaterializePath {
        input: Box<LogicalOp>,
        path_alias: String,
        node_aliases: Vec<String>,
        rel_aliases: Vec<String>,
    },

    /// Correlated inner join: for each input record, execute right side with
    /// bindings from the left. Only emit combined rows. If right produces
    /// nothing for a left row, that row is dropped.
    CorrelatedJoin {
        input: Box<LogicalOp>,
        right: Box<LogicalOp>,
    },

    /// Left outer join: for each input record, attempt right side; emit NULLs if no match.
    LeftOuterJoin {
        input: Box<LogicalOp>,
        right: Box<LogicalOp>,
        /// Aliases from the optional pattern that should be NULL-filled on no match.
        optional_aliases: Vec<String>,
        /// Optional WHERE clause applied after joining but before null-filling.
        /// If the predicate fails on a joined row, that row is null-filled instead.
        opt_filter: Option<Expr>,
    },

    /// Unwind a list expression into one record per element.
    Unwind {
        input: Box<LogicalOp>,
        expr: Expr,
        alias: String,
    },

    /// Find shortest path(s) between two bound nodes.
    ShortestPath {
        input: Box<LogicalOp>,
        src_alias: String,
        dst_alias: String,
        path_alias: String,
        edge_type: Option<String>,
        direction: Direction,
        max_hops: u32,
        all_paths: bool,
    },

    /// Union of multiple pipelines. `all` = keep duplicates.
    Union { inputs: Vec<LogicalOp>, all: bool },

    /// Produce a single empty record (used as starting input for scans).
    EmptyRow,
}

/// An aggregate expression within an Aggregate operator.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateExpr {
    pub function: AggregateFunction,
    pub input: Expr,
    pub alias: Option<String>,
    pub distinct: bool,
    /// Second argument for percentile functions (the percentile value).
    pub extra_arg: Option<Expr>,
    /// Original function name as written in the query (preserves case).
    pub original_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    PercentileDisc,
    PercentileCont,
    StDev,
    StDevP,
}
