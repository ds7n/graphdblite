//! Write operators — Create, Delete, Set, Remove.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::eval_expr;
use crate::cypher::record::NamedRecord;
use crate::types::*;
use crate::{edge, fts, index, node};

use super::util::{bind_edge_fields, populate_node_bindings};
use super::*;

pub(in crate::cypher::executor) fn exec_create_node(
    conn: &Connection,
    labels: &[String],
    alias: Option<&str>,
    properties: &HashMap<String, Expr>,
) -> Result<Vec<NamedRecord>> {
    let mut rec = NamedRecord::new();
    create_node_into(conn, &mut rec, labels, alias, properties)?;
    Ok(vec![rec])
}

pub(in crate::cypher::executor) fn exec_create_edge(
    _conn: &Connection,
    _src_alias: &str,
    _dst_alias: &str,
    _edge_type: &str,
    _properties: &HashMap<String, Expr>,
) -> Result<Vec<NamedRecord>> {
    // Standalone edge creation is handled by exec_create_sequence.
    // This path is only reached for isolated CreateEdge ops (shouldn't happen in practice).
    Ok(vec![])
}

pub(in crate::cypher::executor) fn exec_create_sequence(
    conn: &Connection,
    ops: &[LogicalOp],
) -> Result<Vec<NamedRecord>> {
    let mut rec = NamedRecord::new();
    run_create_ops(conn, &mut rec, ops)?;
    Ok(vec![rec])
}

pub(in crate::cypher::executor) fn exec_match_create(
    conn: &Connection,
    input: &LogicalOp,
    create_ops: &[LogicalOp],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        run_create_ops(conn, rec, create_ops)?;
    }
    Ok(records)
}

/// Run the `CreateNode` / `CreateEdge` ops of one CREATE clause against the
/// row `rec`, binding every created entity into it. Property expressions see
/// the row as built so far, so later elements can read earlier ones.
fn run_create_ops(conn: &Connection, rec: &mut NamedRecord, ops: &[LogicalOp]) -> Result<()> {
    for op in ops {
        match op {
            LogicalOp::CreateNode {
                labels,
                alias,
                properties,
            } => {
                // A variable bound by an earlier clause names an existing
                // node; the pattern only refers to it. Anonymous nodes are
                // always new (the planner numbers them per pattern, so two
                // patterns of one CREATE can share an `__anon_N` alias).
                if let Some(a) = alias {
                    if !a.starts_with("__anon_") && rec.get(a).is_some() {
                        continue;
                    }
                }
                create_node_into(conn, rec, labels, alias.as_deref(), properties)?;
            }
            LogicalOp::CreateEdge {
                src_alias,
                dst_alias,
                edge_type,
                rel_alias,
                properties,
            } => {
                let src = bound_node_id(conn, rec, src_alias, "CREATE")?;
                let dst = bound_node_id(conn, rec, dst_alias, "CREATE")?;
                let props = eval_create_properties(conn, rec, properties)?;
                validate_write_labels(std::slice::from_ref(edge_type))?;
                validate_write_properties(&props)?;
                let seq = edge::create_edge(conn, src, dst, edge_type, props.clone())?;
                if let Some(r_alias) = rel_alias {
                    rec.set(r_alias.clone(), Value::String(edge_type.clone()));
                    bind_edge_fields(rec, r_alias, src, dst, edge_type, seq, &props);
                }
            }
            _ => {
                // Shouldn't happen in a CREATE clause.
            }
        }
    }
    Ok(())
}

/// Evaluate a CREATE pattern's property map against `rec`; null entries are
/// dropped (creating a property with a null value is a no-op).
fn eval_create_properties(
    conn: &Connection,
    rec: &NamedRecord,
    properties: &HashMap<String, Expr>,
) -> Result<Properties> {
    let mut props = Properties::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
        if val != Value::Null {
            props.insert(key.clone(), val);
        }
    }
    Ok(props)
}

/// Create a node from a CREATE pattern element and bind it under `alias`
/// as a full node binding (id, labels and properties).
fn create_node_into(
    conn: &Connection,
    rec: &mut NamedRecord,
    labels: &[String],
    alias: Option<&str>,
    properties: &HashMap<String, Expr>,
) -> Result<NodeId> {
    let props = eval_create_properties(conn, rec, properties)?;
    let node = create_indexed_node(conn, labels, props)?;
    if let Some(alias) = alias {
        populate_node_bindings(rec, &node, alias);
    }
    Ok(node.id)
}

/// Create a node and its secondary-index / FTS entries; returns the node as
/// stored (labels normalized).
pub(in crate::cypher::executor) fn create_indexed_node(
    conn: &Connection,
    labels: &[String],
    props: Properties,
) -> Result<Node> {
    validate_write_labels(labels)?;
    validate_write_properties(&props)?;
    let id = node::create_node(conn, labels, props.clone())?;
    let labels = node::normalize_labels(labels);
    index::update_indexes_for_node(conn, id, &labels, None, &props)?;
    fts::update_fts_for_node(conn, id, &labels, None, &props)?;
    Ok(Node {
        id,
        labels,
        properties: props,
    })
}

/// The live node `var` refers to in `rec`, for use as a relationship
/// endpoint by `what` (CREATE / MERGE). The binding must be a node (a flat
/// node binding or a node value) that still exists; a plain integer is not a
/// node reference.
pub(in crate::cypher::executor) fn bound_node_id(
    conn: &Connection,
    rec: &NamedRecord,
    var: &str,
    what: &str,
) -> Result<NodeId> {
    let id = match rec.get(var) {
        Some(Value::I64(id)) if is_node_binding(rec, var) => NodeId(*id as u64),
        Some(Value::Node(n)) => n.id,
        None => return Err(GraphError::semantic(format!("unbound variable: {var}"))),
        Some(Value::Null) => {
            return Err(GraphError::query(
                QueryPhase::Runtime,
                ErrorCode::Other,
                format!("{what} cannot use `{var}` as a relationship endpoint: it is null"),
            ))
        }
        Some(other) => {
            return Err(GraphError::type_error(
                QueryPhase::Runtime,
                format!(
                    "{what} expected a node for `{var}`, got {}",
                    value_type_name(other)
                ),
            )
            .with_code(ErrorCode::InvalidArgumentType))
        }
    };
    if !node::node_exists(conn, id)? {
        return Err(GraphError::NodeNotFound { id, hint: None });
    }
    Ok(id)
}

pub(in crate::cypher::executor) fn exec_delete(
    conn: &Connection,
    input: &LogicalOp,
    exprs: &[Expr],
    detach: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;

    // Two-phase delete: collect all entities first, then delete edges, then nodes.
    // This prevents DeleteConnectedNode errors when multiple paths share nodes.
    let mut edges_to_delete: Vec<(NodeId, NodeId, String, Option<u64>)> = Vec::new();
    let mut nodes_to_delete: Vec<NodeId> = Vec::new();

    for rec in &mut records {
        for expr in exprs {
            if let ExprKind::Variable(var) = &expr.kind {
                collect_var_entities(rec, var, &mut edges_to_delete, &mut nodes_to_delete)?;
                rec.set(format!("{var}.__deleted"), Value::Bool(true));
            } else {
                let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                collect_value_entities(&val, &mut edges_to_delete, &mut nodes_to_delete)?;
            }
        }
    }

    // Phase 1: delete all edges. Both storage calls are no-ops for an edge
    // that is already gone (the same edge bound in several rows).
    for (src, dst, label, seq) in &edges_to_delete {
        if let Some(s) = seq {
            edge::delete_single_edge(conn, *src, *dst, label, *s)?;
        } else {
            edge::delete_edge(conn, *src, *dst, label)?;
        }
    }

    // Phase 2: delete all nodes (edges already removed).
    for node_id in &nodes_to_delete {
        if !detach && node::node_has_edges(conn, *node_id)? {
            return Err(GraphError::constraint(format!(
                "cannot delete node {} because it still has relationships. Use DETACH DELETE.",
                node_id
            ))
            .with_code(ErrorCode::DeleteConnectedNode));
        }
        // A node listed twice (bound in several rows) is already gone on
        // its second visit, along with its index entries: skip it.
        let n = match node::get_node(conn, *node_id) {
            Ok(n) => n,
            Err(GraphError::NodeNotFound { .. }) => continue,
            Err(e) => return Err(e),
        };
        // Clean up secondary indexes (btree + FTS) before removing the node
        // itself. Mirrors WriteTransaction::delete_node in transaction.rs.
        index::remove_indexes_for_node(conn, *node_id, &n.labels, &n.properties)?;
        fts::remove_fts_for_node(conn, *node_id, &n.labels, &n.properties)?;
        node::delete_node(conn, *node_id)?;
    }

    Ok(records)
}

/// Collect entities from a variable binding for two-phase delete.
///
/// Node bindings are stored as `Value::I64(id)` plus an `alias.__id` marker;
/// a bare integer without the marker is an ordinary integer value (e.g. from
/// `WITH n.k AS x`) and must not be treated as a node reference.
pub(in crate::cypher::executor) fn collect_var_entities(
    rec: &NamedRecord,
    var: &str,
    edges: &mut Vec<(NodeId, NodeId, String, Option<u64>)>,
    nodes: &mut Vec<NodeId>,
) -> Result<()> {
    let edge_src_key = format!("{var}.__src");
    let edge_dst_key = format!("{var}.__dst");
    let edge_type_key = format!("{var}.__type");
    if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
        rec.get(&edge_src_key),
        rec.get(&edge_dst_key),
        rec.get(&edge_type_key),
    ) {
        let edge_seq_key = format!("{var}.__edge_seq");
        let seq = if let Some(Value::I64(s)) = rec.get(&edge_seq_key) {
            Some(*s as u64)
        } else {
            None
        };
        edges.push((NodeId(*src as u64), NodeId(*dst as u64), label.clone(), seq));
        return Ok(());
    }
    match rec.get(var) {
        Some(Value::I64(id)) if is_node_binding(rec, var) => {
            nodes.push(NodeId(*id as u64));
            Ok(())
        }
        Some(val) => {
            let val = val.clone();
            collect_value_entities(&val, edges, nodes)
        }
        None => Ok(()),
    }
}

/// Whether `var` in `rec` is a node binding (flat `I64` id + `__id` marker).
pub(in crate::cypher::executor) fn is_node_binding(rec: &NamedRecord, var: &str) -> bool {
    matches!(rec.get(var), Some(Value::I64(_)))
        && matches!(rec.get(&format!("{var}.__id")), Some(Value::I64(_)))
}

fn not_an_entity(what: &str, val: &Value) -> GraphError {
    GraphError::type_error(
        crate::types::QueryPhase::Runtime,
        format!(
            "{what} requires a node, relationship, or path, got {}",
            value_type_name(val)
        ),
    )
    .with_code(ErrorCode::InvalidArgumentType)
}

/// Collect entities from a Value for two-phase delete. Nulls are skipped;
/// any other non-entity value is a type error.
pub(in crate::cypher::executor) fn collect_value_entities(
    val: &Value,
    edges: &mut Vec<(NodeId, NodeId, String, Option<u64>)>,
    nodes: &mut Vec<NodeId>,
) -> Result<()> {
    match val {
        Value::Null => {}
        Value::Node(n) => {
            nodes.push(n.id);
        }
        // Relationship values carry their parallel-edge `seq`, so exactly
        // that edge is deleted (not every edge sharing its endpoints/type).
        Value::Edge(e) => {
            edges.push((e.src, e.dst, e.label.clone(), Some(e.seq)));
        }
        Value::Path(p) => {
            for e in &p.edges {
                edges.push((e.src, e.dst, e.label.clone(), Some(e.seq)));
            }
            for n in &p.nodes {
                nodes.push(n.id);
            }
        }
        Value::List(items) => {
            for item in items {
                collect_value_entities(item, edges, nodes)?;
            }
        }
        other => return Err(not_an_entity("DELETE", other)),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SET / REMOVE
//
// Every SET / REMOVE form (and MERGE's ON CREATE / ON MATCH items) goes
// through the per-row `apply_*` functions below. Each one resolves the target
// variable to a stored entity, re-reads it (so a row sees the writes of
// earlier rows to the same entity), applies the change with index / FTS
// maintenance, and rebinds the variable in the row to the new state.

/// The stored entity a SET / REMOVE target variable refers to.
enum WriteTarget {
    Node(NodeId),
    Edge {
        src: NodeId,
        dst: NodeId,
        label: String,
        seq: u64,
    },
}

/// Resolve `var` in `rec` to the entity it names. `Ok(None)` for a null or
/// absent binding (an unmatched OPTIONAL MATCH: the write is skipped) and for
/// paths; any other non-entity value is a type error.
fn resolve_write_target(
    conn: &Connection,
    rec: &NamedRecord,
    var: &str,
    what: &str,
) -> Result<Option<WriteTarget>> {
    if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
        rec.get(&format!("{var}.__src")),
        rec.get(&format!("{var}.__dst")),
        rec.get(&format!("{var}.__type")),
    ) {
        let (src, dst) = (NodeId(*src as u64), NodeId(*dst as u64));
        let seq = match rec.get(&format!("{var}.__edge_seq")) {
            Some(Value::I64(s)) => *s as u64,
            // No seq recorded: the first parallel edge of the triple.
            _ => {
                let prefix = edge::edge_props_prefix(src, dst, label);
                let entries = crate::storage::kv::scan_prefix(
                    conn,
                    crate::storage::kv::TABLE_EDGE_PROPS,
                    &prefix,
                )?;
                entries
                    .first()
                    .map(|(k, _)| edge::edge_seq_from_key(k, prefix.len()))
                    .unwrap_or(0)
            }
        };
        return Ok(Some(WriteTarget::Edge {
            src,
            dst,
            label: label.clone(),
            seq,
        }));
    }
    match rec.get(var) {
        None | Some(Value::Null) | Some(Value::Path(_)) => Ok(None),
        Some(Value::I64(id)) if is_node_binding(rec, var) => {
            Ok(Some(WriteTarget::Node(NodeId(*id as u64))))
        }
        Some(Value::Node(n)) => Ok(Some(WriteTarget::Node(n.id))),
        Some(Value::Edge(e)) => Ok(Some(WriteTarget::Edge {
            src: e.src,
            dst: e.dst,
            label: e.label.clone(),
            seq: e.seq,
        })),
        Some(other) => Err(not_an_entity(what, other)),
    }
}

/// A stored entity as currently persisted.
enum Entity {
    Node(Node),
    Edge(Edge),
}

/// Read the current state of `target`. A deleted node is `NodeNotFound`, a
/// deleted relationship `EdgeNotFound`.
fn load_target(conn: &Connection, target: &WriteTarget) -> Result<Entity> {
    match target {
        WriteTarget::Node(id) => Ok(Entity::Node(node::get_node(conn, *id)?)),
        WriteTarget::Edge {
            src,
            dst,
            label,
            seq,
        } => {
            if !edge::edge_exists_at(conn, *src, *dst, label, *seq)? {
                return Err(GraphError::EdgeNotFound {
                    src: *src,
                    label: label.clone(),
                    dst: *dst,
                    hint: None,
                });
            }
            let properties = edge::get_edge_properties_at(conn, *src, *dst, label, *seq)?;
            Ok(Entity::Edge(Edge {
                src: *src,
                dst: *dst,
                label: label.clone(),
                seq: *seq,
                properties,
            }))
        }
    }
}

/// Overwrite `var`'s binding in `rec` with `entity`. A node / relationship
/// held as a value is replaced; a flat binding gets its property keys
/// rebuilt (stale keys removed) and, for nodes, its label keys.
fn rebind(rec: &mut NamedRecord, var: &str, entity: &Entity) {
    match (rec.fields.get_mut(var), entity) {
        (Some(Value::Node(n)), Entity::Node(new)) => {
            *n = new.clone();
            return;
        }
        (Some(Value::Edge(e)), Entity::Edge(new)) => {
            *e = new.clone();
            return;
        }
        _ => {}
    }
    let prefix = format!("{var}.");
    rec.fields.retain(|k, _| match k.strip_prefix(&prefix) {
        Some(rest) => rest.starts_with("__"),
        None => true,
    });
    match entity {
        Entity::Node(n) => populate_node_bindings(rec, n, var),
        Entity::Edge(e) => populate_edge_bindings(rec, e, var),
    }
}

/// Persist `new_props` as the full property set of node `old`, keeping the
/// secondary and FTS indexes in sync. Returns the node's new state.
fn write_node_props(conn: &Connection, old: &Node, new_props: Properties) -> Result<Node> {
    node::set_all_node_properties(conn, old.id, new_props.clone())?;
    index::update_indexes_for_node(conn, old.id, &old.labels, Some(&old.properties), &new_props)?;
    fts::update_fts_for_node(conn, old.id, &old.labels, Some(&old.properties), &new_props)?;
    Ok(Node {
        id: old.id,
        labels: old.labels.clone(),
        properties: new_props,
    })
}

/// Persist `new_props` as the full property set of edge `old`.
fn write_edge_props(conn: &Connection, old: &Edge, new_props: Properties) -> Result<Edge> {
    edge::set_all_edge_properties_at(
        conn,
        old.src,
        old.dst,
        &old.label,
        old.seq,
        new_props.clone(),
    )?;
    Ok(Edge {
        properties: new_props,
        ..old.clone()
    })
}

/// Write `new_props` to `entity` and rebind `var` in `rec`.
fn write_props(
    conn: &Connection,
    rec: &mut NamedRecord,
    var: &str,
    entity: &Entity,
    new_props: Properties,
) -> Result<()> {
    let new = match entity {
        Entity::Node(n) => Entity::Node(write_node_props(conn, n, new_props)?),
        Entity::Edge(e) => Entity::Edge(write_edge_props(conn, e, new_props)?),
    };
    rebind(rec, var, &new);
    Ok(())
}

fn entity_props(entity: &Entity) -> &Properties {
    match entity {
        Entity::Node(n) => &n.properties,
        Entity::Edge(e) => &e.properties,
    }
}

/// Resolve `var`, re-read the entity and refresh its binding in `rec` so
/// expressions evaluated next see the stored state. `Ok(None)` when the
/// write must be skipped (null binding).
fn load_and_refresh(
    conn: &Connection,
    rec: &mut NamedRecord,
    var: &str,
    what: &str,
) -> Result<Option<Entity>> {
    let Some(target) = resolve_write_target(conn, rec, var, what)? else {
        return Ok(None);
    };
    let entity = load_target(conn, &target)?;
    rebind(rec, var, &entity);
    Ok(Some(entity))
}

/// `SET var.prop = expr` on one row.
pub(in crate::cypher::executor) fn apply_set_property(
    conn: &Connection,
    rec: &mut NamedRecord,
    assignment: &Assignment,
) -> Result<()> {
    let var = &assignment.variable;
    let Some(entity) = load_and_refresh(conn, rec, var, "SET")? else {
        return Ok(());
    };
    let val = eval_expr(
        &assignment.value,
        rec,
        crate::cypher::eval::EvalCx::new(conn),
    )?;
    validate_write_value(&assignment.property, &val)?;
    if matches!(entity, Entity::Node(_)) {
        crate::types::validate_name(&assignment.property)?;
    }
    let mut new_props = entity_props(&entity).clone();
    set_or_remove(&mut new_props, &assignment.property, val);
    write_props(conn, rec, var, &entity, new_props)
}

/// The property map a `SET x = value` / `SET x += value` copies from: a map
/// (null entries kept, they mean "remove"), or the properties of a node or
/// relationship. `Ok(None)` for null (nothing to set); anything else is a
/// type error.
fn props_source(val: Value) -> Result<Option<Properties>> {
    match val {
        Value::Map(m) => Ok(Some(m.into_iter().collect())),
        Value::Node(n) => Ok(Some(n.properties)),
        Value::Edge(e) => Ok(Some(e.properties)),
        Value::Null => Ok(None),
        other => Err(GraphError::type_error(
            QueryPhase::Runtime,
            format!(
                "SET requires a map, node or relationship value, got {}",
                value_type_name(&other)
            ),
        )
        .with_code(ErrorCode::InvalidArgumentType)),
    }
}

/// `SET var = value` (`merge == false`) or `SET var += value` (`merge ==
/// true`) on one row.
pub(in crate::cypher::executor) fn apply_set_properties(
    conn: &Connection,
    rec: &mut NamedRecord,
    variable: &str,
    value_expr: &Expr,
    merge: bool,
) -> Result<()> {
    let Some(entity) = load_and_refresh(conn, rec, variable, "SET")? else {
        return Ok(());
    };
    let val = eval_expr(value_expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
    let Some(map) = props_source(val)? else {
        return Ok(());
    };
    // Merge: start from the stored properties; overwrite: from empty.
    let mut new_props = if merge {
        entity_props(&entity).clone()
    } else {
        Properties::new()
    };
    for (k, v) in map {
        if v == Value::Null {
            new_props.remove(&k);
        } else {
            validate_write_value(&k, &v)?;
            crate::types::validate_name(&k)?;
            new_props.insert(k, v);
        }
    }
    write_props(conn, rec, variable, &entity, new_props)
}

/// `SET var:Label...` on one row.
pub(in crate::cypher::executor) fn apply_set_labels(
    conn: &Connection,
    rec: &mut NamedRecord,
    variable: &str,
    labels: &[String],
) -> Result<()> {
    let Some(Entity::Node(n)) = load_and_refresh(conn, rec, variable, "SET")? else {
        return Ok(());
    };
    validate_write_labels(labels)?;
    for label in labels {
        node::add_node_label(conn, n.id, label)?;
    }
    // Adding labels can bring new per-label indexes into scope; insert
    // entries across the full (post-add) label set. Idempotent for labels
    // the node already had.
    let new = node::get_node(conn, n.id)?;
    index::update_indexes_for_node(conn, n.id, &new.labels, None, &new.properties)?;
    fts::update_fts_for_node(conn, n.id, &new.labels, None, &new.properties)?;
    rebind(rec, variable, &Entity::Node(new));
    Ok(())
}

/// One `REMOVE` item on one row.
pub(in crate::cypher::executor) fn apply_remove_item(
    conn: &Connection,
    rec: &mut NamedRecord,
    item: &RemoveItem,
) -> Result<()> {
    match item {
        RemoveItem::Property { variable, property } => {
            let Some(entity) = load_and_refresh(conn, rec, variable, "REMOVE")? else {
                return Ok(());
            };
            let mut new_props = entity_props(&entity).clone();
            new_props.remove(property);
            write_props(conn, rec, variable, &entity, new_props)
        }
        RemoveItem::Label { variable, labels } => {
            let Some(Entity::Node(old)) = load_and_refresh(conn, rec, variable, "REMOVE")? else {
                return Ok(());
            };
            // A removed label's per-label index entries must be dropped;
            // entries owned by the node's remaining labels stay valid.
            for label in labels {
                index::remove_indexes_for_label(conn, old.id, label, &old.properties)?;
                fts::remove_fts_for_label(conn, old.id, label, &old.properties)?;
                node::remove_node_label(conn, old.id, label)?;
            }
            let new = node::get_node(conn, old.id)?;
            rebind(rec, variable, &Entity::Node(new));
            Ok(())
        }
    }
}

/// Apply one SET item (from a SET clause or a MERGE ON CREATE / ON MATCH)
/// to whichever variable it names in `rec`.
pub(in crate::cypher::executor) fn apply_set_item(
    conn: &Connection,
    rec: &mut NamedRecord,
    item: &SetItem,
) -> Result<()> {
    match item {
        SetItem::Property(assignment) => apply_set_property(conn, rec, assignment),
        SetItem::Label { variable, labels } => apply_set_labels(conn, rec, variable, labels),
        SetItem::MapOverwrite { variable, value } => {
            apply_set_properties(conn, rec, variable, value, false)
        }
        SetItem::MapMerge { variable, value } => {
            apply_set_properties(conn, rec, variable, value, true)
        }
    }
}

pub(in crate::cypher::executor) fn exec_set_property(
    conn: &Connection,
    input: &LogicalOp,
    assignments: &[crate::cypher::ast::Assignment],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        for assignment in assignments {
            apply_set_property(conn, rec, assignment)?;
        }
    }
    Ok(records)
}

/// Apply `SET .prop = val` to an in-record property map (null removes).
fn set_or_remove(props: &mut Properties, key: &str, val: Value) {
    if val == Value::Null {
        props.remove(key);
    } else {
        props.insert(key.to_string(), val);
    }
}

thread_local! {
    /// `(max_property_value_bytes, max_name_bytes)` for the query currently
    /// executing on this thread; 0 = unlimited. Installed by
    /// [`WriteLimitsScope`] from the `ExecContext` in `execute_with_ctx`.
    static WRITE_LIMITS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// RAII guard that installs the write limits for one query execution and
/// restores the previous limits (supporting nested executions) on drop.
pub(crate) struct WriteLimitsScope {
    prev: (usize, usize),
}

impl WriteLimitsScope {
    pub(crate) fn enter(max_property_value_bytes: usize, max_name_bytes: usize) -> Self {
        let prev = WRITE_LIMITS.with(|c| c.replace((max_property_value_bytes, max_name_bytes)));
        Self { prev }
    }
}

impl Drop for WriteLimitsScope {
    fn drop(&mut self) {
        WRITE_LIMITS.with(|c| c.set(self.prev));
    }
}

fn current_write_limits() -> (usize, usize) {
    let (v, n) = WRITE_LIMITS.with(|c| c.get());
    (
        if v == 0 { usize::MAX } else { v },
        if n == 0 { usize::MAX } else { n },
    )
}

/// Validate one property assignment written by Cypher: storable type
/// (no maps / entities, recursively in lists), key length, and value size.
pub(in crate::cypher::executor) fn validate_write_value(key: &str, val: &Value) -> Result<()> {
    let (max_value, max_name) = current_write_limits();
    let one = Properties::from([(key.to_string(), val.clone())]);
    crate::types::validate_properties("", &one, max_name, max_value)
}

/// [`validate_write_value`] for every entry of a property map.
pub(in crate::cypher::executor) fn validate_write_properties(props: &Properties) -> Result<()> {
    for (k, v) in props {
        validate_write_value(k, v)?;
    }
    Ok(())
}

/// Validate label / relationship-type names written by Cypher against the
/// configured name-length limit.
pub(in crate::cypher::executor) fn validate_write_labels(labels: &[String]) -> Result<()> {
    let (_, max_name) = current_write_limits();
    for l in labels {
        crate::types::validate_name_length(l, max_name)?;
    }
    Ok(())
}

pub(in crate::cypher::executor) fn exec_set_label(
    conn: &Connection,
    input: &LogicalOp,
    variable: &str,
    labels: &[String],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        apply_set_labels(conn, rec, variable, labels)?;
    }
    Ok(records)
}

pub(in crate::cypher::executor) fn exec_set_properties(
    conn: &Connection,
    input: &LogicalOp,
    variable: &str,
    value_expr: &crate::cypher::ast::Expr,
    merge: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        apply_set_properties(conn, rec, variable, value_expr, merge)?;
    }
    Ok(records)
}

pub(in crate::cypher::executor) fn exec_remove(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::RemoveItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        for item in items {
            apply_remove_item(conn, rec, item)?;
        }
    }
    Ok(records)
}

/// Execute `CREATE INDEX ON :Label(prop1, prop2, ...)`.
///
/// Delegates to `index::create_composite_index` (which handles both single-
/// property and composite cases via the same table schema).
pub(in crate::cypher::executor) fn exec_create_index(
    conn: &Connection,
    label: &str,
    properties: &[String],
) -> Result<Vec<NamedRecord>> {
    let props: Vec<&str> = properties.iter().map(String::as_str).collect();
    index::create_composite_index(conn, label, &props)?;
    Ok(vec![])
}

/// Execute `DROP INDEX ON :Label(prop1, prop2, ...)`.
pub(in crate::cypher::executor) fn exec_drop_index(
    conn: &Connection,
    label: &str,
    properties: &[String],
) -> Result<Vec<NamedRecord>> {
    let props: Vec<&str> = properties.iter().map(String::as_str).collect();
    index::drop_composite_index(conn, label, &props)?;
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use crate::{Database, GraphError};

    /// A storage failure while deleting must fail the statement, not be
    /// swallowed (the statement rolls back and the entity survives).
    #[test]
    fn delete_propagates_storage_errors() {
        for (trigger_table, query) in [
            ("edge_props", "MATCH ()-[r:R]->() DELETE r"),
            ("nodes", "MATCH (n:B) DETACH DELETE n"),
        ] {
            let mut db = Database::open_memory().unwrap();
            db.execute("CREATE (:A)-[:R]->(:B)").unwrap();
            db.connection()
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER fail_delete BEFORE DELETE ON main.{trigger_table} \
                     BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
                ))
                .unwrap();
            match db.execute(query) {
                Err(GraphError::Storage { source, .. }) => {
                    assert!(source.to_string().contains("injected failure"), "{source}")
                }
                other => panic!("expected a storage error from {query}, got {other:?}"),
            }
            db.connection()
                .execute_batch("DROP TRIGGER fail_delete")
                .unwrap();
            let rows = db
                .execute("MATCH (:A)-[r:R]->(:B) RETURN count(r)")
                .unwrap();
            assert_eq!(rows[0].values().next(), Some(&crate::Value::I64(1)));
        }
    }
}
