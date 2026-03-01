use std::path::Path;
use std::sync::Arc;
use std::thread;

use mlua::prelude::*;
use mlua_probe_core::{
    testing, BreakpointId, DebugController, DebugEvent, DebugSession, PauseReason, SessionState,
    StackFrame, Variable,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

// ─── Parameter types ──────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LaunchParams {
    /// Lua source code to execute (inline). Provide either `code` or
    /// `code_file`, not both.
    pub code: Option<String>,
    /// Path to a Lua source file to execute. Provide either `code` or
    /// `code_file`, not both. When `chunk_name` is omitted, it is
    /// derived from the filename (e.g. "script.lua" → "@script.lua").
    pub code_file: Option<String>,
    /// Chunk name (default: "@main.lua"). Used to identify the source
    /// in breakpoints and stack traces.
    pub chunk_name: Option<String>,
    /// Pause on the first executable line (default: true). Set to true
    /// so you can set breakpoints before code runs.
    pub stop_on_entry: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetBreakpointParams {
    /// Source identifier matching the chunk name (e.g. "@main.lua").
    pub source: String,
    /// 1-based line number.
    pub line: usize,
    /// Optional condition expression — fires only when it evaluates to true.
    pub condition: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RemoveBreakpointParams {
    /// Breakpoint ID (returned by set_breakpoint).
    pub id: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FrameParams {
    /// Stack frame ID (0 = top of stack).
    pub frame_id: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvaluateParams {
    /// Lua expression to evaluate.
    pub expression: String,
    /// Stack frame for evaluation context (None = global scope).
    pub frame_id: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TestLaunchParams {
    /// Lua test code using the mlua-lspec framework (describe/it/expect).
    /// The `lust` global is pre-loaded automatically.
    /// Provide either `code` or `code_file`, not both.
    pub code: Option<String>,
    /// Path to a Lua test file. Provide either `code` or `code_file`,
    /// not both. When `chunk_name` is omitted, it is derived from the
    /// filename.
    pub code_file: Option<String>,
    /// Chunk name for error messages (default: "@test.lua").
    pub chunk_name: Option<String>,
}

// ─── Code source resolution ──────────────────────────────────────

/// Resolved code source: the Lua source text and an inferred chunk name.
struct ResolvedCode {
    code: String,
    inferred_chunk_name: Option<String>,
}

/// Resolve Lua source from either inline `code` or a `code_file` path.
///
/// Exactly one of the two must be provided.  When `code_file` is used,
/// the chunk name is inferred from the filename (e.g. `"script.lua"` →
/// `"@script.lua"`).
fn resolve_code(code: Option<String>, code_file: Option<String>) -> Result<ResolvedCode, String> {
    match (code, code_file) {
        (Some(c), None) => Ok(ResolvedCode {
            code: c,
            inferred_chunk_name: None,
        }),
        (None, Some(path)) => {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read {path}: {e}"))?;
            let file_name = Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file.lua");
            Ok(ResolvedCode {
                code: content,
                inferred_chunk_name: Some(format!("@{file_name}")),
            })
        }
        (Some(_), Some(_)) => Err("Provide either `code` or `code_file`, not both.".to_string()),
        (None, None) => Err("Either `code` or `code_file` must be provided.".to_string()),
    }
}

// ─── Formatting helpers ───────────────────────────────────────────

fn format_stack_frames(frames: &[StackFrame]) -> serde_json::Value {
    frames
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "name": f.name,
                "source": f.source,
                "line": f.line,
                "kind": format!("{:?}", f.what),
            })
        })
        .collect()
}

fn format_variables(vars: &[Variable]) -> serde_json::Value {
    vars.iter()
        .map(|v| {
            json!({
                "name": v.name,
                "value": v.value,
                "type": v.type_name,
            })
        })
        .collect()
}

fn format_event(event: &DebugEvent) -> serde_json::Value {
    match event {
        DebugEvent::Paused { reason, stack } => {
            let reason_json = match reason {
                PauseReason::Breakpoint(id) => {
                    json!({"type": "breakpoint", "breakpoint_id": id.as_raw()})
                }
                PauseReason::Step => json!({"type": "step"}),
                PauseReason::UserPause => json!({"type": "user_pause"}),
                PauseReason::Error(msg) => json!({"type": "error", "message": msg}),
                PauseReason::Entry => json!({"type": "entry"}),
            };
            json!({
                "event": "paused",
                "reason": reason_json,
                "stack": format_stack_frames(stack),
            })
        }
        DebugEvent::Continued => json!({"event": "continued"}),
        DebugEvent::Terminated { result, error } => json!({
            "event": "terminated",
            "result": result,
            "error": error,
        }),
        DebugEvent::Output {
            category,
            text,
            source,
            line,
        } => json!({
            "event": "output",
            "category": format!("{category:?}"),
            "text": text,
            "source": source,
            "line": line,
        }),
    }
}

fn format_state(state: &SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::Running => "running",
        SessionState::Paused(_) => "paused",
        SessionState::Terminated => "terminated",
    }
}

// ─── MCP Handler ──────────────────────────────────────────────────

/// Holds all resources for a running debug session.
///
/// Dropped when the session is disconnected.  The VM thread continues
/// running independently (its `JoinHandle` is detached on drop) but
/// hooks are removed and no further events are emitted.
struct ActiveSession {
    controller: DebugController,
    session: DebugSession,
    lua: Arc<Lua>,
    /// Joined on disconnect to detect VM thread panics.
    vm_thread: thread::JoinHandle<()>,
}

#[derive(Clone)]
pub struct DebugMcpHandler {
    tool_router: ToolRouter<Self>,
    active: Arc<Mutex<Option<ActiveSession>>>,
}

/// Clone the controller from the active session, or return a tool-level error.
macro_rules! get_ctrl {
    ($self:expr) => {{
        let guard = $self.active.lock().await;
        match guard.as_ref() {
            Some(s) => s.controller.clone(),
            None => {
                return Err("No active debug session. Call debug_launch first.".to_string());
            }
        }
    }};
}

#[tool_router]
impl DebugMcpHandler {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            active: Arc::new(Mutex::new(None)),
        }
    }

    // ── Session lifecycle ──────────────────────────────────────

    /// Launch a Lua debug session. Creates a new Lua VM, attaches the
    /// debugger, and runs the code on a background thread.
    ///
    /// Typical workflow:
    /// 1. debug_launch(code, stop_on_entry=true)
    /// 2. wait_event → receives "paused" (entry)
    /// 3. set_breakpoint(source, line)
    /// 4. continue_execution
    /// 5. wait_event → receives "paused" (breakpoint)
    /// 6. get_locals / get_stack_trace / evaluate
    /// 7. step_into / step_over / continue_execution
    /// 8. disconnect when done
    #[tool(name = "debug_launch")]
    async fn debug_launch(
        &self,
        Parameters(params): Parameters<LaunchParams>,
    ) -> Result<String, String> {
        let mut guard = self.active.lock().await;
        if guard.is_some() {
            return Err("A debug session is already active. Call disconnect first.".to_string());
        }

        let resolved = resolve_code(params.code, params.code_file)?;
        let chunk_name = params
            .chunk_name
            .or(resolved.inferred_chunk_name)
            .unwrap_or_else(|| "@main.lua".to_string());
        let stop_on_entry = params.stop_on_entry.unwrap_or(true);
        let code = resolved.code;

        let lua = Arc::new(Lua::new());
        let (session, controller) = DebugSession::new();
        session.set_stop_on_entry(stop_on_entry);

        session
            .register_source(&chunk_name, &code)
            .map_err(|e| format!("Failed to register source: {e}"))?;
        session
            .attach(&lua)
            .map_err(|e| format!("Failed to attach debugger: {e}"))?;

        let notifier = session.completion_notifier();
        let lua_clone = lua.clone();
        let name_clone = chunk_name.clone();
        let vm_thread = thread::spawn(move || {
            let result = lua_clone.load(&code).set_name(&name_clone).exec();
            notifier.notify(result.err().map(|e| e.to_string()));
        });

        *guard = Some(ActiveSession {
            controller,
            session,
            lua,
            vm_thread,
        });

        if stop_on_entry {
            Ok(format!(
                "Debug session started (chunk: {chunk_name}, stop_on_entry: true). \
                 Call wait_event to receive the initial pause."
            ))
        } else {
            Ok(format!(
                "Debug session started (chunk: {chunk_name}). Code is running."
            ))
        }
    }

    /// End the debug session.  Detaches the debugger (terminates the
    /// session, removes hooks, unblocks the VM if paused).  The Lua VM
    /// thread finishes its current execution independently.
    #[tool(name = "disconnect")]
    async fn disconnect(&self) -> Result<String, String> {
        let mut guard = self.active.lock().await;
        let active = guard.take().ok_or("No active debug session.")?;

        // Detach: set Terminated state, send Disconnect to unblock the
        // paused loop, and remove the hook so no further callbacks fire.
        active.session.detach(&active.lua);

        // Join the VM thread in the background to detect panics.
        // The thread finishes when Lua execution completes (which may
        // take time for long-running scripts — we don't block on it).
        let vm_thread = active.vm_thread;
        tokio::task::spawn(async move {
            if let Ok(Err(_)) = tokio::task::spawn_blocking(move || vm_thread.join()).await {
                tracing::warn!("VM thread panicked during session teardown");
            }
        });

        Ok("Session disconnected.".to_string())
    }

    /// Get the current session state: idle, running, paused, or terminated.
    #[tool(name = "get_state")]
    async fn get_state(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let state = ctrl.state();
        Ok(json!({"state": format_state(&state)}).to_string())
    }

    // ── Breakpoint management ──────────────────────────────────

    /// Set a breakpoint at a source:line location. Works in any session
    /// state (running or paused). Returns the assigned breakpoint ID.
    #[tool(name = "set_breakpoint")]
    async fn set_breakpoint(
        &self,
        Parameters(params): Parameters<SetBreakpointParams>,
    ) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let id = ctrl
            .set_breakpoint(&params.source, params.line, params.condition.as_deref())
            .map_err(|e| format!("Failed to set breakpoint: {e}"))?;
        Ok(json!({
            "breakpoint_id": id.as_raw(),
            "source": params.source,
            "line": params.line,
        })
        .to_string())
    }

    /// Remove a breakpoint by its ID.
    #[tool(name = "remove_breakpoint")]
    async fn remove_breakpoint(
        &self,
        Parameters(params): Parameters<RemoveBreakpointParams>,
    ) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let id = BreakpointId::from_raw(params.id)
            .ok_or_else(|| format!("Invalid breakpoint ID: {} (must be > 0)", params.id))?;
        let removed = ctrl
            .remove_breakpoint(id)
            .map_err(|e| format!("Failed to remove breakpoint: {e}"))?;
        if removed {
            Ok(format!("Breakpoint {} removed.", params.id))
        } else {
            Err(format!("Breakpoint {} not found.", params.id))
        }
    }

    /// List all breakpoints in the current session.
    #[tool(name = "list_breakpoints")]
    async fn list_breakpoints(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let bps = ctrl
            .list_breakpoints()
            .map_err(|e| format!("Failed to list breakpoints: {e}"))?;
        let list: Vec<_> = bps
            .iter()
            .map(|bp| {
                json!({
                    "id": bp.id.as_raw(),
                    "source": &*bp.source,
                    "line": bp.line,
                    "condition": bp.condition,
                    "enabled": bp.enabled,
                })
            })
            .collect();
        Ok(json!({"breakpoints": list}).to_string())
    }

    // ── Execution control ──────────────────────────────────────

    /// Resume execution after a pause.
    #[tool(name = "continue_execution")]
    async fn continue_execution(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        ctrl.continue_execution()
            .map_err(|e| format!("Continue failed: {e}"))?;
        Ok("Resumed. Call wait_event for the next pause.".to_string())
    }

    /// Step into the next line (descends into function calls).
    #[tool(name = "step_into")]
    async fn step_into(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        ctrl.step_into()
            .map_err(|e| format!("Step into failed: {e}"))?;
        Ok("Stepping into. Call wait_event for the next pause.".to_string())
    }

    /// Step to the next line at the same or shallower call depth.
    #[tool(name = "step_over")]
    async fn step_over(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        ctrl.step_over()
            .map_err(|e| format!("Step over failed: {e}"))?;
        Ok("Stepping over. Call wait_event for the next pause.".to_string())
    }

    /// Step out of the current function.
    #[tool(name = "step_out")]
    async fn step_out(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        ctrl.step_out()
            .map_err(|e| format!("Step out failed: {e}"))?;
        Ok("Stepping out. Call wait_event for the next pause.".to_string())
    }

    /// Request the VM to pause at the next opportunity.
    /// Use when the VM is running and you want to interrupt.
    #[tool(name = "pause")]
    async fn pause_execution(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        ctrl.pause().map_err(|e| format!("Pause failed: {e}"))?;
        Ok("Pause requested. Call wait_event.".to_string())
    }

    // ── Events ─────────────────────────────────────────────────

    /// Block until the next debug event arrives (paused, continued,
    /// terminated, or output). Call this after launching or resuming
    /// to learn when the VM pauses or terminates.
    #[tool(name = "wait_event")]
    async fn wait_event(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let result = tokio::task::spawn_blocking(move || ctrl.wait_event())
            .await
            .map_err(|e| format!("Internal error: {e}"))?;
        match result {
            Ok(event) => Ok(format_event(&event).to_string()),
            Err(e) => Err(format!("Event wait failed (session may be closed): {e}")),
        }
    }

    // ── Inspection (valid while paused) ────────────────────────

    /// Get the current call stack. Only meaningful while paused.
    #[tool(name = "get_stack_trace")]
    async fn get_stack_trace(&self) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let result = tokio::task::spawn_blocking(move || ctrl.get_stack_trace())
            .await
            .map_err(|e| format!("Internal error: {e}"))?;
        match result {
            Ok(frames) => Ok(json!({"frames": format_stack_frames(&frames)}).to_string()),
            Err(e) => Err(format!("Failed to get stack trace: {e}")),
        }
    }

    /// Get local variables at a stack frame. Only meaningful while paused.
    #[tool(name = "get_locals")]
    async fn get_locals(
        &self,
        Parameters(params): Parameters<FrameParams>,
    ) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let frame_id = params.frame_id;
        let result = tokio::task::spawn_blocking(move || ctrl.get_locals(frame_id))
            .await
            .map_err(|e| format!("Internal error: {e}"))?;
        match result {
            Ok(vars) => Ok(json!({"locals": format_variables(&vars)}).to_string()),
            Err(e) => Err(format!("Failed to get locals: {e}")),
        }
    }

    /// Get upvalues (captured variables) at a stack frame.
    /// Only meaningful while paused.
    #[tool(name = "get_upvalues")]
    async fn get_upvalues(
        &self,
        Parameters(params): Parameters<FrameParams>,
    ) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let frame_id = params.frame_id;
        let result = tokio::task::spawn_blocking(move || ctrl.get_upvalues(frame_id))
            .await
            .map_err(|e| format!("Internal error: {e}"))?;
        match result {
            Ok(vars) => Ok(json!({"upvalues": format_variables(&vars)}).to_string()),
            Err(e) => Err(format!("Failed to get upvalues: {e}")),
        }
    }

    /// Evaluate a Lua expression while paused. Returns the result as
    /// a string. When `frame_id` is provided, locals and upvalues of
    /// that stack frame are accessible in the expression. When omitted,
    /// evaluation runs in the global scope.
    #[tool(name = "evaluate")]
    async fn evaluate(
        &self,
        Parameters(params): Parameters<EvaluateParams>,
    ) -> Result<String, String> {
        let ctrl = get_ctrl!(self);
        let expr = params.expression;
        let frame = params.frame_id;
        let result = tokio::task::spawn_blocking(move || ctrl.evaluate(&expr, frame))
            .await
            .map_err(|e| format!("Internal error: {e}"))?;
        match result {
            Ok(val) => Ok(json!({"result": val}).to_string()),
            Err(e) => Err(format!("Evaluation failed: {e}")),
        }
    }

    // ── Testing ─────────────────────────────────────────────────

    /// Run Lua test code with the mlua-lspec test framework pre-loaded.
    /// Returns structured test results (passed/failed counts and
    /// per-test details).
    ///
    /// The `lust` global is available automatically. Use describe/it/expect:
    /// ```lua
    /// local describe, it, expect = lust.describe, lust.it, lust.expect
    /// describe('math', function()
    ///   it('adds numbers', function()
    ///     expect(1 + 1).to.equal(2)
    ///   end)
    /// end)
    /// ```
    #[tool(name = "test_launch")]
    async fn test_launch(
        &self,
        Parameters(params): Parameters<TestLaunchParams>,
    ) -> Result<String, String> {
        let resolved = resolve_code(params.code, params.code_file)?;
        let code = resolved.code;
        let chunk_name = params
            .chunk_name
            .or(resolved.inferred_chunk_name)
            .unwrap_or_else(|| "@test.lua".to_string());

        let result =
            tokio::task::spawn_blocking(move || testing::framework::run_tests(&code, &chunk_name))
                .await
                .map_err(|e| format!("Internal error: {e}"))?;

        match result {
            Ok(summary) => {
                let tests: Vec<serde_json::Value> = summary
                    .tests
                    .iter()
                    .map(|t| {
                        json!({
                            "suite": t.suite,
                            "name": t.name,
                            "passed": t.passed,
                            "error": t.error,
                        })
                    })
                    .collect();

                Ok(json!({
                    "passed": summary.passed,
                    "failed": summary.failed,
                    "total": summary.total,
                    "tests": tests,
                })
                .to_string())
            }
            Err(e) => Err(e),
        }
    }
}

#[tool_handler]
impl ServerHandler for DebugMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Lua debugger and test runner (mlua-probe) for environments without DAP support. \
                 Provides breakpoints, stepping, variable inspection, expression \
                 evaluation, and a built-in test framework for Lua code running in an mlua VM.\n\n\
                 Debugging: Start with debug_launch, then use wait_event to receive pause events.\n\n\
                 Testing: Use test_launch to run Lua tests with the mlua-lspec framework \
                 (describe/it/expect/spy). Returns structured JSON results.\n\n\
                 SECURITY: debug_launch, evaluate, and test_launch execute arbitrary Lua code \
                 with full standard library access (including os and io modules). \
                 Only use with trusted input."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
