//! Debug session — the core coordinator.
//!
//! [`DebugSession`] attaches to a [`mlua::Lua`] instance and installs
//! a debug hook.  A [`DebugController`] is used by frontends to send
//! commands and receive events.
//!
//! # Error strategy
//!
//! Two distinct error types coexist because of mlua's callback constraint:
//!
//! | Context | Return type | Poison conversion |
//! |---------|-------------|-------------------|
//! | Hook callbacks (`set_hook`) | [`LuaResult`] | [`poison_to_lua`] → [`mlua::Error::runtime`] |
//! | Public API ([`DebugSession`]) | `Result<_, DebugError>` | `From<PoisonError>` → [`DebugError::Internal`] |
//!
//! **Why two paths?**  [`Lua::set_hook`](mlua::Lua::set_hook) requires
//! `Fn(&Lua, &Debug) -> Result<VmState>`, so hook internals must return
//! [`mlua::Error`], not [`DebugError`].  Public methods return
//! [`DebugError`] to keep frontends independent of mlua error types.
//!
//! `Error::runtime()` is chosen over `Error::external()` because a
//! poisoned mutex is unrecoverable — no caller will downcast it — and
//! `runtime()` avoids the `Arc<Box<dyn Error>>` indirection.
//!
//! # Mutex ordering
//!
//! All mutexes in [`SessionInner`] are acquired on the VM thread only
//! (inside the hook callback).  The lock ordering is:
//!
//! 1. `cmd_rx` (held for the duration of the paused loop)
//! 2. `step_mode` (short-lived)
//! 3. `breakpoints` (short-lived)
//!
//! `step_mode` and `breakpoints` are never held simultaneously.
//! `stop_on_entry` and `first_line_seen` are `AtomicBool` — lock-free.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use mlua::prelude::*;
use mlua::{HookTriggers, VmState};

use super::breakpoint::BreakpointRegistry;
use super::controller::DebugController;
use super::error::DebugError;
use super::inspector;
use super::source::SourceRegistry;
use super::stepping;
use super::types::{DebugCommand, DebugEvent, PauseReason, SessionState, StepMode};

/// Convert a [`PoisonError`](std::sync::PoisonError) into [`mlua::Error`]
/// for propagation from hook callbacks.
///
/// Used exclusively inside [`hook_callback`], [`enter_paused_loop`], and
/// [`dispatch_command`] where the return type is constrained to
/// [`LuaResult`] by [`Lua::set_hook`](mlua::Lua::set_hook).
///
/// See [module-level docs](self) for the rationale behind the two-path
/// error strategy.
fn poison_to_lua<T>(e: std::sync::PoisonError<T>) -> mlua::Error {
    mlua::Error::runtime(format!("mutex poisoned: {e}"))
}

/// A debug session attached to a single `Lua` instance.
///
/// Create with [`DebugSession::new`], which also returns a
/// [`DebugController`] for the frontend.
///
/// # Lifecycle
///
/// ```text
/// new() → Idle → attach() → Running ⇄ Paused → Terminated
/// ```
///
/// [`attach`](Self::attach) is a **one-shot** operation (per DAP
/// semantics).  Calling it on a non-Idle session returns an error.
/// To re-attach, create a new `DebugSession`.
pub struct DebugSession {
    inner: Arc<SessionInner>,
}

pub(crate) struct SessionInner {
    pub cmd_rx: Mutex<mpsc::Receiver<DebugCommand>>,
    /// Internal sender for `detach()` to unblock the paused loop.
    pub cmd_tx_internal: mpsc::Sender<DebugCommand>,
    pub evt_tx: mpsc::Sender<DebugEvent>,
    pub state: Arc<AtomicU8>,
    pub breakpoints: Arc<Mutex<BreakpointRegistry>>,
    pub step_mode: Mutex<Option<StepMode>>,
    pub call_depth: AtomicUsize,
    pub sources: Mutex<SourceRegistry>,
    /// Whether the engine should stop on the very first line.
    pub stop_on_entry: AtomicBool,
    /// Tracks whether we've seen the first line event.
    pub first_line_seen: AtomicBool,
    /// Set by [`DebugController::pause`] — checked on every line event.
    pub pause_requested: Arc<AtomicBool>,
    /// Fast-path flag: `true` when step mode is active.
    ///
    /// Updated by [`resume_execution`] (set) and [`enter_paused_loop`]
    /// (clear).  Avoids mutex lock on `step_mode` in the hot path.
    pub has_step_mode: AtomicBool,
    /// Fast-path flag: `true` when the breakpoint registry is non-empty.
    ///
    /// Shared with [`DebugController`](super::controller::DebugController)
    /// which updates the flag after add/remove operations.
    pub has_active_breakpoints: Arc<AtomicBool>,
    /// Re-entrancy guard: `true` while evaluating a breakpoint condition.
    ///
    /// Prevents the hook from firing again (via `lua_pcallk` inside the
    /// condition evaluation) and potentially deadlocking or pausing
    /// inside the condition code.
    pub evaluating_condition: AtomicBool,
}

impl DebugSession {
    /// Create a new debug session and its associated controller.
    ///
    /// The session is in `Idle` state until [`attach`](Self::attach) is
    /// called.
    pub fn new() -> (Self, DebugController) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let state = Arc::new(AtomicU8::new(SessionState::Idle.to_u8()));

        let breakpoints = Arc::new(Mutex::new(BreakpointRegistry::new()));
        let pause_requested = Arc::new(AtomicBool::new(false));
        let has_active_breakpoints = Arc::new(AtomicBool::new(false));

        let inner = Arc::new(SessionInner {
            cmd_rx: Mutex::new(cmd_rx),
            cmd_tx_internal: cmd_tx.clone(),
            evt_tx,
            state: state.clone(),
            breakpoints: breakpoints.clone(),
            step_mode: Mutex::new(None),
            call_depth: AtomicUsize::new(0),
            sources: Mutex::new(SourceRegistry::new()),
            stop_on_entry: AtomicBool::new(false),
            first_line_seen: AtomicBool::new(false),
            pause_requested: pause_requested.clone(),
            has_step_mode: AtomicBool::new(false),
            has_active_breakpoints: has_active_breakpoints.clone(),
            evaluating_condition: AtomicBool::new(false),
        });

        let controller = DebugController::new(
            cmd_tx,
            evt_rx,
            state,
            breakpoints,
            pause_requested,
            has_active_breakpoints,
        );

        (Self { inner }, controller)
    }

    /// Install the debug hook on the given `Lua` instance.
    ///
    /// After this call, breakpoints and stepping become active.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is not in [`SessionState::Idle`]
    /// (e.g. already attached or terminated).  Per DAP semantics,
    /// attach/launch is a one-shot operation per session.
    pub fn attach(&self, lua: &Lua) -> LuaResult<()> {
        // Atomically transition Idle → Running.  Reject if not Idle.
        let prev = self.inner.state.compare_exchange(
            SessionState::Idle.to_u8(),
            SessionState::Running.to_u8(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if prev.is_err() {
            return Err(mlua::Error::runtime(
                "attach failed: session is not Idle (already attached or terminated)",
            ));
        }

        let inner = self.inner.clone();
        let triggers = HookTriggers::EVERY_LINE | HookTriggers::ON_CALLS | HookTriggers::ON_RETURNS;

        lua.set_hook(triggers, move |lua, debug| {
            hook_callback(lua, debug, &inner)
        })?;

        Ok(())
    }

    /// Detach from the Lua instance and terminate the session.
    ///
    /// If the VM is paused (blocked in the command loop), this sends
    /// a disconnect command to unblock it.  A [`DebugEvent::Terminated`]
    /// event is emitted to notify the frontend.
    ///
    /// After detach, this session instance should be dropped.
    /// To debug again, create a new `DebugSession`.
    pub fn detach(&self, lua: &Lua) {
        // Terminate the session (idempotent — only sends event once).
        terminate(&self.inner, None, None);
        // If the VM thread is blocked in the paused loop, unblock it.
        let _ = self.inner.cmd_tx_internal.send(DebugCommand::Disconnect);
        // Remove the hook to prevent further invocations.
        lua.remove_hook();
    }

    /// Register source code so the engine can validate breakpoints
    /// and display source context.
    ///
    /// # Errors
    ///
    /// Returns [`DebugError::Internal`] if the source lock is poisoned
    /// (another thread panicked while holding it).
    pub fn register_source(&self, name: &str, content: &str) -> Result<(), DebugError> {
        self.inner.sources.lock()?.register(name, content);
        Ok(())
    }

    /// Set whether to pause on the first executable line.
    ///
    /// **Must be called before [`attach`](Self::attach)** to guarantee
    /// the setting takes effect before the first hook fires.
    pub fn set_stop_on_entry(&self, stop: bool) {
        self.inner.stop_on_entry.store(stop, Ordering::Release);
    }

    /// Create a [`CompletionNotifier`] for signaling execution completion.
    ///
    /// Move the returned handle into the thread that runs Lua code.
    /// Call [`CompletionNotifier::notify`] when execution finishes.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let notifier = session.completion_notifier();
    /// std::thread::spawn(move || {
    ///     let result = lua.load("...").exec();
    ///     notifier.notify(result.err().map(|e| e.to_string()));
    /// });
    /// ```
    pub fn completion_notifier(&self) -> CompletionNotifier {
        CompletionNotifier {
            inner: self.inner.clone(),
        }
    }
}

/// A handle for notifying the session that Lua execution has completed.
///
/// Created by [`DebugSession::completion_notifier`].  Move this into the
/// thread that runs Lua code so it can emit [`DebugEvent::Terminated`] when
/// execution finishes (successfully or with an error).
///
/// # Idempotent
///
/// If the session was already terminated (e.g. via
/// [`DebugController::disconnect`](super::controller::DebugController::disconnect)),
/// [`notify`](Self::notify) is a no-op — no duplicate event is sent.
pub struct CompletionNotifier {
    inner: Arc<SessionInner>,
}

impl CompletionNotifier {
    /// Signal that Lua execution has finished.
    ///
    /// Transitions to [`SessionState::Terminated`] and emits
    /// [`DebugEvent::Terminated`].  Pass `Some(message)` if execution
    /// failed; `None` for normal completion.
    ///
    /// **Note:** This method sets `result` to `None`.  Use
    /// [`notify_with_result`](Self::notify_with_result) to also report
    /// a successful return value.
    pub fn notify(self, error: Option<String>) {
        terminate(&self.inner, None, error);
    }

    /// Signal that Lua execution has finished, with an optional result.
    ///
    /// Like [`notify`](Self::notify), but additionally propagates a
    /// successful return value through [`DebugEvent::Terminated::result`].
    pub fn notify_with_result(self, result: Option<String>, error: Option<String>) {
        terminate(&self.inner, result, error);
    }
}

// Compile-time guarantee that CompletionNotifier can be moved to another thread.
#[allow(dead_code)]
const _: () = {
    fn assert_send<T: Send>() {}
    fn check() {
        assert_send::<CompletionNotifier>();
    }
};

// ─── Hook callback ──────────────────────────────────

fn hook_callback(lua: &Lua, debug: &mlua::Debug<'_>, inner: &SessionInner) -> LuaResult<VmState> {
    let event = debug.event();

    match event {
        mlua::DebugEvent::Call => {
            inner.call_depth.fetch_add(1, Ordering::Relaxed);
        }
        mlua::DebugEvent::TailCall => {
            // Tail call replaces the current frame — no new stack level.
            //
            // Lua 5.4 Reference Manual (lua_Hook):
            //   "for a tail call; in this case, there will be no
            //    corresponding return event."
            //
            // Because TAILCALL consumes the caller's frame and RET only
            // fires once (for the tail-called function), incrementing
            // here would permanently inflate call_depth.
            //
            // Example: `function a() return b() end; a()`
            //   CALL(a) depth 0→1 | TAILCALL(b) depth 1→1 | RET(b) depth 1→0
        }
        mlua::DebugEvent::Ret => {
            // Saturating sub to avoid underflow if Ret fires before
            // the matching Call (can happen for C functions).
            let _ = inner
                .call_depth
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| {
                    Some(d.saturating_sub(1))
                });
        }
        mlua::DebugEvent::Line => {
            // Skip processing if session has been terminated (e.g. after
            // Disconnect).  Without this guard, a subsequent breakpoint
            // hit would enter the paused loop and block forever because
            // no frontend is sending commands.
            if inner.state.load(Ordering::Acquire) == SessionState::Terminated.to_u8() {
                return Ok(VmState::Continue);
            }

            // Re-entrancy guard: skip the hook while evaluating a
            // breakpoint condition.  lua_pcallk inside evaluate_condition
            // fires line events for the condition code — without this
            // guard we'd deadlock or pause inside the condition.
            if inner.evaluating_condition.load(Ordering::Acquire) {
                return Ok(VmState::Continue);
            }

            // Check stop-on-entry (lock-free via AtomicBool).
            // compare_exchange: if first_line_seen is false, set to true
            // and check stop_on_entry.  Subsequent calls see true and skip.
            let stop_entry = inner
                .first_line_seen
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
                && inner.stop_on_entry.load(Ordering::Acquire);

            // Check pause request from the controller (atomic swap consumes the flag).
            let user_pause = inner.pause_requested.swap(false, Ordering::AcqRel);

            // Fast path: skip source lookup and mutex locks when nothing
            // to check.  All four conditions are atomic loads — no
            // contention on the hot path.
            let needs_check = stop_entry
                || user_pause
                || inner.has_step_mode.load(Ordering::Acquire)
                || inner.has_active_breakpoints.load(Ordering::Acquire);

            if needs_check {
                let line = debug.current_line().unwrap_or(0);
                let source = debug.source();
                let source_name = source.source.as_deref().unwrap_or("<unknown>");

                let call_depth = inner.call_depth.load(Ordering::Relaxed);
                let step_mode = inner.step_mode.lock().map_err(poison_to_lua)?.clone();

                // Step mode check (pure logic, no Lua interaction).
                let step_pauses = stepping::step_triggers(&step_mode, call_depth);

                // Breakpoint check with condition evaluation.
                // Extract ID + condition, then drop the lock before
                // evaluating (evaluation fires line events that
                // re-enter this hook).
                let bp_registry = inner.breakpoints.lock().map_err(poison_to_lua)?;
                let bp_info = bp_registry
                    .find(source_name, line)
                    .filter(|bp| bp.enabled)
                    .map(|bp| (bp.id, bp.condition.clone()));
                drop(bp_registry);

                let bp_hit_id = match bp_info {
                    Some((id, Some(cond))) => {
                        inner.evaluating_condition.store(true, Ordering::Release);
                        // SAFETY: We are on the VM thread inside
                        // the hook.  Level 0 = the function that
                        // triggered the line event.
                        let result = lua.exec_raw_lua(|raw| unsafe {
                            super::ffi::evaluate_condition(raw.state(), &cond, 0)
                        });
                        inner.evaluating_condition.store(false, Ordering::Release);
                        if result {
                            Some(id)
                        } else {
                            None
                        }
                    }
                    Some((id, None)) => Some(id),
                    None => None,
                };

                // Determine pause reason with priority:
                // Breakpoint > Step > UserPause > Entry.
                let reason = if let Some(id) = bp_hit_id {
                    Some(PauseReason::Breakpoint(id))
                } else if step_pauses {
                    Some(PauseReason::Step)
                } else if user_pause {
                    Some(PauseReason::UserPause)
                } else if stop_entry {
                    Some(PauseReason::Entry)
                } else {
                    None
                };

                if let Some(reason) = reason {
                    enter_paused_loop(lua, inner, reason)?;
                }
            }
        }
        _ => {}
    }

    Ok(VmState::Continue)
}

// ─── Paused loop ────────────────────────────────────

/// Set step mode, transition to `Running`, and emit `Continued`.
fn resume_execution(inner: &SessionInner, step: Option<StepMode>) -> LuaResult<()> {
    inner.has_step_mode.store(step.is_some(), Ordering::Release);
    if let Some(mode) = step {
        *inner.step_mode.lock().map_err(poison_to_lua)? = Some(mode);
    }
    inner
        .state
        .store(SessionState::Running.to_u8(), Ordering::Release);
    let _ = inner.evt_tx.send(DebugEvent::Continued);
    Ok(())
}

/// Transition to `Terminated` and emit the event.
///
/// Idempotent: if the session is already `Terminated`, the event is
/// not sent again (prevents duplicate `Terminated` events when
/// `detach()` races with `Disconnect`).
fn terminate(inner: &SessionInner, result: Option<String>, error: Option<String>) {
    let prev = inner
        .state
        .swap(SessionState::Terminated.to_u8(), Ordering::AcqRel);
    if prev != SessionState::Terminated.to_u8() {
        let _ = inner.evt_tx.send(DebugEvent::Terminated { result, error });
    }
}

fn enter_paused_loop(lua: &Lua, inner: &SessionInner, reason: PauseReason) -> LuaResult<()> {
    // Clear step mode when we actually pause.
    *inner.step_mode.lock().map_err(poison_to_lua)? = None;
    inner.has_step_mode.store(false, Ordering::Release);

    // Update state.
    inner.state.store(
        SessionState::Paused(reason.clone()).to_u8(),
        Ordering::Release,
    );

    // Collect stack trace for the event.
    let stack = inspector::collect_stack_trace(lua, 0);

    // Notify frontend.
    let _ = inner.evt_tx.send(DebugEvent::Paused { reason, stack });

    // Command dispatch loop — blocks the VM thread.
    let cmd_rx = inner.cmd_rx.lock().map_err(poison_to_lua)?;
    loop {
        match cmd_rx.recv() {
            Ok(cmd) => {
                if dispatch_command(lua, inner, cmd)? {
                    break;
                }
            }
            Err(_) => {
                terminate(inner, None, Some("frontend disconnected".into()));
                break;
            }
        }
    }

    Ok(())
}

/// Handle a single command inside the paused loop.
///
/// Returns `Ok(true)` for resume/disconnect commands (caller breaks the loop).
/// Returns `Ok(false)` for inspection/management commands (loop continues).
fn dispatch_command(lua: &Lua, inner: &SessionInner, cmd: DebugCommand) -> LuaResult<bool> {
    match cmd {
        // ── Resume commands ──
        DebugCommand::Continue => {
            resume_execution(inner, None)?;
        }
        DebugCommand::StepInto => {
            resume_execution(inner, Some(StepMode::Into))?;
        }
        DebugCommand::StepOver => {
            let depth = inner.call_depth.load(Ordering::Relaxed);
            resume_execution(inner, Some(StepMode::Over { start_depth: depth }))?;
        }
        DebugCommand::StepOut => {
            let depth = inner.call_depth.load(Ordering::Relaxed);
            resume_execution(inner, Some(StepMode::Out { start_depth: depth }))?;
        }

        // ── Inspection commands (VM thread) ──
        DebugCommand::GetStackTrace { reply } => {
            let _ = reply.send(inspector::collect_stack_trace(lua, 0));
            return Ok(false);
        }
        DebugCommand::GetLocals { frame_id, reply } => {
            let _ = reply.send(inspector::inspect_locals(lua, frame_id));
            return Ok(false);
        }
        DebugCommand::GetUpvalues { frame_id, reply } => {
            let _ = reply.send(inspector::inspect_upvalues(lua, frame_id));
            return Ok(false);
        }
        DebugCommand::Evaluate {
            expression,
            frame_id,
            reply,
        } => {
            let _ = reply.send(inspector::evaluate_expression(lua, &expression, frame_id));
            return Ok(false);
        }

        // ── Session ──
        DebugCommand::Disconnect => {
            terminate(inner, None, None);
        }
    }

    Ok(true)
}
