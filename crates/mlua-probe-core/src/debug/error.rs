//! Error types for the debug engine.
//!
//! [`DebugError`] is the public error type for [`DebugController`] and
//! [`DebugSession`] APIs.  It deliberately does **not** expose
//! [`mlua::Error`] — frontends should remain independent of the Lua
//! binding layer.
//!
//! Hook-internal errors use [`mlua::Error`] instead (see
//! [`engine`](super::engine) module docs for the two-path rationale).
//!
//! [`DebugController`]: super::controller::DebugController
//! [`DebugSession`]: super::engine::DebugSession

use thiserror::Error;

/// Errors returned by [`DebugController`](super::controller::DebugController)
/// and [`DebugSession`](super::engine::DebugSession) methods.
#[derive(Debug, Error)]
pub enum DebugError {
    /// The debug session has been closed (channel disconnected).
    #[error("debug session closed: {0}")]
    SessionClosed(String),
    /// The command is not valid in the current session state.
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// Expression evaluation failed.
    #[error("eval error: {0}")]
    EvalError(String),
    /// An internal error (e.g. mutex poisoned).  Indicates a bug or
    /// a panic on another thread — the session is no longer usable.
    #[error("internal error: {0}")]
    Internal(String),
}

impl<T> From<std::sync::mpsc::SendError<T>> for DebugError {
    fn from(_: std::sync::mpsc::SendError<T>) -> Self {
        Self::SessionClosed("command send failed: engine disconnected".into())
    }
}

impl From<std::sync::mpsc::RecvError> for DebugError {
    fn from(_: std::sync::mpsc::RecvError) -> Self {
        Self::SessionClosed("event receive failed: engine disconnected".into())
    }
}

/// A poisoned mutex means another thread panicked while holding the
/// lock.  The session is unrecoverable — map to [`DebugError::Internal`].
impl<T> From<std::sync::PoisonError<T>> for DebugError {
    fn from(e: std::sync::PoisonError<T>) -> Self {
        Self::Internal(format!("mutex poisoned: {e}"))
    }
}
