//! Bounded per-`Database` cache for planned Cypher queries.
//!
//! Builds on the parameter-agnostic planner from Phase 2 of
//! `plans/plan-cache.md`: index decisions emit `LookupKey::Param` instead of
//! baking literals, so a single plan is reusable across `$param` values for
//! the same query template.
//!
//! ## What's cached
//!
//! [`LogicalOp`] keyed on `(cypher, schema_epoch)`. The epoch is bumped on
//! `CREATE INDEX` / `DROP INDEX`; old entries naturally fall out of reach the
//! next time their cypher key is looked up under the new epoch. FIFO eviction
//! reclaims the slots over time.
//!
//! ## Caching is skipped when the plan would not be reusable
//!
//! [`is_plan_cacheable`] rejects:
//! - SKIP / LIMIT containing `$param` — the planner evaluates these at plan
//!   time via `eval_skip_limit`, so the cached `count: u64` would be wrong
//!   for a different `$n` value.
//! - CALL statements/clauses — `plan_call` reads the procedure registry at
//!   plan time, and the registry is supplied per-execute via `ExecContext`.
//!
//! Everything else (including queries with `$param` referenced in WHERE
//! filters, properties, projections, etc.) is fully cacheable thanks to
//! Phase 2's `LookupKey::Param` plumbing.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::cypher::ast::{Clause, Expr, ExprKind, IntermediateClause, Statement, UnwindBody};
use crate::cypher::ir::LogicalOp;
use crate::types::Result;

const DEFAULT_CAPACITY: usize = 128;

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct Key {
    cypher: String,
    epoch: u64,
}

pub(crate) struct PlanCache {
    inner: Mutex<Inner>,
}

struct Inner {
    map: HashMap<Key, LogicalOp>,
    order: VecDeque<Key>,
    capacity: usize,
}

impl PlanCache {
    pub(crate) fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::with_capacity(capacity),
                order: VecDeque::with_capacity(capacity),
                capacity,
            }),
        }
    }

    /// Resolve `cypher` at the given `epoch` to a planned [`LogicalOp`],
    /// reusing a cached plan when available. On a miss, calls `build` to
    /// produce the plan and inserts it. Errors from `build` are not cached.
    pub(crate) fn get_or_plan<F>(&self, cypher: &str, epoch: u64, build: F) -> Result<LogicalOp>
    where
        F: FnOnce() -> Result<LogicalOp>,
    {
        let key = Key {
            cypher: cypher.to_string(),
            epoch,
        };

        // Fast path: hit. Clone the cached plan under the lock; the planner
        // is never invoked.
        if let Ok(guard) = self.inner.lock() {
            if let Some(plan) = guard.map.get(&key) {
                return Ok(plan.clone());
            }
        }

        // Miss: plan outside the lock so concurrent misses on different
        // queries don't block on each other. Two concurrent misses on the
        // same key both plan; the later insert is a no-op overwrite.
        let plan = build()?;

        if let Ok(mut guard) = self.inner.lock() {
            if !guard.map.contains_key(&key) {
                if guard.map.len() >= guard.capacity {
                    if let Some(oldest) = guard.order.pop_front() {
                        guard.map.remove(&oldest);
                    }
                }
                guard.order.push_back(key.clone());
                guard.map.insert(key, plan.clone());
            }
        }
        Ok(plan)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().map(|g| g.map.len()).unwrap_or(0)
    }
}

impl Default for PlanCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Cacheability check.
// ---------------------------------------------------------------------------

/// Whether `stmt`'s plan can be reused across executions.
///
/// Returns false when the planner evaluates a value at plan time that depends
/// on either the parameter map (SKIP/LIMIT containing `$param`) or the
/// procedure registry (any CALL).
pub(crate) fn is_plan_cacheable(stmt: &Statement) -> bool {
    match stmt {
        Statement::Call { .. } => false,
        Statement::Explain(inner) => is_plan_cacheable(inner),
        Statement::Union { statements, .. } => statements.iter().all(is_plan_cacheable),
        Statement::Match(m) => {
            !skip_limit_has_param(&m.skip, &m.limit)
                && !intermediates_block_caching(&m.intermediate_clauses)
        }
        Statement::Create(c) => !skip_limit_has_param(&c.skip, &c.limit),
        Statement::MatchCreate(mc) => !skip_limit_has_param(&mc.skip, &mc.limit),
        Statement::MatchMerge(mm) => !skip_limit_has_param(&mm.skip, &mm.limit),
        Statement::Delete(d) => !skip_limit_has_param(&d.skip, &d.limit),
        Statement::Set(s) => {
            !skip_limit_has_param(&s.skip, &s.limit)
                && !intermediates_block_caching(&s.intermediate_clauses)
        }
        Statement::Remove(r) => !skip_limit_has_param(&r.skip, &r.limit),
        Statement::Merge(m) => !skip_limit_has_param(&m.skip, &m.limit),
        Statement::Unwind(u) => match &u.body {
            UnwindBody::Return {
                intermediate_clauses,
                skip,
                limit,
                ..
            } => {
                !skip_limit_has_param(skip, limit)
                    && !intermediates_block_caching(intermediate_clauses)
            }
            UnwindBody::Create {
                intermediate_clauses,
                skip,
                limit,
                ..
            } => {
                !skip_limit_has_param(skip, limit)
                    && !intermediates_block_caching(intermediate_clauses)
            }
        },
        Statement::Return(r) => !skip_limit_has_param(&r.skip, &r.limit),
        Statement::MultiClause(mc) => {
            !skip_limit_has_param(&mc.skip, &mc.limit)
                && mc.clauses.iter().all(|c| !clause_blocks_caching(c))
        }
    }
}

fn clause_blocks_caching(clause: &Clause) -> bool {
    match clause {
        Clause::Call { .. } => true,
        Clause::With(w) => skip_limit_has_param(&w.skip, &w.limit),
        _ => false,
    }
}

fn intermediates_block_caching(clauses: &[IntermediateClause]) -> bool {
    clauses.iter().any(|c| match c {
        IntermediateClause::With(w) => skip_limit_has_param(&w.skip, &w.limit),
        _ => false,
    })
}

fn skip_limit_has_param(skip: &Option<Expr>, limit: &Option<Expr>) -> bool {
    expr_opt_has_param(skip) || expr_opt_has_param(limit)
}

fn expr_opt_has_param(e: &Option<Expr>) -> bool {
    e.as_ref().is_some_and(expr_has_param)
}

fn expr_has_param(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Parameter(_) => true,
        ExprKind::BinaryOp { left, right, .. } => expr_has_param(left) || expr_has_param(right),
        ExprKind::Not(e) | ExprKind::IsNull(e) | ExprKind::IsNotNull(e) => expr_has_param(e),
        ExprKind::FunctionCall { args, .. } => args.iter().any(expr_has_param),
        ExprKind::Case {
            operand,
            alternatives,
            default,
        } => {
            operand.as_deref().is_some_and(expr_has_param)
                || alternatives
                    .iter()
                    .any(|(c, r)| expr_has_param(c) || expr_has_param(r))
                || default.as_deref().is_some_and(expr_has_param)
        }
        ExprKind::List(items) => items.iter().any(expr_has_param),
        ExprKind::ListComprehension {
            list_expr,
            filter,
            map_expr,
            ..
        } => {
            expr_has_param(list_expr)
                || filter.as_deref().is_some_and(expr_has_param)
                || map_expr.as_deref().is_some_and(expr_has_param)
        }
        ExprKind::PatternComprehension {
            where_clause,
            map_expr,
            ..
        } => where_clause.as_deref().is_some_and(expr_has_param) || expr_has_param(map_expr),
        ExprKind::Quantifier {
            list_expr,
            predicate,
            ..
        } => expr_has_param(list_expr) || expr_has_param(predicate),
        ExprKind::Exists { where_clause, .. } => {
            where_clause.as_deref().is_some_and(expr_has_param)
        }
        ExprKind::ExistsSubquery(_) => false, // subqueries don't expose params to outer SKIP/LIMIT positions
        ExprKind::MapLiteral(pairs) => pairs.iter().any(|(_, v)| expr_has_param(v)),
        ExprKind::Index { expr: e, index } => expr_has_param(e) || expr_has_param(index),
        ExprKind::DotAccess { expr: e, .. } => expr_has_param(e),
        ExprKind::Slice {
            expr: e,
            start,
            end,
        } => {
            expr_has_param(e)
                || start.as_deref().is_some_and(expr_has_param)
                || end.as_deref().is_some_and(expr_has_param)
        }
        ExprKind::Literal(_)
        | ExprKind::Property(_, _)
        | ExprKind::Variable(_)
        | ExprKind::HasLabel(_, _)
        | ExprKind::PatternPredicate(_)
        | ExprKind::Star => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ir::LogicalOp;
    use crate::cypher::parser;

    fn dummy_plan() -> LogicalOp {
        LogicalOp::SingleRow
    }

    #[test]
    fn hit_returns_equal_plan() {
        let cache = PlanCache::with_capacity(8);
        let q = "MATCH (n:Person) RETURN n";
        let a = cache.get_or_plan(q, 0, || Ok(dummy_plan())).unwrap();
        let b = cache
            .get_or_plan(q, 0, || panic!("should not rebuild on hit"))
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn epoch_change_misses() {
        let cache = PlanCache::with_capacity(8);
        let q = "MATCH (n:Person) RETURN n";
        cache.get_or_plan(q, 0, || Ok(dummy_plan())).unwrap();
        let mut rebuilt = false;
        cache
            .get_or_plan(q, 1, || {
                rebuilt = true;
                Ok(dummy_plan())
            })
            .unwrap();
        assert!(rebuilt, "epoch change should miss");
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let cache = PlanCache::with_capacity(2);
        cache
            .get_or_plan("MATCH (n) RETURN n", 0, || Ok(dummy_plan()))
            .unwrap();
        cache
            .get_or_plan("MATCH (n:A) RETURN n", 0, || Ok(dummy_plan()))
            .unwrap();
        cache
            .get_or_plan("MATCH (n:B) RETURN n", 0, || Ok(dummy_plan()))
            .unwrap();
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn build_error_is_not_cached() {
        let cache = PlanCache::with_capacity(8);
        let q = "MATCH (n) RETURN n";
        let err: Result<LogicalOp> =
            cache.get_or_plan(q, 0, || Err(crate::types::GraphError::syntax("forced")));
        assert!(err.is_err());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cacheability_skip_limit_param() {
        let stmt = parser::parse("MATCH (n) RETURN n LIMIT $k").unwrap();
        assert!(!is_plan_cacheable(&stmt));
        let stmt = parser::parse("MATCH (n) RETURN n SKIP $s LIMIT 10").unwrap();
        assert!(!is_plan_cacheable(&stmt));
        let stmt = parser::parse("MATCH (n) RETURN n LIMIT 10").unwrap();
        assert!(is_plan_cacheable(&stmt));
    }

    #[test]
    fn cacheability_with_clause_skip_param() {
        let stmt = parser::parse("MATCH (n) WITH n LIMIT $k RETURN n").unwrap();
        assert!(!is_plan_cacheable(&stmt));
    }

    #[test]
    fn cacheability_param_in_where_is_ok() {
        let stmt = parser::parse("MATCH (n:Person) WHERE n.age = $age RETURN n").unwrap();
        assert!(is_plan_cacheable(&stmt));
    }
}
