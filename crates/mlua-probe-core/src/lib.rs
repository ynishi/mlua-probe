//! Core debug engine for mlua.
//!
//! Provides breakpoints, stepping, variable inspection, and expression
//! evaluation for Lua code running inside an mlua `Lua` instance.
//!
//! # Architecture
//!
//! The engine uses a **VM-thread blocking** model:
//!
//! - A Lua debug hook pauses the VM thread when a breakpoint or step
//!   condition is met.
//! - While paused, the hook dispatches inspection commands (locals,
//!   upvalues, evaluate) **on the VM thread** — required because
//!   `lua_getlocal` and friends are not thread-safe.
//! - A resume command (continue / step) unblocks the hook and lets
//!   Lua execution proceed.
//!
//! # Usage
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

mod debug;

pub use debug::breakpoint::Breakpoint;
pub use debug::controller::DebugController;
pub use debug::engine::{CompletionNotifier, DebugSession};
pub use debug::error::DebugError;
pub use debug::types::{
    BreakpointId, DebugEvent, FrameKind, OutputCategory, PauseReason, SessionState, StackFrame,
    StepMode, Variable, VariableRef,
};
