//! Stepping logic — determines when to pause after a resume command.

use super::breakpoint::BreakpointRegistry;
use super::types::StepMode;

/// Check whether the VM should pause at the current location.
///
/// This is called from the line-event hook on every executed line.
/// Returns `true` if the VM should enter the paused loop.
///
/// **Phase 1 limitation:** breakpoint conditions are not evaluated.
/// All enabled breakpoints fire unconditionally.
pub(crate) fn should_pause(
    step_mode: &Option<StepMode>,
    breakpoints: &BreakpointRegistry,
    source: &str,
    line: usize,
    call_depth: usize,
) -> bool {
    // 1. Step mode takes precedence.
    if let Some(mode) = step_mode {
        match mode {
            StepMode::Into => return true,
            StepMode::Over { start_depth } => {
                if call_depth <= *start_depth {
                    return true;
                }
            }
            StepMode::Out { start_depth } => {
                if call_depth < *start_depth {
                    return true;
                }
            }
        }
    }

    // 2. Check breakpoints.
    // TODO(Phase 2): evaluate bp.condition when present.
    if let Some(bp) = breakpoints.find(source, line) {
        if bp.enabled {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_step_no_bp_continues() {
        let registry = BreakpointRegistry::new();
        assert!(!should_pause(&None, &registry, "@test.lua", 5, 0));
    }

    #[test]
    fn step_into_always_pauses() {
        let registry = BreakpointRegistry::new();
        assert!(should_pause(
            &Some(StepMode::Into),
            &registry,
            "@test.lua",
            1,
            0,
        ));
    }

    #[test]
    fn step_over_pauses_at_same_depth() {
        let registry = BreakpointRegistry::new();
        let mode = Some(StepMode::Over { start_depth: 2 });
        assert!(should_pause(&mode, &registry, "@t.lua", 1, 2));
        assert!(should_pause(&mode, &registry, "@t.lua", 1, 1));
        assert!(!should_pause(&mode, &registry, "@t.lua", 1, 3));
    }

    #[test]
    fn step_out_pauses_at_shallower_depth() {
        let registry = BreakpointRegistry::new();
        let mode = Some(StepMode::Out { start_depth: 2 });
        assert!(should_pause(&mode, &registry, "@t.lua", 1, 1));
        assert!(!should_pause(&mode, &registry, "@t.lua", 1, 2));
        assert!(!should_pause(&mode, &registry, "@t.lua", 1, 3));
    }

    #[test]
    fn breakpoint_hit() {
        let mut registry = BreakpointRegistry::new();
        registry.add("@test.lua".into(), 10, None).unwrap();
        assert!(should_pause(&None, &registry, "@test.lua", 10, 0));
        assert!(!should_pause(&None, &registry, "@test.lua", 11, 0));
    }

    #[test]
    fn disabled_breakpoint_skipped() {
        let mut registry = BreakpointRegistry::new();
        let id = registry.add("@test.lua".into(), 10, None).unwrap();
        // Remove and re-verify — we don't have enable/disable toggle yet
        // so we test via removal.
        registry.remove(id);
        assert!(!should_pause(&None, &registry, "@test.lua", 10, 0));
    }
}
