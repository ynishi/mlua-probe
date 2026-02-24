//! Source registry — maps chunk names to source text.
//!
//! Used for breakpoint validation and source display.

use std::collections::HashMap;

/// Stores Lua source code keyed by chunk name.
pub struct SourceRegistry {
    sources: HashMap<String, SourceEntry>,
}

/// A single registered source.
///
/// Phase 2: used for source display and breakpoint validation.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Phase 2: fields read by get() / get_line()
pub struct SourceEntry {
    /// Full source text.  Lines are derived on demand via
    /// [`get_line`](SourceRegistry::get_line) to avoid doubling
    /// memory usage.
    pub content: String,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
        }
    }

    /// Register source code under a chunk name.
    ///
    /// Conventionally the name starts with `@` for file-like sources
    /// (e.g. `"@main.lua"`).
    pub fn register(&mut self, name: &str, content: &str) {
        self.sources.insert(
            name.to_string(),
            SourceEntry {
                content: content.to_string(),
            },
        );
    }

    /// Retrieve a registered source.
    #[allow(dead_code)] // Phase 2: source display
    pub fn get(&self, name: &str) -> Option<&SourceEntry> {
        self.sources.get(name)
    }

    /// Get a specific line (1-based). Returns `None` if out of range.
    ///
    /// Computes on demand from [`SourceEntry::content`] — O(n) per call
    /// but avoids the memory cost of pre-split `Vec<String>`.  Not on
    /// any hot path (debug display only).
    #[allow(dead_code)] // Phase 2: source display
    pub fn get_line(&self, name: &str, line: usize) -> Option<&str> {
        self.sources
            .get(name)
            .and_then(|e| line.checked_sub(1).and_then(|i| e.content.lines().nth(i)))
    }

    /// Check whether a source is registered.
    #[allow(dead_code)] // Phase 2: breakpoint validation
    pub fn contains(&self, name: &str) -> bool {
        self.sources.contains_key(name)
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_get() {
        let mut reg = SourceRegistry::new();
        reg.register("@test.lua", "local x = 1\nlocal y = 2\n");

        let entry = reg.get("@test.lua").unwrap();
        assert_eq!(entry.content.lines().count(), 2);
        assert!(reg.contains("@test.lua"));
        assert!(!reg.contains("@missing.lua"));
    }

    #[test]
    fn get_line_1based() {
        let mut reg = SourceRegistry::new();
        reg.register("@t.lua", "aaa\nbbb\nccc");

        assert_eq!(reg.get_line("@t.lua", 1), Some("aaa"));
        assert_eq!(reg.get_line("@t.lua", 2), Some("bbb"));
        assert_eq!(reg.get_line("@t.lua", 3), Some("ccc"));
        assert_eq!(reg.get_line("@t.lua", 0), None);
        assert_eq!(reg.get_line("@t.lua", 4), None);
    }
}
