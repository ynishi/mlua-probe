//! Spy / stub / mock test doubles implemented as Lua `UserData`.
//!
//! Provides [`register`] to inject a `test_doubles` global table into
//! a Lua VM with the following factory functions:
//!
//! ```lua
//! -- spy: transparently records calls while delegating to the original
//! local s = test_doubles.spy(function(x) return x * 2 end)
//! s(5)
//! assert(s:call_count() == 1)
//! assert(s:was_called_with(5))
//!
//! -- stub: returns fixed values without calling any original
//! local st = test_doubles.stub()
//! st:returns(42)
//! assert(st() == 42)
//!
//! -- spy_on: replaces a table method with a spy, supports revert()
//! local obj = { greet = function(name) return "hello " .. name end }
//! local s = test_doubles.spy_on(obj, "greet")
//! obj.greet("world")
//! assert(s:call_count() == 1)
//! s:revert()  -- restore the original method
//! ```

use std::sync::{Arc, Mutex};

use mlua::prelude::*;

// ── Value comparison ────────────────────────────────────────────

/// Compare two Lua values for equality (primitive types only).
///
/// Uses exact comparison semantics consistent with Lua 5.4's `==`
/// operator and the lust framework's `eq()` helper (which defaults
/// to `eps = 0`).
///
/// For Integer↔Number cross-type comparison, a round-trip check
/// guards against precision loss when `i64` values exceed 2^53
/// (the limit of exact integer representation in `f64`).
///
/// Tables, functions, and userdata are compared by identity
/// (always `false` here — use `call_args()` for complex assertions).
fn values_match(a: &LuaValue, b: &LuaValue) -> bool {
    use LuaValue::*;
    match (a, b) {
        (Nil, Nil) => true,
        (Boolean(a), Boolean(b)) => a == b,
        (Integer(a), Integer(b)) => a == b,
        (Number(a), Number(b)) => a == b,
        (Integer(a), Number(b)) => {
            let f = *a as f64;
            f == *b && f as i64 == *a
        }
        (Number(a), Integer(b)) => {
            let f = *b as f64;
            *a == f && *a as i64 == *b
        }
        (String(a), String(b)) => a.as_bytes() == b.as_bytes(),
        _ => false,
    }
}

// ── Shared state ────────────────────────────────────────────────

struct RevertInfo {
    target: LuaTable,
    key: String,
    original: LuaFunction,
}

struct DoubleState {
    /// Recorded calls — each entry is the argument list of one invocation.
    calls: Vec<Vec<LuaValue>>,
    /// When set, `__call` returns these values instead of calling through.
    return_values: Option<Vec<LuaValue>>,
    /// The original function (for call-through spies).
    original: Option<LuaFunction>,
    /// Whether `__call` should delegate to `original`.
    call_through: bool,
    /// Present only for `spy_on` — stores what to restore on `revert()`.
    revert_info: Option<RevertInfo>,
}

// ── UserData ────────────────────────────────────────────────────

/// A test double (spy or stub) exposed to Lua as `UserData`.
///
/// Created via the `test_doubles.spy()`, `test_doubles.stub()`, or
/// `test_doubles.spy_on()` factory functions.
pub(crate) struct LuaDouble {
    state: Arc<Mutex<DoubleState>>,
}

impl LuaDouble {
    fn new_spy(original: Option<LuaFunction>) -> Self {
        Self {
            state: Arc::new(Mutex::new(DoubleState {
                calls: Vec::new(),
                return_values: None,
                original,
                call_through: true,
                revert_info: None,
            })),
        }
    }

    fn new_stub() -> Self {
        Self {
            state: Arc::new(Mutex::new(DoubleState {
                calls: Vec::new(),
                return_values: None,
                original: None,
                call_through: false,
                revert_info: None,
            })),
        }
    }

    fn new_table_spy(original: LuaFunction, target: LuaTable, key: String) -> Self {
        let call_fn = original.clone();
        Self {
            state: Arc::new(Mutex::new(DoubleState {
                calls: Vec::new(),
                return_values: None,
                original: Some(call_fn),
                call_through: true,
                revert_info: Some(RevertInfo {
                    target,
                    key,
                    original,
                }),
            })),
        }
    }

    fn lock(&self) -> LuaResult<std::sync::MutexGuard<'_, DoubleState>> {
        self.state
            .lock()
            .map_err(|e| LuaError::runtime(format!("spy state poisoned: {e}")))
    }
}

impl LuaUserData for LuaDouble {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        // ── __call: record args, then delegate or stub ──────────

        methods.add_meta_method(LuaMetaMethod::Call, |_, this, args: LuaMultiValue| {
            let args_vec: Vec<LuaValue> = args.into_vec();

            let mut state = this.lock()?;
            state.calls.push(args_vec.clone());

            // Stub: return fixed values.
            if let Some(ref rv) = state.return_values {
                return Ok(LuaMultiValue::from_vec(rv.clone()));
            }

            // Call-through: delegate to original.
            if state.call_through {
                if let Some(ref original) = state.original {
                    let f = original.clone();
                    // Drop the lock before calling into Lua to avoid
                    // deadlock if the called function re-enters the spy.
                    drop(state);
                    return f.call(LuaMultiValue::from_vec(args_vec));
                }
            }

            // No original and no stub values — return nothing.
            Ok(LuaMultiValue::new())
        });

        // ── __len: call count (lust-compatible #spy) ────────────

        methods.add_meta_method(LuaMetaMethod::Len, |_, this, ()| {
            Ok(this.lock()?.calls.len())
        });

        // ── Inspection methods ──────────────────────────────────

        methods.add_method("call_count", |_, this, ()| Ok(this.lock()?.calls.len()));

        methods.add_method("call_args", |lua, this, n: usize| {
            let state = this.lock()?;
            let idx = n
                .checked_sub(1)
                .ok_or_else(|| LuaError::runtime("call index must be >= 1"))?;
            let call = state
                .calls
                .get(idx)
                .ok_or_else(|| LuaError::runtime(format!("no call at index {n}")))?;
            let table = lua.create_table()?;
            for (i, arg) in call.iter().enumerate() {
                table.set(i + 1, arg.clone())?;
            }
            Ok(table)
        });

        methods.add_method("was_called_with", |_, this, args: LuaMultiValue| {
            let expected: Vec<LuaValue> = args.into_vec();
            let state = this.lock()?;
            for call in &state.calls {
                if call.len() == expected.len()
                    && call
                        .iter()
                        .zip(expected.iter())
                        .all(|(a, b)| values_match(a, b))
                {
                    return Ok(true);
                }
            }
            Ok(false)
        });

        // ── Mutation methods ────────────────────────────────────

        methods.add_method("returns", |_, this, args: LuaMultiValue| {
            let mut state = this.lock()?;
            state.return_values = Some(args.into_vec());
            state.call_through = false;
            Ok(())
        });

        // Clear recorded call history.
        //
        // Only resets `calls`; `return_values` set via `returns()` are
        // preserved.  This mirrors common test-double conventions
        // (e.g. Sinon.js `resetHistory`) where resetting history and
        // resetting behaviour are separate operations.
        methods.add_method("reset", |_, this, ()| {
            let mut state = this.lock()?;
            state.calls.clear();
            Ok(())
        });

        methods.add_method("revert", |_, this, ()| {
            let state = this.lock()?;
            if let Some(ref info) = state.revert_info {
                info.target.set(info.key.as_str(), info.original.clone())?;
            }
            Ok(())
        });
    }
}

// ── Registration ────────────────────────────────────────────────

/// Register the `test_doubles` global table into the given Lua VM.
///
/// Provides `test_doubles.spy(fn)`, `test_doubles.stub()`, and
/// `test_doubles.spy_on(table, key)`.
pub fn register(lua: &Lua) -> LuaResult<()> {
    let doubles = lua.create_table()?;

    // test_doubles.spy(fn?) → LuaDouble
    doubles.set(
        "spy",
        lua.create_function(|_, func: Option<LuaFunction>| Ok(LuaDouble::new_spy(func)))?,
    )?;

    // test_doubles.stub() → LuaDouble
    doubles.set(
        "stub",
        lua.create_function(|_, ()| Ok(LuaDouble::new_stub()))?,
    )?;

    // test_doubles.spy_on(table, key) → LuaDouble
    //
    // Replaces `table[key]` with a spy that calls through to the
    // original.  Call `spy:revert()` to restore.
    doubles.set(
        "spy_on",
        lua.create_function(|lua, (table, key): (LuaTable, String)| {
            let original: LuaFunction = table.get(key.as_str())?;
            let spy = LuaDouble::new_table_spy(original, table.clone(), key.clone());
            let ud = lua.create_userdata(spy)?;
            table.set(key.as_str(), ud.clone())?;
            Ok(ud)
        })?,
    )?;

    lua.globals().set("test_doubles", doubles)?;
    Ok(())
}
