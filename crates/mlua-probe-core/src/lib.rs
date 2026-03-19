//! Core debug and testing engine for mlua.
//!
//! Provides two main capabilities for Lua code running inside an
//! mlua `Lua` instance:
//!
//! - **Debugging** — breakpoints, stepping, variable inspection, and
//!   expression evaluation via [`DebugSession`] / [`DebugController`].
//! - **Testing** — BDD test framework via [`mlua_lspec`], re-exported
//!   as the [`testing`] module.
//!
//! # Architecture
//!
//! The debug engine uses a **VM-thread blocking** model:
//!
//! - A Lua debug hook pauses the VM thread when a breakpoint or step
//!   condition is met.
//! - While paused, the hook dispatches inspection commands (locals,
//!   upvalues, evaluate) **on the VM thread** — required because
//!   `lua_getlocal` and friends are not thread-safe.
//! - A resume command (continue / step) unblocks the hook and lets
//!   Lua execution proceed.
//!
//! The testing module is independent of the debug engine.  It creates
//! a fresh Lua VM per test run, so tests are fully isolated.
//!
//! # Debugging example
//!
//! ```rust,no_run
//! use mlua::prelude::*;
//! use mlua_probe_core::{DebugSession, DebugEvent};
//!
//! let lua = Lua::new();
//! let (session, controller) = DebugSession::new();
//! session.attach(&lua).unwrap();
//!
//! controller.set_breakpoint("@main.lua", 3, None).unwrap();
//!
//! // Run Lua on a separate thread so the current thread can
//! // interact with the controller.
//! let handle = std::thread::spawn(move || {
//!     lua.load(r#"
//!         local x = 1
//!         local y = 2
//!         local z = x + y
//!         return z
//!     "#)
//!     .set_name("@main.lua")
//!     .eval::<i64>()
//! });
//!
//! // Wait for the VM to pause.
//! let event = controller.wait_event().unwrap();
//! // Inspect, step, continue …
//! controller.continue_execution().unwrap();
//!
//! let result = handle.join().unwrap().unwrap();
//! assert_eq!(result, 3);
//! ```
//!
//! # Testing example
//!
//! ```rust
//! use mlua_probe_core::testing;
//!
//! let summary = testing::framework::run_tests(r#"
//!     local describe, it, expect = lust.describe, lust.it, lust.expect
//!     describe('math', function()
//!         it('adds', function()
//!             expect(1 + 1).to.equal(2)
//!         end)
//!     end)
//! "#, "@test.lua").unwrap();
//!
//! assert_eq!(summary.passed, 1);
//! assert_eq!(summary.failed, 0);
//! ```

mod debug;

/// BDD test framework for Lua, re-exported from [`mlua_lspec`].
pub mod testing {
    pub use mlua_lspec::{TestResult, TestSummary};

    pub mod framework {
        pub use mlua_lspec::{collect_results, register, run_tests};
    }

    pub use mlua_lspec::doubles;
}

/// Lua static checker, re-exported from [`mlua_check`].
///
/// Detects undefined variables, globals, and fields before execution.
pub mod checking {
    pub use mlua_check::{
        Diagnostic, LintConfig, LintEngine, LintPolicy, LintResult, RuleId, Severity,
    };

    pub mod framework {
        pub use mlua_check::{collect_symbols, lint, register, register_with_config, run_lint};
    }
}

pub use debug::breakpoint::Breakpoint;
pub use debug::controller::DebugController;
pub use debug::engine::{CompletionNotifier, DebugSession};
pub use debug::error::DebugError;
pub use debug::types::{
    BreakpointId, DebugEvent, FrameKind, OutputCategory, PauseReason, SessionState, StackFrame,
    StepMode, Variable, VariableRef,
};
