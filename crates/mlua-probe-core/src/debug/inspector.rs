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
/// # Frame-scoped evaluation (DAP-compliant)
///
/// When `frame_id` is `Some`, the expression is evaluated in the scope
/// of the specified stack frame.  Locals, upvalues, and globals are all
/// accessible, with Lua's standard resolution order:
///
/// 1. **Locals** (highest priority)
/// 2. **Upvalues**
/// 3. **Globals** (via `__index` fallback)
///
/// This is implemented by building a custom environment table with
/// locals + upvalues and a `{ __index = _G }` metatable, then setting
/// it as the compiled chunk's `_ENV` via `lua_setupvalue`.
///
/// When `frame_id` is `None`, the expression is evaluated in the
/// global scope only.
///
/// # Expression-only
///
/// Statement execution (assignments, loops, etc.) is intentionally
/// rejected to prevent side effects.  The expression is compiled as
/// `return <expr>` to enforce this.
///
/// [dap-eval]: https://microsoft.github.io/debug-adapter-protocol/specification#Requests_Evaluate
pub(crate) fn evaluate_expression(
    lua: &Lua,
    expression: &str,
    frame_id: Option<usize>,
) -> Result<String, String> {
    let expr_code = format!("return {expression}");

    match frame_id {
        Some(fid) => {
            let level = c_int::try_from(fid).map_err(|_| format!("frame_id {fid} out of range"))?;
            // SAFETY: Called from the paused loop on the VM thread.
            // exec_raw_lua holds mlua's internal lock, ensuring exclusive
            // access to the lua_State for the duration of the FFI call.
            lua.exec_raw_lua(|raw| unsafe {
                ffi::evaluate_with_frame_env(raw.state(), &expr_code, level)
            })
        }
        None => {
            // Global scope evaluation.
            let func = lua
                .load(&expr_code)
                .into_function()
                .map_err(|e| format!("syntax error: {e}"))?;

            match func.call::<LuaValue>(()) {
                Ok(val) => Ok(lua_value_to_display(&val)),
                Err(e) => Err(e.to_string()),
            }
        }
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
