//! Stepping logic — determines when to pause after a resume command.

use super::types::StepMode;

/// Check whether step mode requires a pause at the current call depth.
///
/// This only handles stepping (Into / Over / Out).  Breakpoint checking
/// — including condition evaluation — is handled by the hook callback
/// in [`engine`](super::engine).
pub(crate) fn step_triggers(step_mode: &Option<StepMode>, call_depth: usize) -> bool {
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

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_step_no_bp_continues() {
        assert!(!step_triggers(&None, 0));
    }

    #[test]
    fn step_into_always_pauses() {
        assert!(step_triggers(&Some(StepMode::Into), 0));
    }

    #[test]
    fn step_over_pauses_at_same_depth() {
        let mode = Some(StepMode::Over { start_depth: 2 });
        assert!(step_triggers(&mode, 2));
        assert!(step_triggers(&mode, 1));
        assert!(!step_triggers(&mode, 3));
    }

    #[test]
    fn step_out_pauses_at_shallower_depth() {
        let mode = Some(StepMode::Out { start_depth: 2 });
        assert!(step_triggers(&mode, 1));
        assert!(!step_triggers(&mode, 2));
        assert!(!step_triggers(&mode, 3));
    }
}
