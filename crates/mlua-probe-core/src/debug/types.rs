//! Shared types for the debug engine.

use std::fmt;
use std::sync::mpsc;

// ─── Breakpoint ID ─────────────────────────────────

/// Unique identifier for a breakpoint.
///
/// Opaque handle — external consumers can compare and store IDs but
/// cannot construct or inspect the inner value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BreakpointId(pub(crate) u32);

impl BreakpointId {
    /// The first ID assigned to a breakpoint.
    pub(crate) const FIRST: Self = Self(1);

    /// Return the next sequential ID, or `None` on overflow.
    pub(crate) fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }

    /// Reconstruct a `BreakpointId` from its numeric value.
    ///
    /// Used by frontends (MCP server, etc.) that persist IDs across
    /// serialization boundaries.
    ///
    /// Returns `None` if `id` is 0, since the engine assigns IDs
    /// starting from 1.  An ID of 0 is never produced by the
    /// breakpoint registry.
    pub fn from_raw(id: u32) -> Option<Self> {
        if id == 0 {
            return None;
        }
        Some(Self(id))
    }

    /// Get the raw numeric value.
    pub fn as_raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for BreakpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── Session state ──────────────────────────────────

/// State of a debug session.
///
/// Stored as an [`AtomicU8`](std::sync::atomic::AtomicU8) for
/// lock-free cross-thread reads.  Only the **discriminant** is
/// preserved — [`Paused`](Self::Paused) loses its [`PauseReason`]
/// in the atomic representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// No active debugging. Hooks are not installed.
    Idle,
    /// Lua code is executing with hooks active.
    Running,
    /// VM is paused. The VM thread is blocked inside the hook callback.
    Paused(PauseReason),
    /// Execution finished or the session was disconnected.
    Terminated,
}

impl SessionState {
    /// Encode to a `u8` for atomic storage.
    pub(crate) fn to_u8(&self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Running => 1,
            Self::Paused(_) => 2,
            Self::Terminated => 3,
        }
    }

    /// Decode from a `u8` stored in the shared [`AtomicU8`](std::sync::atomic::AtomicU8).
    ///
    /// The atomic representation stores only the discriminant (Idle /
    /// Running / Paused / Terminated).  **`PauseReason` is not
    /// preserved** — `Paused` always decodes as `PauseReason::Step`.
    ///
    /// This is intentional: per DAP semantics, the precise pause
    /// reason is delivered via [`DebugEvent::Paused`] and should be
    /// consumed from the event stream, not polled from `state()`.
    /// [`DebugController::state`](super::controller::DebugController::state)
    /// exists only for coarse-grained "is the VM running?" checks.
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::Running,
            2 => Self::Paused(PauseReason::Step),
            3 => Self::Terminated,
            _ => unreachable!("invalid SessionState discriminant: {v} (only 0..=3 are valid)"),
        }
    }
}

// ─── Pause reason ───────────────────────────────────

/// Why the VM was paused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseReason {
    /// Hit a breakpoint.
    Breakpoint(BreakpointId),
    /// Completed a step operation.
    Step,
    /// User requested pause.
    UserPause,
    /// Lua runtime error while executing.
    Error(String),
    /// Paused on first executable line (`stop_on_entry`).
    Entry,
}

// ─── Step mode ──────────────────────────────────────

/// How the stepping engine should behave after a resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepMode {
    /// Stop at the very next line (descend into function calls).
    Into,
    /// Stop at the next line at the same or shallower call depth.
    Over { start_depth: usize },
    /// Stop after returning from the current function.
    Out { start_depth: usize },
}

// ─── Stack / Variable inspection ────────────────────

/// A single frame in the Lua call stack.
#[derive(Debug, Clone)]
pub struct StackFrame {
    /// Stack level (0 = top of stack / currently executing).
    pub id: usize,
    /// Function name, or `"<main>"` for the top-level chunk.
    pub name: String,
    /// Source identifier (e.g. `"@main.lua"`).
    pub source: String,
    /// Current line number in that source (1-based).
    ///
    /// `None` when line information is unavailable (e.g. C frames
    /// or chunks without debug info).
    pub line: Option<usize>,
    /// Kind of frame.
    pub what: FrameKind,
}

/// What produced this stack frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameKind {
    Lua,
    C,
    Main,
}

/// A single variable (local, upvalue, or global).
#[derive(Debug, Clone)]
pub struct Variable {
    /// Variable name.
    pub name: String,
    /// Human-readable representation of the value.
    pub value: String,
    /// Lua type name (`"number"`, `"string"`, `"table"`, …).
    pub type_name: String,
    /// If the value is a table, an opaque reference for lazy expansion.
    pub children_ref: Option<VariableRef>,
}

/// Opaque handle used to expand structured values (tables) on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VariableRef(pub(crate) u32);

// ─── Commands (frontend → engine) ───────────────────

/// A command sent from the frontend to the debug engine.
///
/// Commands fall into three categories:
///
/// - **Resume** (`Continue`, `StepInto`, …) — unblock the VM thread.
/// - **Inspect** (`GetStackTrace`, `GetLocals`, …) — query state while
///   paused; the response is sent through the `reply` channel.
/// - **Manage** (`Disconnect`, …) — configure the session.
pub(crate) enum DebugCommand {
    // ── resume ──
    Continue,
    StepInto,
    StepOver,
    StepOut,

    // ── inspection (valid only while paused) ──
    GetStackTrace {
        reply: mpsc::Sender<Vec<StackFrame>>,
    },
    GetLocals {
        frame_id: usize,
        reply: mpsc::Sender<Vec<Variable>>,
    },
    GetUpvalues {
        frame_id: usize,
        reply: mpsc::Sender<Vec<Variable>>,
    },
    Evaluate {
        expression: String,
        frame_id: Option<usize>,
        reply: mpsc::Sender<Result<String, String>>,
    },

    // ── session ──
    Disconnect,
}

// ─── Events (engine → frontend) ─────────────────────

/// An event emitted by the debug engine.
///
/// Events are the **authoritative** data source for session state
/// transitions.  [`DebugController::state`](super::controller::DebugController::state)
/// provides a coarse-grained snapshot, but details like
/// [`PauseReason`] are only available through events.
///
/// Consume via [`DebugController::wait_event`](super::controller::DebugController::wait_event)
/// or [`try_event`](super::controller::DebugController::try_event).
#[derive(Debug, Clone)]
pub enum DebugEvent {
    /// The VM has paused.  Carries the precise [`PauseReason`] and
    /// the call stack at the point of suspension.
    Paused {
        reason: PauseReason,
        stack: Vec<StackFrame>,
    },
    /// The VM has resumed execution.
    Continued,
    /// Execution has completed (or errored).
    Terminated {
        result: Option<String>,
        error: Option<String>,
    },
    /// Output produced during execution (print, logpoint, …).
    Output {
        category: OutputCategory,
        text: String,
        source: Option<String>,
        line: Option<usize>,
    },
}

/// Category for [`DebugEvent::Output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputCategory {
    Console,
    Stdout,
    Stderr,
    LogPoint,
}
