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

// ─── Lua 5.4 internal struct bindings (NOT public API) ───
//
// These `#[repr(C)]` definitions mirror the vendored Lua 5.4.8
// source (lua-src-550.0.0).  They are used **only** to recover
// the names of for-loop control variables that `lua_getlocal`
// reports as `(temporary)` because the variable's debug scope
// (`startpc..endpc`) does not cover the FORLOOP instruction.
//
// !! If the vendored Lua version changes, these layouts MUST be
// !! verified against the new source.

/// `LocVar` — lobject.h:525
#[repr(C)]
struct LocVar {
    varname: *mut TString,
    startpc: c_int,
    endpc: c_int,
}

/// Minimal `TString` header — lobject.h:385-396
///
/// Layout: CommonHeader (next, tt, marked) + extra + shrlen + hash + union u + contents[1]
/// We only need `shrlen` and `contents` to read the string.
#[repr(C)]
struct TString {
    next: *mut std::ffi::c_void, // GCObject *next
    tt: u8,                      // lu_byte tt
    marked: u8,                  // lu_byte marked
    extra: u8,                   // lu_byte extra
    shrlen: u8,                  // lu_byte shrlen (0xFF for long strings)
    hash: u32,                   // unsigned int hash
    u: usize,                    // union { size_t lnglen; TString *hnext; }
                                 // contents[1] follows — accessed via pointer arithmetic
}

impl TString {
    /// Read the string contents as a `&str`.
    ///
    /// # Safety
    ///
    /// `self` must point to a valid, live `TString` allocated by Lua.
    unsafe fn as_str(&self) -> Option<&str> {
        let len = if self.shrlen != 0xFF {
            self.shrlen as usize
        } else {
            self.u // lnglen
        };
        if len == 0 {
            return Some("");
        }
        // `contents` is a flexible array member starting at offset
        // `&self.u as *const _ + size_of::<usize>()` — but it's
        // actually declared as `char contents[1]` right after the `u`
        // union in the C struct.  We use the field offset.
        let contents_ptr = std::ptr::addr_of!(self.u)
            .cast::<u8>()
            .add(std::mem::size_of::<usize>());
        let bytes = std::slice::from_raw_parts(contents_ptr, len);
        std::str::from_utf8(bytes).ok()
    }
}

/// Minimal `Proto` — lobject.h:550-573
///
/// We only need `code`, `locvars`, and `sizelocvars`.
/// Fields before those are included to maintain correct offsets.
#[repr(C)]
struct Proto {
    // CommonHeader
    next: *mut std::ffi::c_void,
    tt: u8,
    marked: u8,
    // Proto fields
    numparams: u8,
    is_vararg: u8,
    maxstacksize: u8,
    _pad: [u8; 0], // alignment may insert padding; see note below
    sizeupvalues: c_int,
    sizek: c_int,
    sizecode: c_int,
    sizelineinfo: c_int,
    sizep: c_int,
    sizelocvars: c_int,
    sizeabslineinfo: c_int,
    linedefined: c_int,
    lastlinedefined: c_int,
    k: *mut std::ffi::c_void,           // TValue *k
    code: *mut u32,                     // Instruction *code
    p: *mut *mut Proto,                 // Proto **p
    upvalues: *mut std::ffi::c_void,    // Upvaldesc *upvalues
    lineinfo: *mut i8,                  // ls_byte *lineinfo
    abslineinfo: *mut std::ffi::c_void, // AbsLineInfo *abslineinfo
    locvars: *mut LocVar,               // LocVar *locvars
    source: *mut std::ffi::c_void,      // TString *source
    gclist: *mut std::ffi::c_void,      // GCObject *gclist
}

/// Minimal `LClosure` — lobject.h:654-658
///
/// Layout: ClosureHeader (CommonHeader + nupvalues + gclist) + Proto *p
#[repr(C)]
struct LClosure {
    // CommonHeader
    next: *mut std::ffi::c_void,
    tt: u8,
    marked: u8,
    // ClosureHeader additions
    nupvalues: u8,
    _pad: [u8; 0],
    gclist: *mut std::ffi::c_void,
    // LClosure-specific
    p: *mut Proto,
    // UpVal *upvals[1] follows — we don't need it
}

/// Minimal `CallInfo` — lstate.h:177-204
///
/// We only need `func` (StkIdRel) and `u.l.savedpc`.
/// On 64-bit systems `StkIdRel` is just a pointer (`StackValue *`).
#[repr(C)]
struct CallInfo {
    func: *mut std::ffi::c_void, // StkIdRel func
    top: *mut std::ffi::c_void,  // StkIdRel top
    previous: *mut CallInfo,
    next_ci: *mut CallInfo,
    // union u — we access the Lua branch (u.l)
    savedpc: *const u32, // u.l.savedpc (Instruction *)
    trap: i32,           // u.l.trap (l_signalT = sig_atomic_t)
    nextraargs: c_int,   // u.l.nextraargs
                         // u2 union follows but we don't need it
}

/// Mirror of `ffi::lua_Debug` with `i_ci` exposed as public.
///
/// mlua-sys declares `i_ci` as private.  Since the struct is
/// `#[repr(C)]`, we can safely transmute a pointer to read it.
#[repr(C)]
struct LuaDebugExt {
    pub event: c_int,
    pub name: *const std::os::raw::c_char,
    pub namewhat: *const std::os::raw::c_char,
    pub what: *const std::os::raw::c_char,
    pub source: *const std::os::raw::c_char,
    pub srclen: usize,
    pub currentline: c_int,
    pub linedefined: c_int,
    pub lastlinedefined: c_int,
    pub nups: u8,
    pub nparams: u8,
    pub isvararg: std::os::raw::c_char,
    pub istailcall: std::os::raw::c_char,
    pub ftransfer: u16,
    pub ntransfer: u16,
    pub short_src: [std::os::raw::c_char; 60], // LUA_IDSIZE = 60
    pub i_ci: *mut std::ffi::c_void,
}

/// Recover the real name for a `(temporary)` local variable slot.
///
/// When `lua_getlocal` returns `(temporary)` for slot `n`, the
/// variable's value is still in the register but Lua considers
/// it out of scope.  This function walks `Proto.locvars` to find
/// the original variable name for that register slot.
///
/// Returns `None` if the name cannot be recovered (e.g. true
/// temporary, or internal variable).
///
/// # Safety
///
/// `state` must be valid.  `ar` must have been populated by
/// `lua_getstack`.  Must be called on the VM thread.
unsafe fn recover_varname(
    state: *mut ffi::lua_State,
    ar: &ffi::lua_Debug,
    slot: c_int,
) -> Option<String> {
    unsafe {
        // 1. Get CallInfo from lua_Debug.i_ci.
        //    mlua-sys declares i_ci as private, so we cast through
        //    our mirror struct with identical layout.
        let ar_ext = (ar as *const ffi::lua_Debug).cast::<LuaDebugExt>();
        let ci = (*ar_ext).i_ci as *const CallInfo;
        if ci.is_null() {
            return None;
        }

        // 2. Push the function via lua_getinfo("f") to get LClosure pointer
        let mut ar2: ffi::lua_Debug = std::mem::zeroed();
        std::ptr::copy_nonoverlapping(ar, &mut ar2, 1);
        if ffi::lua_getinfo(state, c"f".as_ptr(), &mut ar2) == 0 {
            return None;
        }
        // lua_topointer gives us the GCObject* which IS the LClosure*
        let closure_ptr = ffi::lua_topointer(state, -1) as *const LClosure;
        ffi::lua_pop(state, 1); // pop the function

        if closure_ptr.is_null() {
            return None;
        }

        let proto = (*closure_ptr).p;
        if proto.is_null() {
            return None;
        }

        // 3. Compute currentpc = savedpc - proto.code - 1
        let savedpc = (*ci).savedpc;
        let code = (*proto).code;
        if savedpc.is_null() || code.is_null() {
            return None;
        }
        let currentpc = (savedpc.offset_from(code) as c_int) - 1;

        // 4. Walk locvars to find the variable whose register matches
        //    `slot - 1` (0-based) and whose scope doesn't cover currentpc.
        let sizelocvars = (*proto).sizelocvars;
        let locvars = (*proto).locvars;
        if locvars.is_null() || sizelocvars <= 0 {
            return None;
        }

        // Build register mapping: for each locvar[k], determine its
        // 1-based slot number by counting how many locvars are active
        // at locvar[k].startpc (including k itself).
        //
        // This replicates the logic in luaF_getlocalname: the n-th
        // active variable at a given PC occupies register n-1.
        let register_for = |k: c_int| -> c_int {
            let target_pc = (*locvars.offset(k as isize)).startpc;
            let mut count: c_int = 0;
            for j in 0..=k {
                let lv = &*locvars.offset(j as isize);
                if lv.startpc <= target_pc && target_pc < lv.endpc {
                    count += 1;
                }
            }
            count // 1-based slot
        };

        // Search for the locvar that:
        // a) maps to the given slot
        // b) is NOT active at currentpc (out of scope)
        // c) has a real name (not starting with '(')
        for k in 0..sizelocvars {
            let lv = &*locvars.offset(k as isize);

            // Skip if currently in scope (lua_getlocal would have
            // returned the real name already).
            if lv.startpc <= currentpc && currentpc < lv.endpc {
                continue;
            }

            // Skip if this locvar hasn't been reached yet
            if currentpc < lv.startpc {
                continue;
            }

            // Check if this locvar's register matches our slot
            if register_for(k) != slot {
                continue;
            }

            // Read the variable name from TString
            let varname_ts = lv.varname as *const TString;
            if varname_ts.is_null() {
                continue;
            }
            if let Some(name) = (*varname_ts).as_str() {
                // Skip internal names like "(for state)"
                if !name.starts_with('(') {
                    return Some(name.to_string());
                }
            }
        }

        None
    }
}

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

            if name == "(temporary)" {
                // Out-of-scope variable (e.g. for-loop control var at
                // FORLOOP instruction).  Try to recover the real name.
                if let Some(real_name) = recover_varname(state, &ar, n) {
                    let var = read_stack_top(state, &real_name);
                    ffi::lua_pop(state, 1);
                    vars.push(var);
                } else {
                    ffi::lua_pop(state, 1);
                }
            } else if !name.starts_with('(') {
                // Normal in-scope variable.
                let var = read_stack_top(state, &name);
                ffi::lua_pop(state, 1);
                vars.push(var);
            } else {
                // Internal variable like "(for state)" — skip.
                ffi::lua_pop(state, 1);
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
                    let name_bytes = name.to_bytes();

                    if name_bytes == b"(temporary)" {
                        // Out-of-scope variable — recover real name.
                        if let Some(real_name) = recover_varname(state, &ar2, n) {
                            let cname = std::ffi::CString::new(real_name).ok();
                            if let Some(cname) = cname {
                                // Value is on top; setfield pops it.
                                ffi::lua_setfield(state, env_idx, cname.as_ptr());
                            } else {
                                ffi::lua_pop(state, 1);
                            }
                        } else {
                            ffi::lua_pop(state, 1);
                        }
                    } else if name_bytes.starts_with(b"(") {
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

    /// Test that `get_locals` returns the for-loop control variable `i`
    /// at the FORLOOP instruction (line hit #2).
    #[test]
    fn get_locals_recovers_for_loop_var() {
        let lua = Lua::new();

        let hit_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hit_clone = hit_count.clone();
        let diag = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let diag_clone = diag.clone();
        let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected_clone = collected.clone();

        lua.set_hook(mlua::HookTriggers::EVERY_LINE, move |lua, debug| {
            if debug.current_line() == Some(2) {
                let n = hit_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 1 {
                    // 2nd hit on line 2 = FORLOOP instruction
                    let state = lua.exec_raw_lua(|raw| raw.state());
                    let mut d = String::new();

                    for level in 0..5 {
                        // SAFETY: In hook callback on VM thread.
                        let locals = unsafe { get_locals(state, level) };
                        d.push_str(&format!(
                            "level={level} locals={:?}\n",
                            locals
                                .iter()
                                .map(|v| format!("{}={}", v.name, v.value))
                                .collect::<Vec<_>>()
                        ));
                        if locals.iter().any(|v| v.name == "sum") {
                            *collected_clone.lock().unwrap() = locals;
                            break;
                        }
                    }

                    // Also dump raw lua_getlocal results at each slot
                    for level in 0..5 {
                        unsafe {
                            let mut ar: ffi::lua_Debug = std::mem::zeroed();
                            if ffi::lua_getstack(state, level, &mut ar) == 0 {
                                continue;
                            }
                            let check = ffi::lua_getlocal(state, &ar, 1);
                            if check.is_null() {
                                continue;
                            }
                            let cname = CStr::from_ptr(check).to_string_lossy().into_owned();
                            ffi::lua_pop(state, 1);
                            if cname != "sum" {
                                continue;
                            }

                            d.push_str(&format!("raw slots at level={level}:\n"));
                            for slot in 1..=10 {
                                let name_ptr = ffi::lua_getlocal(state, &ar, slot);
                                if name_ptr.is_null() {
                                    d.push_str(&format!("  slot {slot}: NULL\n"));
                                    break;
                                }
                                let sname = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
                                let val = stack_top_to_display(state);
                                ffi::lua_pop(state, 1);
                                d.push_str(&format!("  slot {slot}: name={sname:?} val={val}\n"));
                            }
                            break;
                        }
                    }

                    *diag_clone.lock().unwrap() = d;
                }
            }
            Ok(mlua::VmState::Continue)
        })
        .unwrap();

        lua.load("local sum = 0\nfor i = 1, 10 do\nsum = sum + i\nend")
            .set_name("@fortest.lua")
            .exec()
            .unwrap();

        let d = diag.lock().unwrap();
        eprintln!("=== DIAG ===\n{d}");

        let locals = collected.lock().unwrap();
        eprintln!(
            "=== COLLECTED ===\n{:?}",
            locals
                .iter()
                .map(|v| format!("{}={}", v.name, v.value))
                .collect::<Vec<_>>()
        );

        assert!(
            locals.iter().any(|v| v.name == "i"),
            "get_locals should recover for-loop variable 'i', got: {:?}",
            locals.iter().map(|v| &v.name).collect::<Vec<_>>()
        );
    }

    /// Test evaluate_condition at FORLOOP can access the for-loop variable.
    #[test]
    fn evaluate_condition_at_forloop() {
        let lua = Lua::new();

        let hit_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hit_clone = hit_count.clone();
        let results = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let results_clone = results.clone();

        lua.set_hook(mlua::HookTriggers::EVERY_LINE, move |lua, debug| {
            if debug.current_line() == Some(2) {
                let n = hit_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n >= 1 {
                    // FORLOOP hits (n=1 is i=1, n=2 is i=2, etc.)
                    let cond_result = lua.exec_raw_lua(|raw| unsafe {
                        evaluate_condition(raw.state(), "i == 3", 0)
                    });
                    let i_val = lua.exec_raw_lua(|raw| unsafe {
                        evaluate_with_frame_env(raw.state(), "return i", 0)
                    });
                    results_clone.lock().unwrap().push((n, cond_result, i_val));
                }
            }
            Ok(mlua::VmState::Continue)
        })
        .unwrap();

        lua.load("local sum = 0\nfor i = 1, 10 do\nsum = sum + i\nend")
            .set_name("@fortest.lua")
            .exec()
            .unwrap();

        let r = results.lock().unwrap();
        eprintln!("=== CONDITION RESULTS ===");
        for (n, cond, val) in r.iter() {
            eprintln!("  hit={n} cond(i==3)={cond} eval(i)={val:?}");
        }

        // At hit n=3 (i=3), the condition should be true
        assert!(
            r.iter().any(|(_, cond, _)| *cond),
            "condition 'i == 3' should be true at some iteration"
        );
        // Verify i is evaluable
        assert!(
            r.iter().all(|(_, _, val)| val.is_ok()),
            "evaluating 'i' should succeed at all FORLOOP hits"
        );
    }

    /// Same test but calling get_locals INSIDE exec_raw_lua (mimics MCP path).
    #[test]
    fn get_locals_inside_exec_raw_lua() {
        let lua = Lua::new();

        let hit_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hit_clone = hit_count.clone();
        let diag = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let diag_clone = diag.clone();
        let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected_clone = collected.clone();

        lua.set_hook(mlua::HookTriggers::EVERY_LINE, move |lua, debug| {
            if debug.current_line() == Some(2) {
                let n = hit_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 1 {
                    // Call get_locals INSIDE exec_raw_lua (MCP path)
                    let mut d = String::new();

                    for level in 0..5i32 {
                        let locals =
                            lua.exec_raw_lua(|raw| unsafe { get_locals(raw.state(), level) });
                        d.push_str(&format!(
                            "level={level} locals={:?}\n",
                            locals
                                .iter()
                                .map(|v| format!("{}={}", v.name, v.value))
                                .collect::<Vec<_>>()
                        ));
                        if locals.iter().any(|v| v.name == "sum") {
                            *collected_clone.lock().unwrap() = locals;
                            break;
                        }
                    }

                    *diag_clone.lock().unwrap() = d;
                }
            }
            Ok(mlua::VmState::Continue)
        })
        .unwrap();

        lua.load("local sum = 0\nfor i = 1, 10 do\nsum = sum + i\nend")
            .set_name("@fortest.lua")
            .exec()
            .unwrap();

        let d = diag.lock().unwrap();
        eprintln!("=== INSIDE exec_raw_lua ===\n{d}");

        let locals = collected.lock().unwrap();
        eprintln!(
            "=== COLLECTED ===\n{:?}",
            locals
                .iter()
                .map(|v| format!("{}={}", v.name, v.value))
                .collect::<Vec<_>>()
        );

        assert!(
            locals.iter().any(|v| v.name == "i"),
            "get_locals inside exec_raw_lua should recover 'i', got: {:?}",
            locals.iter().map(|v| &v.name).collect::<Vec<_>>()
        );
    }
}
