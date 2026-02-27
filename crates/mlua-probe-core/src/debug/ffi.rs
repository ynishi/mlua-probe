//! Unsafe FFI wrappers for Lua debug C API functions.
//!
//! All `unsafe` code for `lua_getlocal`, `lua_getupvalue`, etc. is
//! concentrated in this module.  Every function documents its safety
//! contract via `// SAFETY:` comments.
//!
//! # Stack discipline
//!
//! Functions that push values onto the Lua stack **must** pop them
//! before returning.  Debug builds assert stack balance.

use mlua::ffi;
use std::ffi::CStr;
use std::os::raw::c_int;

use super::types::Variable;

/// Collect local variables at the given stack `level`.
///
/// Level 0 is the currently executing function inside the hook.
/// Level 1 is the function that triggered the hook, etc.
///
/// # Safety contract
///
/// Must be called on the VM thread (inside a hook callback or while
/// the VM is otherwise guaranteed not to be running concurrently).
/// # Safety
///
/// `state` must be a valid `lua_State` pointer and the caller must be
/// on the VM thread (inside a hook callback or while the VM is
/// guaranteed not to be running concurrently).
pub(crate) unsafe fn get_locals(state: *mut ffi::lua_State, level: c_int) -> Vec<Variable> {
    let mut vars = Vec::new();

    unsafe {
        // SAFETY: `state` is a valid lua_State pointer obtained from
        // Lua::as_raw_state(). We are on the VM thread.
        let top_before = ffi::lua_gettop(state);

        let mut ar: ffi::lua_Debug = std::mem::zeroed();
        if ffi::lua_getstack(state, level, &mut ar) == 0 {
            return vars;
        }

        let mut n: c_int = 1;
        loop {
            // SAFETY: ar was populated by lua_getstack above.
            // lua_getlocal pushes the variable's value onto the stack.
            let name_ptr = ffi::lua_getlocal(state, &ar, n);
            if name_ptr.is_null() {
                break;
            }

            // SAFETY: name_ptr is a valid C string from the Lua runtime.
            let name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();

            let var = read_stack_top(state, &name);

            // SAFETY: lua_getlocal pushed one value; pop it.
            ffi::lua_pop(state, 1);

            // Names starting with '(' are internal (e.g. "(for state)").
            if !name.starts_with('(') {
                vars.push(var);
            }

            n = match n.checked_add(1) {
                Some(next) => next,
                None => break,
            };
        }

        debug_assert_eq!(
            ffi::lua_gettop(state),
            top_before,
            "stack imbalance in get_locals"
        );
    }

    vars
}

/// Collect upvalues for the function at the given stack `level`.
/// # Safety
///
/// Same contract as [`get_locals`].
pub(crate) unsafe fn get_upvalues(state: *mut ffi::lua_State, level: c_int) -> Vec<Variable> {
    let mut vars = Vec::new();

    unsafe {
        // SAFETY: same contract as get_locals.
        let top_before = ffi::lua_gettop(state);

        let mut ar: ffi::lua_Debug = std::mem::zeroed();
        if ffi::lua_getstack(state, level, &mut ar) == 0 {
            return vars;
        }

        // Push the function onto the stack so we can query its upvalues.
        // SAFETY: "f" flag tells lua_getinfo to push the function.
        let ret = ffi::lua_getinfo(state, c"f".as_ptr(), &mut ar);
        if ret == 0 {
            return vars;
        }
        let func_idx = ffi::lua_gettop(state);

        let mut n: c_int = 1;
        loop {
            // SAFETY: func_idx points to a Lua function on the stack.
            let name_ptr = ffi::lua_getupvalue(state, func_idx, n);
            if name_ptr.is_null() {
                break;
            }

            let name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
            let var = read_stack_top(state, &name);

            // SAFETY: lua_getupvalue pushed one value.
            ffi::lua_pop(state, 1);

            vars.push(var);
            n = match n.checked_add(1) {
                Some(next) => next,
                None => break,
            };
        }

        // Pop the function pushed by lua_getinfo.
        ffi::lua_pop(state, 1);

        debug_assert_eq!(
            ffi::lua_gettop(state),
            top_before,
            "stack imbalance in get_upvalues"
        );
    }

    vars
}

/// Convert the value at the top of the Lua stack to a display string.
///
/// Does **not** pop the value.
///
/// # Safety
///
/// The Lua stack must have at least one element.
unsafe fn stack_top_to_display(state: *mut ffi::lua_State) -> String {
    unsafe {
        let type_id = ffi::lua_type(state, -1);
        match type_id {
            ffi::LUA_TNIL => "nil".to_string(),
            ffi::LUA_TBOOLEAN => if ffi::lua_toboolean(state, -1) != 0 {
                "true"
            } else {
                "false"
            }
            .to_string(),
            ffi::LUA_TNUMBER => {
                if ffi::lua_isinteger(state, -1) != 0 {
                    format!("{}", ffi::lua_tointeger(state, -1))
                } else {
                    format!("{}", ffi::lua_tonumber(state, -1))
                }
            }
            ffi::LUA_TSTRING => {
                let mut len: usize = 0;
                let ptr = ffi::lua_tolstring(state, -1, &mut len);
                if ptr.is_null() {
                    "\"\"".to_string()
                } else {
                    let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
                    format!("\"{}\"", String::from_utf8_lossy(bytes))
                }
            }
            ffi::LUA_TTABLE => {
                let raw_len = ffi::lua_rawlen(state, -1);
                format!("table (len={raw_len})")
            }
            ffi::LUA_TFUNCTION => "function".to_string(),
            ffi::LUA_TUSERDATA => "userdata".to_string(),
            ffi::LUA_TTHREAD => "thread".to_string(),
            ffi::LUA_TLIGHTUSERDATA => "lightuserdata".to_string(),
            _ => {
                let type_name_ptr = ffi::lua_typename(state, type_id);
                let type_name = CStr::from_ptr(type_name_ptr).to_string_lossy();
                format!("<{type_name}>")
            }
        }
    }
}

/// Read the value at the top of the Lua stack and convert it to a
/// [`Variable`].  Does **not** pop the value.
///
/// # Safety
///
/// The Lua stack must have at least one element.
unsafe fn read_stack_top(state: *mut ffi::lua_State, name: &str) -> Variable {
    let type_id = ffi::lua_type(state, -1);
    let type_name_ptr = ffi::lua_typename(state, type_id);
    let type_name = CStr::from_ptr(type_name_ptr).to_string_lossy().into_owned();

    let value = stack_top_to_display(state);

    Variable {
        name: name.to_string(),
        value,
        type_name,
        // TODO: Phase 2 — issue a real VariableRef for table lazy expansion.
        children_ref: None,
    }
}

// ─── Frame-scoped evaluation (Phase 2) ──────────────

/// Build an environment table for frame-scoped expression evaluation.
///
/// Creates a table containing all locals and upvalues at the given
/// stack `level`, with a metatable `{ __index = _G }` so that global
/// variables remain accessible.
///
/// Locals are set **after** upvalues so they take priority, matching
/// Lua's scoping rules.
///
/// # Stack effect
///
/// Pushes **one** table (the environment) onto the stack.
///
/// # Safety
///
/// Same contract as [`get_locals`].
unsafe fn build_frame_env(state: *mut ffi::lua_State, level: c_int) {
    unsafe {
        ffi::lua_createtable(state, 0, 16);
        let env_idx = ffi::lua_gettop(state);

        let mut ar: ffi::lua_Debug = std::mem::zeroed();
        if ffi::lua_getstack(state, level, &mut ar) != 0 {
            // ── Upvalues first (locals will override) ──
            if ffi::lua_getinfo(state, c"f".as_ptr(), &mut ar) != 0 {
                let func_idx = ffi::lua_gettop(state);
                let mut n: c_int = 1;
                loop {
                    let name_ptr = ffi::lua_getupvalue(state, func_idx, n);
                    if name_ptr.is_null() {
                        break;
                    }
                    // lua_getupvalue pushed the value; setfield pops it.
                    ffi::lua_setfield(state, env_idx, name_ptr);
                    n = match n.checked_add(1) {
                        Some(next) => next,
                        None => break,
                    };
                }
                ffi::lua_pop(state, 1); // pop the function
            }

            // ── Locals override upvalues ──
            let mut ar2: ffi::lua_Debug = std::mem::zeroed();
            if ffi::lua_getstack(state, level, &mut ar2) != 0 {
                let mut n: c_int = 1;
                loop {
                    let name_ptr = ffi::lua_getlocal(state, &ar2, n);
                    if name_ptr.is_null() {
                        break;
                    }
                    let name = CStr::from_ptr(name_ptr);
                    if name.to_bytes().starts_with(b"(") {
                        // Internal variable (e.g. "(for state)") — skip.
                        ffi::lua_pop(state, 1);
                    } else {
                        ffi::lua_setfield(state, env_idx, name_ptr);
                    }
                    n = match n.checked_add(1) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
        }

        // ── Metatable: { __index = <globals> } ──
        ffi::lua_createtable(state, 0, 1);
        ffi::lua_rawgeti(state, ffi::LUA_REGISTRYINDEX, ffi::LUA_RIDX_GLOBALS);
        ffi::lua_setfield(state, -2, c"__index".as_ptr());
        ffi::lua_setmetatable(state, env_idx);

        debug_assert_eq!(
            ffi::lua_gettop(state),
            env_idx,
            "stack imbalance in build_frame_env"
        );
    }
}

/// Evaluate a Lua expression in the scope of a specific stack frame.
///
/// Builds a locals+upvalues environment (via [`build_frame_env`]),
/// sets it as the chunk's `_ENV`, compiles and executes the expression.
///
/// In Lua 5.4, the first upvalue of a compiled chunk is always `_ENV`.
/// [`lua_setupvalue`](ffi::lua_setupvalue) replaces it with our custom
/// env table, so all name lookups in the expression go through:
///
/// 1. Locals (highest priority)
/// 2. Upvalues
/// 3. Globals (via `__index` metamethod)
///
/// # Stack effect
///
/// Net zero — all pushed values are cleaned up.
///
/// # Safety
///
/// Same contract as [`get_locals`].
pub(crate) unsafe fn evaluate_with_frame_env(
    state: *mut ffi::lua_State,
    code: &str,
    level: c_int,
) -> Result<String, String> {
    unsafe {
        let top = ffi::lua_gettop(state);

        // 1. Build env table (pushes 1 value).
        build_frame_env(state, level);
        let env_idx = ffi::lua_gettop(state);

        // 2. Compile expression.
        let ret = ffi::luaL_loadbufferx(
            state,
            code.as_ptr().cast(),
            code.len(),
            c"=eval".as_ptr(),
            std::ptr::null(),
        );
        if ret != ffi::LUA_OK {
            let err = stack_top_to_error(state);
            ffi::lua_settop(state, top);
            return Err(format!("syntax error: {err}"));
        }
        // Stack: ... env func
        let func_idx = ffi::lua_gettop(state);

        // 3. Set _ENV as the first upvalue of the compiled chunk.
        ffi::lua_pushvalue(state, env_idx);
        let upname = ffi::lua_setupvalue(state, func_idx, 1);
        if upname.is_null() {
            // lua_setupvalue failed — clean up.
            ffi::lua_settop(state, top);
            return Err("failed to set chunk environment".to_string());
        }
        // lua_setupvalue consumed the pushed value.
        // Stack: ... env func

        // 4. Execute (0 args, 1 result).
        let call_ret = ffi::lua_pcallk(state, 0, 1, 0, 0, None);
        // Stack: ... env result_or_error

        if call_ret != ffi::LUA_OK {
            let err = stack_top_to_error(state);
            ffi::lua_settop(state, top);
            return Err(err);
        }

        // 5. Read result.
        let result = stack_top_to_display(state);
        ffi::lua_settop(state, top);
        Ok(result)
    }
}

/// Evaluate a breakpoint condition in the scope of a specific stack frame.
///
/// Returns `true` if the condition evaluates to a truthy value (not `nil`,
/// not `false`).  Returns `false` on any error (syntax, runtime) or if the
/// result is falsy — a failing condition silently skips the breakpoint
/// rather than disrupting execution.
///
/// Uses [`build_frame_env`] to give the condition access to locals,
/// upvalues, and globals.
///
/// # Stack effect
///
/// Net zero — all pushed values are cleaned up.
///
/// # Safety
///
/// Same contract as [`get_locals`].
pub(crate) unsafe fn evaluate_condition(
    state: *mut ffi::lua_State,
    condition: &str,
    level: c_int,
) -> bool {
    unsafe {
        let top = ffi::lua_gettop(state);

        // 1. Build env table with locals + upvalues.
        build_frame_env(state, level);
        let env_idx = ffi::lua_gettop(state);

        // 2. Compile condition as "return <condition>".
        let code = format!("return {condition}");
        let ret = ffi::luaL_loadbufferx(
            state,
            code.as_ptr().cast(),
            code.len(),
            c"=condition".as_ptr(),
            std::ptr::null(),
        );
        if ret != ffi::LUA_OK {
            ffi::lua_settop(state, top);
            return false;
        }
        let func_idx = ffi::lua_gettop(state);

        // 3. Set _ENV as the first upvalue of the compiled chunk.
        ffi::lua_pushvalue(state, env_idx);
        let upname = ffi::lua_setupvalue(state, func_idx, 1);
        if upname.is_null() {
            ffi::lua_settop(state, top);
            return false;
        }

        // 4. Execute (0 args, 1 result).
        let call_ret = ffi::lua_pcallk(state, 0, 1, 0, 0, None);
        if call_ret != ffi::LUA_OK {
            ffi::lua_settop(state, top);
            return false;
        }

        // 5. Check truthiness (not nil and not false).
        let is_truthy = ffi::lua_toboolean(state, -1) != 0;
        ffi::lua_settop(state, top);
        is_truthy
    }
}

/// Read the value at the top of the stack as an error string.
///
/// # Safety
///
/// The Lua stack must have at least one element.
unsafe fn stack_top_to_error(state: *mut ffi::lua_State) -> String {
    unsafe {
        let mut len: usize = 0;
        let ptr = ffi::lua_tolstring(state, -1, &mut len);
        if ptr.is_null() {
            "unknown error".to_string()
        } else {
            let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::prelude::*;

    #[test]
    fn get_locals_from_hook() {
        let lua = Lua::new();

        // Collect locals from inside a hook.
        let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected_clone = collected.clone();

        lua.set_hook(mlua::HookTriggers::EVERY_LINE, move |lua, debug| {
            if debug.current_line() == Some(3) {
                let state = lua.exec_raw_lua(|raw| raw.state());
                // Level 0 = hook internal, level 1 = the Lua code.
                // In practice the offset depends on mlua's internal
                // wrapping. We try levels until we find locals.
                for level in 0..5 {
                    // SAFETY: We are on the VM thread inside a hook callback.
                    // `state` was obtained from `lua.exec_raw_lua()`.
                    let locals = unsafe { get_locals(state, level) };
                    if locals.iter().any(|v| v.name == "x") {
                        *collected_clone.lock().unwrap() = locals;
                        break;
                    }
                }
            }
            Ok(mlua::VmState::Continue)
        })
        .unwrap();

        lua.load("local x = 42\nlocal y = 'hello'\nlocal z = x + 1")
            .set_name("@test.lua")
            .exec()
            .unwrap();

        let locals = collected.lock().unwrap();
        assert!(
            locals.iter().any(|v| v.name == "x" && v.value == "42"),
            "expected local x=42, got: {locals:?}"
        );
    }
}
