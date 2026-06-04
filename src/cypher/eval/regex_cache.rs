//! Per-query bounded LRU of compiled regexes for the `=~` operator.
//!
//! Patterns are anchored as `^(?:<pat>)$` at compile time so callers get
//! full-match semantics (openCypher / Java `String.matches()`). The cache
//! is keyed on the *original* user pattern so the anchoring is invisible
//! to lookups.

use regex::Regex;

use crate::types::{ErrorCode, GraphError, QueryPhase, Result};

const DEFAULT_CAPACITY: usize = 32;

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

        let anchored = format!("^(?:{pat})$");
        let re = Regex::new(&anchored).map_err(|err| {
            GraphError::query(
                QueryPhase::Runtime,
                ErrorCode::InvalidArgumentValue,
                format!("invalid regex pattern `{pat}`: {err}"),
            )
        })?;

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
    fn inline_case_insensitive_flag() {
        let mut c = RegexCache::default();
        let re = c.get_or_compile("(?i)hello").unwrap();
        assert!(re.is_match("HELLO"));
        assert!(re.is_match("Hello"));
    }
}
