//! Frontend-facing API for interacting with a debug session.
//!
//! [`DebugController`] is `Clone + Send + Sync` and can be shared
//! across threads.  All methods are blocking.
//!
//! # Two access patterns
//!
//! The controller uses two distinct mechanisms depending on the
//! operation type:
//!
//! - **Breakpoint management** — operates directly on the shared
//!   [`BreakpointRegistry`] via `Arc<Mutex<…>>`.  Works in any session
//!   state (Running, Paused, etc.) because DAP clients can set
//!   breakpoints at any time.
//!
//! - **Inspection & execution control** — sends a [`DebugCommand`]
//!   through the `mpsc` command channel.  These are dispatched on the
//!   **VM thread** inside the paused loop, which is required because
//!   `lua_getlocal` and friends are not thread-safe.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use super::breakpoint::{Breakpoint, BreakpointRegistry};
use super::error::DebugError;
use super::types::*;

/// Controls a [`DebugSession`](super::engine::DebugSession) from a
/// frontend (MCP server, DAP adapter, Web console, …).
///
/// Thread-safe: `Clone + Send + Sync`.
#[derive(Clone)]
pub struct DebugController {
    cmd_tx: mpsc::Sender<DebugCommand>,
    evt_rx: Arc<Mutex<mpsc::Receiver<DebugEvent>>>,
    state: Arc<AtomicU8>,
    breakpoints: Arc<Mutex<BreakpointRegistry>>,
    pause_requested: Arc<AtomicBool>,
    /// Shared with [`SessionInner`](super::engine::SessionInner) —
    /// updated after breakpoint add/remove for fast-path checking.
    has_active_breakpoints: Arc<AtomicBool>,
}

impl DebugController {
    pub(crate) fn new(
        cmd_tx: mpsc::Sender<DebugCommand>,
        evt_rx: mpsc::Receiver<DebugEvent>,
        state: Arc<AtomicU8>,
        breakpoints: Arc<Mutex<BreakpointRegistry>>,
        pause_requested: Arc<AtomicBool>,
        has_active_breakpoints: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cmd_tx,
            evt_rx: Arc::new(Mutex::new(evt_rx)),
            state,
            breakpoints,
            pause_requested,
            has_active_breakpoints,
        }
    }

    // ── State query ─────────────────────────────────

    /// Current session state (non-blocking, coarse-grained).
    ///
    /// Returns whether the VM is Idle, Running, Paused, or Terminated.
    /// **`PauseReason` is not preserved** in the atomic representation —
    /// use [`wait_event`](Self::wait_event) to obtain the precise reason
    /// from [`DebugEvent::Paused`].
    pub fn state(&self) -> SessionState {
        SessionState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Block until the next event arrives from the engine.
    ///
    /// This is the **authoritative source** for session events.  In
    /// particular, [`DebugEvent::Paused`] carries the precise
    /// [`PauseReason`] that [`state()`](Self::state) does not preserve.
    pub fn wait_event(&self) -> Result<DebugEvent, DebugError> {
        let rx = self.evt_rx.lock()?;
        rx.recv().map_err(|_| {
            DebugError::SessionClosed("event receive failed: engine disconnected".into())
        })
    }

    /// Try to receive an event without blocking.
    pub fn try_event(&self) -> Result<Option<DebugEvent>, DebugError> {
        let rx = self.evt_rx.lock()?;
        Ok(rx.try_recv().ok())
    }

    /// Block until the next event arrives, with a timeout.
    ///
    /// Returns `Ok(None)` if the timeout elapses without an event.
    pub fn wait_event_timeout(&self, timeout: Duration) -> Result<Option<DebugEvent>, DebugError> {
        let rx = self.evt_rx.lock()?;
        match rx.recv_timeout(timeout) {
            Ok(evt) => Ok(Some(evt)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(DebugError::SessionClosed(
                "event receive failed: engine disconnected".into(),
            )),
        }
    }

    // ── Breakpoint management ───────────────────────
    //
    // Direct `Arc<Mutex<BreakpointRegistry>>` access — NOT through the
    // command channel.  This is intentional: DAP clients (e.g. VSCode)
    // send setBreakpoints at any time, including while the VM is
    // Running.  The command channel is only dispatched inside the
    // paused loop, so it cannot serve Running-state BP requests.
    //
    // The hook reads the same registry via `breakpoints.lock()` on the
    // VM thread.  Mutex serialises access — no race conditions.

    /// Set a breakpoint.  Returns the assigned ID.
    ///
    /// Works in **any** session state (Idle, Running, Paused,
    /// Terminated) — the breakpoint registry is shared via
    /// `Arc<Mutex<…>>` and accessed directly, not through the command
    /// channel.
    pub fn set_breakpoint(
        &self,
        source: &str,
        line: usize,
        condition: Option<&str>,
    ) -> Result<BreakpointId, DebugError> {
        let id =
            self.breakpoints
                .lock()?
                .add(source.to_string(), line, condition.map(String::from))?;
        self.has_active_breakpoints.store(true, Ordering::Release);
        Ok(id)
    }

    /// Remove a breakpoint by ID.
    pub fn remove_breakpoint(&self, id: BreakpointId) -> Result<bool, DebugError> {
        let mut reg = self.breakpoints.lock()?;
        let removed = reg.remove(id);
        if removed && reg.is_empty() {
            self.has_active_breakpoints.store(false, Ordering::Release);
        }
        Ok(removed)
    }

    /// List all breakpoints.
    pub fn list_breakpoints(&self) -> Result<Vec<Breakpoint>, DebugError> {
        Ok(self.breakpoints.lock()?.list())
    }

    // ── Execution control ───────────────────────────
    //
    // Resume commands send DebugCommand variants through the mpsc channel.
    // The commands are dispatched on the VM thread inside the paused
    // loop.  Sending while Running is harmless (the command queues
    // until the next pause) but has no immediate effect.
    //
    // `pause()` is the exception: it sets an atomic flag that the hook
    // checks on every line event, making it effective while Running.

    /// Resume execution.
    pub fn continue_execution(&self) -> Result<(), DebugError> {
        self.cmd_tx.send(DebugCommand::Continue)?;
        Ok(())
    }

    /// Step into the next line (descend into calls).
    pub fn step_into(&self) -> Result<(), DebugError> {
        self.cmd_tx.send(DebugCommand::StepInto)?;
        Ok(())
    }

    /// Step to the next line at the same or shallower call depth.
    pub fn step_over(&self) -> Result<(), DebugError> {
        self.cmd_tx.send(DebugCommand::StepOver)?;
        Ok(())
    }

    /// Step out of the current function.
    pub fn step_out(&self) -> Result<(), DebugError> {
        self.cmd_tx.send(DebugCommand::StepOut)?;
        Ok(())
    }

    /// Request the VM to pause at the next opportunity.
    ///
    /// Sets an atomic flag that is checked on every Lua line event.
    /// Effective in any state — if the VM is Running, it will pause
    /// at the next executed line.
    pub fn pause(&self) -> Result<(), DebugError> {
        self.pause_requested.store(true, Ordering::Release);
        Ok(())
    }

    // ── Inspection (only valid while paused) ────────
    //
    // These send a DebugCommand with a one-shot reply channel.
    // The engine executes the inspection on the **VM thread** (required
    // because lua_getlocal / lua_getupvalue are not thread-safe) and
    // sends the result back through the reply channel.

    /// Verify the session is in [`Paused`](SessionState::Paused) state.
    ///
    /// Returns [`DebugError::InvalidState`] if the VM is not paused,
    /// preventing callers from blocking indefinitely on a reply that
    /// would never arrive.
    fn ensure_paused(&self) -> Result<(), DebugError> {
        match self.state() {
            SessionState::Paused(_) => Ok(()),
            other => Err(DebugError::InvalidState(format!(
                "inspection requires Paused state, current: {other:?}"
            ))),
        }
    }

    /// Get the current call stack.
    pub fn get_stack_trace(&self) -> Result<Vec<StackFrame>, DebugError> {
        self.ensure_paused()?;
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(DebugCommand::GetStackTrace { reply: tx })?;
        rx.recv().map_err(|_| {
            DebugError::SessionClosed("stack trace reply lost: engine disconnected".into())
        })
    }

    /// Get local variables at a stack frame.
    pub fn get_locals(&self, frame_id: usize) -> Result<Vec<Variable>, DebugError> {
        self.ensure_paused()?;
        let (tx, rx) = mpsc::channel();
        self.cmd_tx.send(DebugCommand::GetLocals {
            frame_id,
            reply: tx,
        })?;
        rx.recv().map_err(|_| {
            DebugError::SessionClosed("get_locals reply lost: engine disconnected".into())
        })
    }

    /// Get upvalues at a stack frame.
    pub fn get_upvalues(&self, frame_id: usize) -> Result<Vec<Variable>, DebugError> {
        self.ensure_paused()?;
        let (tx, rx) = mpsc::channel();
        self.cmd_tx.send(DebugCommand::GetUpvalues {
            frame_id,
            reply: tx,
        })?;
        rx.recv().map_err(|_| {
            DebugError::SessionClosed("get_upvalues reply lost: engine disconnected".into())
        })
    }

    /// Evaluate a Lua expression while paused.
    ///
    /// When `frame_id` is `Some`, the expression is evaluated in the
    /// scope of that stack frame — locals, upvalues, and globals are
    /// all accessible.  When `None`, evaluation runs in the global
    /// scope only.
    pub fn evaluate(
        &self,
        expression: &str,
        frame_id: Option<usize>,
    ) -> Result<String, DebugError> {
        self.ensure_paused()?;
        let (tx, rx) = mpsc::channel();
        self.cmd_tx.send(DebugCommand::Evaluate {
            expression: expression.to_string(),
            frame_id,
            reply: tx,
        })?;
        rx.recv()
            .map_err(|_| {
                DebugError::SessionClosed("evaluate reply lost: engine disconnected".into())
            })?
            .map_err(DebugError::EvalError)
    }

    /// Disconnect from the session.
    pub fn disconnect(&self) -> Result<(), DebugError> {
        self.cmd_tx.send(DebugCommand::Disconnect)?;
        Ok(())
    }
}

// Compile-time guarantee that DebugController can be shared across threads.
#[allow(dead_code)]
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn check() {
        assert_send_sync::<DebugController>();
    }
};
