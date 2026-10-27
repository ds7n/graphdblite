//! Per-query bounded LRU of compiled regexes for the `=~` operator.
//!
//! Patterns are anchored as `^(?:<pat>)$` at compile time so callers get
//! full-match semantics (openCypher / Java `String.matches()`). The cache
//! is keyed on the *original* user pattern so the anchoring is invisible
//! to lookups.

use regex::Regex;

use crate::types::{ErrorCode, GraphError, QueryPhase, Result};

const DEFAULT_CAPACITY: usize = 32;

#[cfg(test)]
thread_local! {
    /// Cache-miss compilations on this thread (tests only).
    static COMPILES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Number of cache-miss compilations so far on this thread (tests only).
#[cfg(test)]
pub(crate) fn compile_count() -> usize {
    COMPILES.with(|c| c.get())
}

/// Bounded least-recently-used cache of compiled regexes.
///
/// Stored as a `Vec` of `(pattern, Regex)` pairs because the working set
/// is tiny (≤32) and a linear scan is faster than a `HashMap` at that
/// size.
pub struct RegexCache {
    entries: Vec<(String, Regex)>,
    capacity: usize,
}

impl Default for RegexCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl RegexCache {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns a compiled regex for `pat`, compiling and inserting on
    /// miss. Pattern is anchored as `^(?:<pat>)$` before compilation so
    /// the resulting regex full-matches its input.
    ///
    /// On compilation failure, returns `Err` and does not cache the
    /// failure — subsequent calls retry compilation.
    pub fn get_or_compile(&mut self, pat: &str) -> Result<&Regex> {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == pat) {
            // Move-to-end = mark most-recently-used.
            let entry = self.entries.remove(pos);
            self.entries.push(entry);
            return Ok(&self.entries.last().unwrap().1);
        }

        #[cfg(test)]
        COMPILES.with(|c| c.set(c.get() + 1));
        let invalid = |err: regex::Error| {
            GraphError::query(
                QueryPhase::Runtime,
                ErrorCode::InvalidArgumentValue,
                format!("invalid regex pattern `{pat}`: {err}"),
            )
        };
        // Validate the user pattern on its own first: wrapping an unbalanced
        // pattern like `a)|(b` in the anchoring group would balance it and
        // change its meaning (`^(?:a)|(b)$`). Only cache misses pay this.
        Regex::new(pat).map_err(invalid)?;
        let anchored = format!("^(?:{pat})$");
        let re = Regex::new(&anchored).map_err(invalid)?;

        if self.entries.len() >= self.capacity {
            self.entries.remove(0); // evict least-recently-used
        }
        self.entries.push((pat.to_string(), re));
        Ok(&self.entries.last().unwrap().1)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn contains_pattern(&self, pat: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == pat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_and_full_matches() {
        let mut c = RegexCache::default();
        let re = c.get_or_compile("hello").unwrap();
        assert!(re.is_match("hello"));
        assert!(!re.is_match("ahello")); // anchored: must match from start
        assert!(!re.is_match("hello!")); // anchored: must match to end
    }

    #[test]
    fn cache_hit_returns_same_compiled_regex() {
        let mut c = RegexCache::default();
        c.get_or_compile("h.*").unwrap();
        assert_eq!(c.len(), 1);
        c.get_or_compile("h.*").unwrap();
        assert_eq!(c.len(), 1, "second call must hit cache, not append");
    }

    #[test]
    fn eviction_at_capacity() {
        let mut c = RegexCache::with_capacity(2);
        c.get_or_compile("a").unwrap();
        c.get_or_compile("b").unwrap();
        c.get_or_compile("c").unwrap();
        assert_eq!(c.len(), 2);
        assert!(!c.contains_pattern("a"), "lru entry must be evicted");
        assert!(c.contains_pattern("b"));
        assert!(c.contains_pattern("c"));
    }

    #[test]
    fn touching_entry_marks_it_recent() {
        let mut c = RegexCache::with_capacity(2);
        c.get_or_compile("a").unwrap();
        c.get_or_compile("b").unwrap();
        c.get_or_compile("a").unwrap(); // refresh `a`
        c.get_or_compile("c").unwrap(); // should evict `b`, not `a`
        assert!(c.contains_pattern("a"));
        assert!(!c.contains_pattern("b"));
        assert!(c.contains_pattern("c"));
    }

    #[test]
    fn invalid_pattern_returns_err_and_is_not_cached() {
        let mut c = RegexCache::default();
        let err = c.get_or_compile("([unclosed").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid regex pattern"), "got: {msg}");
        assert!(
            msg.contains("([unclosed"),
            "must echo offending pattern: {msg}"
        );
        assert_eq!(c.len(), 0, "failed compile must not insert");
    }

    #[test]
    fn auto_tx_execute_reuses_one_compiled_regex() {
        // `Database::execute` without an open transaction must publish the
        // per-query cache too; otherwise every row recompiled the pattern.
        let mut db = crate::Database::open_memory().unwrap();
        let before = compile_count();
        let rows = db
            .execute(
                "UNWIND range(1, 50) AS i RETURN count(CASE WHEN 'ab' =~ 'a.' THEN 1 END) AS c",
            )
            .unwrap();
        assert_eq!(rows[0].get("c"), Some(&crate::types::Value::I64(50)));
        assert_eq!(
            compile_count() - before,
            1,
            "pattern compiled once per query"
        );
    }

    #[test]
    fn inline_case_insensitive_flag() {
        let mut c = RegexCache::default();
        let re = c.get_or_compile("(?i)hello").unwrap();
        assert!(re.is_match("HELLO"));
        assert!(re.is_match("Hello"));
    }
}
