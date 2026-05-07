//! Bounded per-`Database` cache for parsed Cypher ASTs.
//!
//! Profiling showed that a hot point-lookup (`MATCH (p:Person {name: 'p5000'})
//! RETURN p.age`) spends ~49% of wall-clock in pest. Applications that issue
//! the same query text repeatedly (REPLs, request handlers, ORMs) reparse the
//! grammar on every call. Caching the parsed [`Statement`] avoids that work
//! while keeping the planner free to make index-aware decisions on each call.
//!
//! ## What's cached, and why caching the AST is safe
//!
//! Only the parser output (the AST [`Statement`]) is stored. Two reasons:
//!
//! - The AST is index-independent. Plans, by contrast, depend on which
//!   indexes exist on which `(label, property)` pairs — and plans embed
//!   resolved literal values from `$param` substitution. Caching plans would
//!   require invalidation on `CREATE INDEX`/`DROP INDEX` *and* per-parameter
//!   keying. The AST has neither concern.
//! - The AST is parameter-agnostic. `parser::resolve_params` is called after
//!   we hand back the cached AST, so two callers passing different params
//!   for the same query string both benefit.
//!
//! ## Eviction policy
//!
//! Bounded FIFO. When the map reaches capacity we drop the oldest insertion.
//! True LRU would require touch-on-read bookkeeping; for typical workloads
//! (a handful of query templates issued repeatedly) FIFO is indistinguishable
//! and avoids per-hit mutation.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::cypher::ast::Statement;
use crate::cypher::parser;
use crate::types::Result;

/// Default cache capacity. Sized for typical applications: enough to hold
/// every distinct query template a CRUD app issues, small enough that a
/// pathological workload (unique query per request) doesn't bloat memory.
const DEFAULT_CAPACITY: usize = 128;

pub(crate) struct ParseCache {
    inner: Mutex<Inner>,
}

struct Inner {
    map: HashMap<String, Statement>,
    order: VecDeque<String>,
    capacity: usize,
}

impl ParseCache {
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

    /// Resolve `cypher` to a parsed [`Statement`], reusing a cached result
    /// when available. Returns a fresh `Statement` (clone of the cached
    /// entry) so callers can freely apply `resolve_params` without
    /// affecting subsequent cache hits.
    pub(crate) fn get_or_parse(&self, cypher: &str) -> Result<Statement> {
        // Fast path: hit. Clone the cached AST under the lock; the parser is
        // never invoked on a hit.
        if let Ok(guard) = self.inner.lock() {
            if let Some(stmt) = guard.map.get(cypher) {
                return Ok(stmt.clone());
            }
        }

        // Miss: parse outside the lock so concurrent misses on different
        // queries don't block each other on the parser. Two concurrent
        // misses on the *same* query string both parse — wasteful but
        // correct, and the second insert is a no-op overwrite.
        let stmt = parser::parse(cypher)?;

        if let Ok(mut guard) = self.inner.lock() {
            // If a concurrent miss already inserted the same key, prefer the
            // existing entry (avoids touching `order` twice for one key).
            if !guard.map.contains_key(cypher) {
                if guard.map.len() >= guard.capacity {
                    if let Some(oldest) = guard.order.pop_front() {
                        guard.map.remove(&oldest);
                    }
                }
                guard.order.push_back(cypher.to_string());
                guard.map.insert(cypher.to_string(), stmt.clone());
            }
        }
        Ok(stmt)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().map(|g| g.map.len()).unwrap_or(0)
    }
}

impl Default for ParseCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_returns_equal_ast() {
        let cache = ParseCache::with_capacity(8);
        let q = "MATCH (n:Person) RETURN n";
        let a = cache.get_or_parse(q).unwrap();
        let b = cache.get_or_parse(q).unwrap();
        assert_eq!(a, b);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn miss_parses_and_inserts() {
        let cache = ParseCache::with_capacity(8);
        cache.get_or_parse("MATCH (n) RETURN n").unwrap();
        cache.get_or_parse("MATCH (n:Person) RETURN n").unwrap();
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let cache = ParseCache::with_capacity(2);
        cache.get_or_parse("MATCH (n) RETURN n").unwrap();
        cache.get_or_parse("MATCH (n:A) RETURN n").unwrap();
        cache.get_or_parse("MATCH (n:B) RETURN n").unwrap();
        // First entry should have been evicted.
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn parse_error_is_not_cached() {
        let cache = ParseCache::with_capacity(8);
        assert!(cache.get_or_parse("NOT VALID CYPHER").is_err());
        assert_eq!(cache.len(), 0);
    }
}
