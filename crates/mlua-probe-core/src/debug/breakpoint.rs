//! Breakpoint storage with O(1) lookup by (source, line).

use std::collections::HashMap;
use std::sync::Arc;

use super::error::DebugError;
use super::types::BreakpointId;

/// A single breakpoint definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: BreakpointId,
    /// Source identifier (e.g. `"@main.lua"`).
    ///
    /// Shared via [`Arc<str>`] — the same allocation is reused across
    /// `Breakpoint::source`, the `by_source` map key, and `by_id` value.
    pub source: Arc<str>,
    /// 1-based line number.
    pub line: usize,
    /// Optional Lua expression — breakpoint fires only when this
    /// evaluates to `true`.
    pub condition: Option<String>,
    /// Whether this breakpoint is active.
    pub enabled: bool,
    /// Number of times this breakpoint has been hit.
    #[allow(dead_code)] // Phase 2: hit-count breakpoints
    pub(crate) hit_count: u32,
}

/// Manages all breakpoints for a debug session.
///
/// Uses a two-level HashMap (`source → line → Breakpoint`) so that
/// `find()` does not allocate — critical because it is called on
/// every Lua line event inside the debug hook.
pub(crate) struct BreakpointRegistry {
    by_source: HashMap<Arc<str>, HashMap<usize, Breakpoint>>,
    by_id: HashMap<BreakpointId, (Arc<str>, usize)>,
    next_id: BreakpointId,
}

impl BreakpointRegistry {
    pub fn new() -> Self {
        Self {
            by_source: HashMap::new(),
            by_id: HashMap::new(),
            next_id: BreakpointId::FIRST,
        }
    }

    /// Add or replace a breakpoint at `(source, line)`.
    /// Returns the assigned breakpoint ID.
    ///
    /// **Phase 1 limitation:** `condition` is stored but **not evaluated**
    /// by the stepping engine.  All breakpoints fire unconditionally.
    /// Condition evaluation is planned for Phase 2.
    ///
    /// # Errors
    ///
    /// Returns [`DebugError::Internal`] if the ID space is exhausted
    /// (u32::MAX breakpoints created).
    pub fn add(
        &mut self,
        source: String,
        line: usize,
        condition: Option<String>,
    ) -> Result<BreakpointId, DebugError> {
        // Single Arc allocation shared across Breakpoint, by_source key, and by_id value.
        let source: Arc<str> = Arc::from(source);

        // If a BP already exists at this location, remove its ID mapping.
        if let Some(lines) = self.by_source.get_mut(&*source) {
            if let Some(existing) = lines.remove(&line) {
                self.by_id.remove(&existing.id);
            }
        }

        let id = self.next_id;
        self.next_id = self.next_id.next().ok_or_else(|| {
            DebugError::Internal(
                "breakpoint ID space exhausted (u32::MAX breakpoints created)".into(),
            )
        })?;

        let bp = Breakpoint {
            id,
            source: Arc::clone(&source),
            line,
            condition,
            enabled: true,
            hit_count: 0,
        };

        self.by_source
            .entry(Arc::clone(&source))
            .or_default()
            .insert(line, bp);
        self.by_id.insert(id, (source, line));

        Ok(id)
    }

    /// Remove a breakpoint by ID. Returns `true` if it existed.
    pub fn remove(&mut self, id: BreakpointId) -> bool {
        if let Some((source, line)) = self.by_id.remove(&id) {
            if let Some(lines) = self.by_source.get_mut(&*source) {
                lines.remove(&line);
                if lines.is_empty() {
                    self.by_source.remove(&*source);
                }
            }
            true
        } else {
            false
        }
    }

    /// Look up a breakpoint at `(source, line)`.
    ///
    /// Allocation-free: uses `&str` key into the two-level map.
    pub fn find(&self, source: &str, line: usize) -> Option<&Breakpoint> {
        self.by_source.get(source).and_then(|m| m.get(&line))
    }

    /// Increment the hit count for a breakpoint. Returns the new count.
    #[allow(dead_code)] // Phase 2: hit-count breakpoints
    pub fn record_hit(&mut self, source: &str, line: usize) -> u32 {
        if let Some(bp) = self
            .by_source
            .get_mut(source)
            .and_then(|m| m.get_mut(&line))
        {
            bp.hit_count = bp.hit_count.saturating_add(1);
            bp.hit_count
        } else {
            0
        }
    }

    /// Return all breakpoints (cloned).
    pub fn list(&self) -> Vec<Breakpoint> {
        self.by_source
            .values()
            .flat_map(|m| m.values())
            .cloned()
            .collect()
    }

    /// Returns `true` if any breakpoints exist for the given source.
    #[allow(dead_code)] // Phase 2: conditional hook installation
    pub fn has_breakpoints_in(&self, source: &str) -> bool {
        self.by_source.get(source).is_some_and(|m| !m.is_empty())
    }

    /// Returns `true` if the registry is empty.
    #[allow(dead_code)] // Phase 2: conditional hook installation
    pub fn is_empty(&self) -> bool {
        self.by_source.is_empty()
    }
}

impl Default for BreakpointRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_find() {
        let mut registry = BreakpointRegistry::new();
        let id = registry.add("@test.lua".into(), 10, None).unwrap();

        let bp = registry.find("@test.lua", 10).unwrap();
        assert_eq!(bp.id, id);
        assert_eq!(bp.line, 10);
        assert!(bp.enabled);
        assert_eq!(bp.hit_count, 0);
    }

    #[test]
    fn add_replaces_existing() {
        let mut registry = BreakpointRegistry::new();
        let id1 = registry.add("@test.lua".into(), 10, None).unwrap();
        let id2 = registry
            .add("@test.lua".into(), 10, Some("x > 5".into()))
            .unwrap();

        assert_ne!(id1, id2);
        assert!(registry.find("@test.lua", 10).unwrap().condition.is_some());
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn remove_returns_false_for_missing() {
        let mut registry = BreakpointRegistry::new();
        assert!(!registry.remove(BreakpointId(999)));
    }

    #[test]
    fn remove_existing() {
        let mut registry = BreakpointRegistry::new();
        let id = registry.add("@test.lua".into(), 5, None).unwrap();
        assert!(registry.remove(id));
        assert!(registry.find("@test.lua", 5).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn record_hit() {
        let mut registry = BreakpointRegistry::new();
        registry.add("@test.lua".into(), 3, None).unwrap();
        assert_eq!(registry.record_hit("@test.lua", 3), 1);
        assert_eq!(registry.record_hit("@test.lua", 3), 2);
        assert_eq!(registry.record_hit("@missing.lua", 1), 0);
    }

    #[test]
    fn has_breakpoints_in() {
        let mut registry = BreakpointRegistry::new();
        assert!(!registry.has_breakpoints_in("@test.lua"));
        registry.add("@test.lua".into(), 1, None).unwrap();
        assert!(registry.has_breakpoints_in("@test.lua"));
        assert!(!registry.has_breakpoints_in("@other.lua"));
    }
}
