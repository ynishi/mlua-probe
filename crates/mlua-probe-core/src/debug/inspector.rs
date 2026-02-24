//! High-level inspection functions built on top of the FFI layer.
//!
//! These are called from the paused loop on the VM thread.
//!
//! # mlua internal API dependency
//!
//! This module uses [`Lua::exec_raw_lua`] to access the raw `lua_State`
//! pointer for FFI calls to `lua_getlocal` / `lua_getupvalue`.  These
//! operations have no safe wrapper in mlua.
//!
//! `exec_raw_lua` is available in mlua 0.10+.  The workspace pins
//! `mlua = "0.11"` to limit exposure to breaking changes.  If mlua
//! removes or renames this API, update both [`inspect_locals`] and
//! [`inspect_upvalues`].

use std::os::raw::c_int;

use mlua::prelude::*;

use super::ffi;
use super::types::{FrameKind, StackFrame, Variable};

/// Collect the full call stack starting from `start_level`.
///
/// Level 0 is the top of the stack (currently executing frame).
pub(crate) fn collect_stack_trace(lua: &Lua, start_level: usize) -> Vec<StackFrame> {
    let mut frames = Vec::new();

    for level in start_level.. {
        let frame = lua.inspect_stack(level, |debug| {
            let source = debug.source();
            let names = debug.names();

            let name = names
                .name
                .as_deref()
                .unwrap_or(match source.what {
                    "main" => "<main>",
                    "C" => "<C>",
                    _ => "<anonymous>",
                })
                .to_string();

            let source_name = source.source.as_deref().unwrap_or("<unknown>").to_string();

            let what = match source.what {
                "Lua" => FrameKind::Lua,
                "C" => FrameKind::C,
                "main" => FrameKind::Main,
                other => {
                    debug_assert!(false, "unknown Lua frame kind: {other}");
                    FrameKind::Lua
                }
            };

            StackFrame {
                id: level,
                name,
                source: source_name,
                line: debug.current_line(),
                what,
            }
        });

        match frame {
            Some(f) => frames.push(f),
            None => break,
        }
    }

    frames
}

/// Inspect local variables at the given stack frame.
pub(crate) fn inspect_locals(lua: &Lua, frame_id: usize) -> Vec<Variable> {
    let level: c_int = match c_int::try_from(frame_id) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    // SAFETY: Called from the paused loop on the VM thread.
    // exec_raw_lua holds mlua's internal lock, ensuring exclusive
    // access to the lua_State for the duration of the FFI call.
    lua.exec_raw_lua(|raw| unsafe { ffi::get_locals(raw.state(), level) })
}

/// Inspect upvalues at the given stack frame.
pub(crate) fn inspect_upvalues(lua: &Lua, frame_id: usize) -> Vec<Variable> {
    let level: c_int = match c_int::try_from(frame_id) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    // SAFETY: Called from the paused loop on the VM thread.
    // exec_raw_lua holds mlua's internal lock, ensuring exclusive
    // access to the lua_State for the duration of the FFI call.
    lua.exec_raw_lua(|raw| unsafe { ffi::get_upvalues(raw.state(), level) })
}

/// Evaluate a Lua expression and return its string representation.
///
/// # Current limitation (Phase 1)
///
/// `frame_id` is accepted but **not yet used** — evaluation always runs
/// in the global scope.  This means local variables and upvalues of the
/// paused frame are **not** accessible in the expression.
///
/// # Phase 2: frame-scoped evaluation (DAP-compliant)
///
/// The [DAP specification][dap-eval] requires:
///
/// > *"Evaluate the expression in the scope of this stack frame.
/// > If not specified, the expression is evaluated in the global scope."*
///
/// The standard Lua technique (see [debugger.lua][dbg-lua]) is:
///
/// 1. Collect locals via `lua_getlocal` and upvalues via
///    `lua_getupvalue` for the target frame.
/// 2. Build an environment table keyed by variable name, with an
///    `__index` metamethod falling back to `_ENV` (globals).
/// 3. Compile the expression with `load()` and set the env table.
/// 4. Execute and return the result.
///
/// This keeps the evaluation non-invasive — read-only access to the
/// frame's scope without modifying the Lua state.
///
/// [dap-eval]: https://microsoft.github.io/debug-adapter-protocol/specification#Requests_Evaluate
/// [dbg-lua]: https://www.slembcke.net/blog/DebuggerLua/
pub(crate) fn evaluate_expression(
    lua: &Lua,
    expression: &str,
    _frame_id: Option<usize>,
) -> Result<String, String> {
    // TODO(Phase 2): When frame_id is Some, build a locals+upvalues
    // environment table and pass it to `load()` as the chunk's env.

    // Expression-only evaluation: compile as "return <expr>".
    // `into_function` compiles without executing, separating syntax
    // errors from runtime errors.
    //
    // Statement execution (assignments, loops, etc.) is intentionally
    // rejected to prevent side effects.  DAP-compliant statement
    // evaluation gated behind `context: "repl"` is planned for Phase 2.
    let expr_code = format!("return {expression}");
    let func = lua
        .load(&expr_code)
        .into_function()
        .map_err(|e| format!("syntax error: {e}"))?;

    match func.call::<LuaValue>(()) {
        Ok(val) => Ok(lua_value_to_display(&val)),
        Err(e) => Err(e.to_string()),
    }
}

fn lua_value_to_display(val: &LuaValue) -> String {
    match val {
        LuaValue::Nil => "nil".to_string(),
        LuaValue::Boolean(b) => b.to_string(),
        LuaValue::Integer(n) => n.to_string(),
        LuaValue::Number(n) => n.to_string(),
        LuaValue::String(s) => format!("\"{}\"", s.to_string_lossy()),
        LuaValue::Table(_) => "table".to_string(),
        LuaValue::Function(_) => "function".to_string(),
        LuaValue::Thread(_) => "thread".to_string(),
        LuaValue::UserData(_) => "userdata".to_string(),
        LuaValue::LightUserData(_) => "lightuserdata".to_string(),
        LuaValue::Error(e) => format!("error: {e}"),
        _ => "<unknown>".to_string(),
    }
}
