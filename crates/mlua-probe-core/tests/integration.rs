//! Integration tests for the mlua-probe-core engine.
//!
//! These tests exercise the full DebugSession + DebugController flow:
//! attach → breakpoint → Lua execution → pause → inspect → resume → complete.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use mlua::prelude::*;
use mlua_probe_core::{DebugController, DebugEvent, DebugSession, PauseReason, SessionState};

/// Helper: run Lua code on a separate thread, returning a join handle.
fn spawn_lua(lua: Arc<Lua>, code: &str, chunk_name: &str) -> thread::JoinHandle<LuaResult<()>> {
    let code = code.to_string();
    let name = chunk_name.to_string();
    thread::spawn(move || lua.load(&code).set_name(&name).exec())
}

/// Helper: receive an event with a timeout (no thread leak).
fn wait_event_timeout(ctrl: &DebugController, timeout: Duration) -> Option<DebugEvent> {
    ctrl.wait_event_timeout(timeout)
        .expect("event receiver lock poisoned")
}

/// Helper: wait for the next `Paused` event, draining `Continued` events.
fn wait_paused_timeout(ctrl: &DebugController, timeout: Duration) -> Option<DebugEvent> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match wait_event_timeout(ctrl, remaining) {
            Some(DebugEvent::Continued) => continue,
            other => return other,
        }
    }
}

// ──────────────────────────────────────────────
// 1. Breakpoint hit → inspect locals → continue
// ──────────────────────────────────────────────

#[test]
fn breakpoint_hit_and_inspect_locals() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local x = 10
local y = 20
local z = x + y
return z
"#;

    // BP on line 4 (local z = x + y)
    ctrl.set_breakpoint("@test_bp.lua", 4, None).unwrap();

    let handle = spawn_lua(lua.clone(), code, "@test_bp.lua");

    // Wait for Paused event
    let evt =
        wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should receive Paused event");

    let stack = match &evt {
        DebugEvent::Paused { reason, stack } => {
            assert!(
                matches!(reason, PauseReason::Breakpoint(_)),
                "expected Breakpoint reason, got: {reason:?}"
            );
            assert!(!stack.is_empty(), "stack should not be empty");
            stack
        }
        other => panic!("expected Paused, got: {other:?}"),
    };

    // Use the stack trace to find the correct frame for local inspection.
    let frame = stack
        .iter()
        .find(|f| f.source.contains("test_bp.lua"))
        .expect("stack should contain a frame from @test_bp.lua");

    let locals = ctrl.get_locals(frame.id).unwrap();
    let x_val = locals
        .iter()
        .find(|v| v.name == "x")
        .map(|v| v.value.as_str());
    let y_val = locals
        .iter()
        .find(|v| v.name == "y")
        .map(|v| v.value.as_str());

    assert_eq!(x_val, Some("10"), "local x should be 10");
    assert_eq!(y_val, Some("20"), "local y should be 20");

    // Resume
    ctrl.continue_execution().unwrap();

    // Lua should complete without error
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 2. Step Into
// ──────────────────────────────────────────────

#[test]
fn step_into_stops_on_next_line() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.set_stop_on_entry(true);
    session.attach(&lua).unwrap();

    let code = r#"
local a = 1
local b = 2
local c = a + b
"#;

    let handle = spawn_lua(lua.clone(), code, "@step.lua");

    // First stop: entry
    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should stop on entry");
    assert!(matches!(
        evt,
        DebugEvent::Paused {
            reason: PauseReason::Entry,
            ..
        }
    ));

    // Step into: should stop at the next line
    ctrl.step_into().unwrap();

    let evt2 =
        wait_paused_timeout(&ctrl, Duration::from_secs(5)).expect("should stop after step_into");
    match evt2 {
        DebugEvent::Paused { reason, .. } => {
            assert_eq!(reason, PauseReason::Step, "should be Step reason");
        }
        other => panic!("expected Paused, got: {other:?}"),
    }

    // Continue to finish
    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 3. Step Over (does not descend into calls)
// ──────────────────────────────────────────────

#[test]
fn step_over_skips_function_body() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local function add(a, b)
    local sum = a + b
    return sum
end
local result = add(3, 4)
local done = true
"#;

    // Break on `local result = add(3, 4)` — line 6
    ctrl.set_breakpoint("@step_over.lua", 6, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@step_over.lua");

    // Wait for BP
    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    assert!(matches!(
        evt,
        DebugEvent::Paused {
            reason: PauseReason::Breakpoint(_),
            ..
        }
    ));

    // Step over: should skip the body of add() and land on line 7
    ctrl.step_over().unwrap();

    let evt2 =
        wait_paused_timeout(&ctrl, Duration::from_secs(5)).expect("should stop after step_over");
    match &evt2 {
        DebugEvent::Paused { reason, stack } => {
            assert_eq!(*reason, PauseReason::Step);
            // The top frame's line should be 7 (local done = true)
            if let Some(frame) = stack.first() {
                assert_eq!(frame.line, Some(7), "should land on line 7 after step over");
            }
        }
        other => panic!("expected Paused, got: {other:?}"),
    }

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 4. Evaluate expression while paused
// ──────────────────────────────────────────────

#[test]
fn evaluate_expression_while_paused() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local val = 42
local dummy = val + 1
"#;

    // BP on line 3
    ctrl.set_breakpoint("@eval.lua", 3, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@eval.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    assert!(matches!(evt, DebugEvent::Paused { .. }));

    // Evaluate a global expression
    let result = ctrl.evaluate("1 + 2", None).unwrap();
    assert_eq!(result, "3");

    // Evaluate string expression
    let result = ctrl.evaluate("'hello' .. ' world'", None).unwrap();
    assert_eq!(result, "\"hello world\"");

    // Statement evaluation is rejected (expression-only for safety).
    let err = ctrl.evaluate("for i = 1, 3 do end", None);
    assert!(err.is_err(), "statement should be rejected");

    // Assignment is also a statement — should be rejected.
    let err = ctrl.evaluate("_G.test_var = 99", None);
    assert!(err.is_err(), "assignment statement should be rejected");

    // Evaluate invalid expression
    let err = ctrl.evaluate("this_is_undefined()", None);
    assert!(err.is_err(), "should error on undefined function");

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 5. Stack trace inspection
// ──────────────────────────────────────────────

#[test]
fn stack_trace_shows_nested_calls() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local function inner()
    local x = 1
end
local function outer()
    inner()
end
outer()
"#;

    // BP on line 3 (inside inner())
    ctrl.set_breakpoint("@stack.lua", 3, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@stack.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    assert!(matches!(evt, DebugEvent::Paused { .. }));

    let stack = ctrl.get_stack_trace().unwrap();
    assert!(
        stack.len() >= 2,
        "should have at least 2 frames (inner + outer), got {}",
        stack.len()
    );

    // The stack should contain function names "inner" and "outer"
    let names: Vec<&str> = stack.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.contains(&"inner"),
        "stack should contain 'inner', got: {names:?}"
    );
    assert!(
        names.contains(&"outer"),
        "stack should contain 'outer', got: {names:?}"
    );

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 6. Breakpoint management (set / list / remove)
// ──────────────────────────────────────────────

#[test]
fn breakpoint_lifecycle() {
    let (_session, ctrl) = DebugSession::new();

    // Set two breakpoints
    let id1 = ctrl.set_breakpoint("@test.lua", 10, None).unwrap();
    let id2 = ctrl.set_breakpoint("@test.lua", 20, Some("x > 5")).unwrap();

    // List should return both
    let bps = ctrl.list_breakpoints().unwrap();
    assert_eq!(bps.len(), 2);

    // Remove the first
    let removed = ctrl.remove_breakpoint(id1).unwrap();
    assert!(removed);

    // List should return only one
    let bps = ctrl.list_breakpoints().unwrap();
    assert_eq!(bps.len(), 1);
    assert_eq!(bps[0].id, id2);

    // Remove non-existent
    let removed = ctrl.remove_breakpoint(id1).unwrap();
    assert!(!removed);
}

// ──────────────────────────────────────────────
// 7. Disconnect terminates session
// ──────────────────────────────────────────────

#[test]
fn disconnect_terminates_session() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.set_stop_on_entry(true);
    session.attach(&lua).unwrap();

    assert!(matches!(ctrl.state(), SessionState::Running));

    let handle = spawn_lua(lua.clone(), "local x = 1", "@disc.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should stop on entry");
    assert!(matches!(evt, DebugEvent::Paused { .. }));

    // Disconnect
    ctrl.disconnect().unwrap();

    // Lua thread should complete (disconnect unblocks the paused loop)
    handle.join().unwrap().unwrap();

    assert!(matches!(ctrl.state(), SessionState::Terminated));
}

// ──────────────────────────────────────────────
// 8. Multiple breakpoints in same script
// ──────────────────────────────────────────────

#[test]
fn multiple_breakpoints_sequential_hits() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local a = 1
local b = 2
local c = 3
local d = 4
"#;

    // BP on line 2 and line 4
    ctrl.set_breakpoint("@multi.lua", 2, None).unwrap();
    ctrl.set_breakpoint("@multi.lua", 4, None).unwrap();

    let handle = spawn_lua(lua.clone(), code, "@multi.lua");

    // First hit: line 2
    let evt1 =
        wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit first breakpoint");
    match &evt1 {
        DebugEvent::Paused { reason, stack } => {
            assert!(matches!(reason, PauseReason::Breakpoint(_)));
            if let Some(frame) = stack.first() {
                assert_eq!(frame.line, Some(2), "first BP should be on line 2");
            }
        }
        other => panic!("expected Paused, got: {other:?}"),
    }

    ctrl.continue_execution().unwrap();

    // Drain events until we get the second Paused (skip Continued events)
    let mut found_second_pause = false;
    for _ in 0..5 {
        let evt2 = wait_event_timeout(&ctrl, Duration::from_secs(5))
            .expect("should receive event after continue");
        match &evt2 {
            DebugEvent::Paused { reason, stack } => {
                assert!(matches!(reason, PauseReason::Breakpoint(_)));
                if let Some(frame) = stack.first() {
                    assert_eq!(frame.line, Some(4), "second BP should be on line 4");
                }
                found_second_pause = true;
                break;
            }
            DebugEvent::Continued => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(found_second_pause, "should hit second breakpoint");

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 9. Stop on entry
// ──────────────────────────────────────────────

#[test]
fn stop_on_entry_pauses_at_first_line() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.set_stop_on_entry(true);
    session.attach(&lua).unwrap();

    let code = "local x = 1\nlocal y = 2\n";
    let handle = spawn_lua(lua.clone(), code, "@entry.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should stop on entry");
    match &evt {
        DebugEvent::Paused { reason, stack } => {
            assert_eq!(*reason, PauseReason::Entry, "should be Entry reason");
            if let Some(frame) = stack.first() {
                assert_eq!(frame.line, Some(1), "entry should pause on line 1");
            }
        }
        other => panic!("expected Paused, got: {other:?}"),
    }

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 10. Upvalue inspection
// ──────────────────────────────────────────────

#[test]
fn inspect_upvalues() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local captured = "upval"
local function inner()
    local x = captured
end
inner()
"#;

    // BP on line 4 (local x = captured), inside inner()
    ctrl.set_breakpoint("@upval.lua", 4, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@upval.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    let stack = match &evt {
        DebugEvent::Paused { stack, .. } => stack,
        other => panic!("expected Paused, got: {other:?}"),
    };

    // Use the stack trace to find the inner() frame.
    let frame = stack
        .iter()
        .find(|f| f.source.contains("upval.lua"))
        .expect("stack should contain a frame from @upval.lua");

    let upvalues = ctrl.get_upvalues(frame.id).unwrap();
    let captured = upvalues
        .iter()
        .find(|v| v.name == "captured")
        .expect("should find upvalue 'captured'");
    assert_eq!(captured.value, "\"upval\"");

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 11. Tail call does not break step_over depth
// ──────────────────────────────────────────────

/// Verifies that tail calls do not inflate call_depth.
///
/// Lua 5.4 Reference Manual (lua_Hook):
///   "for a tail call; in this case, there will be no
///    corresponding return event."
///
/// A tail call replaces the current frame, so depth must stay
/// constant.  If depth were incremented, step_over after a tail
/// call chain would never pause (depth would be permanently too
/// high).
#[test]
fn step_over_works_across_tail_call() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local function c()
    return 99
end
local function b()
    return c()
end
local function a()
    return b()
end
local result = a()
local done = true
"#;

    // BP on line 11: `local result = a()`
    ctrl.set_breakpoint("@tail.lua", 11, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@tail.lua");

    // Wait for BP hit
    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    assert!(matches!(
        evt,
        DebugEvent::Paused {
            reason: PauseReason::Breakpoint(_),
            ..
        }
    ));

    // Step over: a() → b() → c() are all tail calls.
    // step_over must land on line 12 (`local done = true`),
    // NOT get stuck inside the tail call chain.
    ctrl.step_over().unwrap();

    let evt2 =
        wait_paused_timeout(&ctrl, Duration::from_secs(5)).expect("should stop after step_over");
    match &evt2 {
        DebugEvent::Paused { reason, stack } => {
            assert_eq!(*reason, PauseReason::Step);
            if let Some(frame) = stack.first() {
                assert_eq!(
                    frame.line,
                    Some(12),
                    "step_over across tail calls should land on line 12, got {:?}",
                    frame.line
                );
            }
        }
        other => panic!("expected Paused, got: {other:?}"),
    }

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 12. Pause while running
// ──────────────────────────────────────────────

/// Verifies that `pause()` stops the VM while it is running.
///
/// Uses a breakpoint to synchronize, then removes it and calls
/// `pause()` before `continue_execution()`.  The VM should pause
/// on the very next line with `PauseReason::UserPause`.
#[test]
fn pause_while_running_stops_vm() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local x = 0
for i = 1, 1000000 do
    x = x + i
end
"#;

    // Breakpoint on first loop iteration to synchronize.
    let bp_id = ctrl.set_breakpoint("@pause_run.lua", 3, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@pause_run.lua");

    // Wait for breakpoint.
    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    assert!(matches!(
        evt,
        DebugEvent::Paused {
            reason: PauseReason::Breakpoint(_),
            ..
        }
    ));

    // Remove breakpoint, request pause, then continue.
    // The VM will resume and immediately pause on the next line.
    ctrl.remove_breakpoint(bp_id).unwrap();
    ctrl.pause().unwrap();
    ctrl.continue_execution().unwrap();

    let evt2 = wait_paused_timeout(&ctrl, Duration::from_secs(5))
        .expect("should pause after pause() request");
    match &evt2 {
        DebugEvent::Paused { reason, .. } => {
            assert_eq!(
                *reason,
                PauseReason::UserPause,
                "should be UserPause reason"
            );
        }
        other => panic!("expected Paused with UserPause, got: {other:?}"),
    }

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 13. Detach while paused unblocks VM
// ──────────────────────────────────────────────

#[test]
fn detach_while_paused_unblocks_vm() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.set_stop_on_entry(true);
    session.attach(&lua).unwrap();

    let handle = spawn_lua(lua.clone(), "local x = 1\nlocal y = 2", "@detach.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should stop on entry");
    assert!(matches!(evt, DebugEvent::Paused { .. }));

    // Detach while paused — should unblock the VM thread.
    session.detach(&lua);

    // VM thread should complete without hanging.
    handle.join().unwrap().unwrap();

    assert!(matches!(ctrl.state(), SessionState::Terminated));
}

// ──────────────────────────────────────────────
// 14. CompletionNotifier emits Terminated on normal completion
// ──────────────────────────────────────────────

/// Verifies that `CompletionNotifier::notify` emits a clean
/// `Terminated` event when Lua code completes normally (no breakpoints,
/// no pause).
#[test]
fn completion_notifier_emits_terminated_on_success() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let notifier = session.completion_notifier();
    let lua_clone = lua.clone();
    let handle = thread::spawn(move || {
        let result = lua_clone
            .load("local x = 1 + 2")
            .set_name("@done.lua")
            .exec();
        notifier.notify(result.err().map(|e| e.to_string()));
    });

    // Should receive Terminated (not an error from disconnected channel).
    let evt =
        wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should receive Terminated event");
    match &evt {
        DebugEvent::Terminated { error, .. } => {
            assert!(error.is_none(), "normal completion should have no error");
        }
        other => panic!("expected Terminated, got: {other:?}"),
    }

    assert!(matches!(ctrl.state(), SessionState::Terminated));
    handle.join().unwrap();
}

// ──────────────────────────────────────────────
// 15. CompletionNotifier emits Terminated with error on Lua failure
// ──────────────────────────────────────────────

/// Verifies that `CompletionNotifier::notify` reports Lua runtime
/// errors via the `error` field of `Terminated`.
#[test]
fn completion_notifier_emits_terminated_on_error() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let notifier = session.completion_notifier();
    let lua_clone = lua.clone();
    let handle = thread::spawn(move || {
        let result = lua_clone.load("error('boom')").set_name("@err.lua").exec();
        notifier.notify(result.err().map(|e| e.to_string()));
    });

    let evt =
        wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should receive Terminated event");
    match &evt {
        DebugEvent::Terminated { error, .. } => {
            let msg = error.as_deref().expect("should contain error message");
            assert!(
                msg.contains("boom"),
                "error should mention 'boom', got: {msg}"
            );
        }
        other => panic!("expected Terminated, got: {other:?}"),
    }

    assert!(matches!(ctrl.state(), SessionState::Terminated));
    handle.join().unwrap();
}

// ──────────────────────────────────────────────
// 16. CompletionNotifier::notify_with_result propagates result
// ──────────────────────────────────────────────

#[test]
fn completion_notifier_propagates_result() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let notifier = session.completion_notifier();
    let lua_clone = lua.clone();
    let handle = thread::spawn(move || {
        let result = lua_clone
            .load("return 1 + 2")
            .set_name("@result.lua")
            .eval::<i64>();
        match result {
            Ok(val) => notifier.notify_with_result(Some(val.to_string()), None),
            Err(e) => notifier.notify_with_result(None, Some(e.to_string())),
        }
    });

    let evt =
        wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should receive Terminated event");
    match &evt {
        DebugEvent::Terminated { result, error } => {
            assert!(error.is_none(), "should have no error");
            let val = result.as_deref().expect("should carry result value");
            assert_eq!(val, "3", "result should be '3'");
        }
        other => panic!("expected Terminated, got: {other:?}"),
    }

    assert!(matches!(ctrl.state(), SessionState::Terminated));
    handle.join().unwrap();
}

// ──────────────────────────────────────────────
// 17. Frame-scoped expression evaluation (locals)
// ──────────────────────────────────────────────

/// Verifies that `evaluate` with a `frame_id` can access local
/// variables of the paused frame.
#[test]
fn evaluate_expression_in_frame_scope() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local x = 10
local y = 20
local z = x + y
return z
"#;

    // BP on line 4 (local z = x + y) — x=10, y=20 are in scope.
    ctrl.set_breakpoint("@frame_eval.lua", 4, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@frame_eval.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    let stack = match &evt {
        DebugEvent::Paused { stack, .. } => stack,
        other => panic!("expected Paused, got: {other:?}"),
    };

    let frame = stack
        .iter()
        .find(|f| f.source.contains("frame_eval.lua"))
        .expect("stack should contain frame_eval.lua frame");

    // Evaluate expression referencing local variables.
    let result = ctrl.evaluate("x + y", Some(frame.id)).unwrap();
    assert_eq!(result, "30", "x(10) + y(20) should be 30");

    // Single local variable.
    let result = ctrl.evaluate("x * 2", Some(frame.id)).unwrap();
    assert_eq!(result, "20", "x(10) * 2 should be 20");

    // Global function accessible through __index fallback.
    let result = ctrl.evaluate("type(x)", Some(frame.id)).unwrap();
    assert_eq!(result, "\"number\"", "type(x) should be 'number'");

    // Global scope should NOT see locals (nil + nil → runtime error).
    let err = ctrl.evaluate("x + y", None);
    assert!(err.is_err(), "global scope should not see local variables");

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 18. Frame-scoped evaluation with upvalues
// ──────────────────────────────────────────────

/// Verifies that frame-scoped evaluation can access both locals and
/// upvalues (captured variables from enclosing scopes).
#[test]
fn evaluate_expression_with_upvalues_in_frame() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local captured = 42
local function inner()
    local x = 1
    local y = captured + x
end
inner()
"#;

    // BP on line 5 (local y = captured + x), inside inner().
    ctrl.set_breakpoint("@upval_eval.lua", 5, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@upval_eval.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    let stack = match &evt {
        DebugEvent::Paused { stack, .. } => stack,
        other => panic!("expected Paused, got: {other:?}"),
    };

    let frame = stack
        .iter()
        .find(|f| f.name == "inner")
        .expect("stack should contain inner() frame");

    // Upvalue + local combined.
    let result = ctrl.evaluate("captured + x", Some(frame.id)).unwrap();
    assert_eq!(result, "43", "captured(42) + x(1) should be 43");

    // Upvalue alone.
    let result = ctrl.evaluate("captured", Some(frame.id)).unwrap();
    assert_eq!(result, "42", "captured should be 42");

    // Global function via __index.
    let result = ctrl.evaluate("type(captured)", Some(frame.id)).unwrap();
    assert_eq!(result, "\"number\"", "type(captured) should be 'number'");

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 19. Local shadows upvalue in frame-scoped eval
// ──────────────────────────────────────────────

/// Verifies that a local variable shadows an upvalue of the same name
/// in frame-scoped evaluation, matching Lua's scoping rules.
#[test]
fn evaluate_local_shadows_upvalue() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local x = 100
local function inner()
    local x = 1
    local y = x + 1
end
inner()
"#;

    // BP on line 5 (local y = x + 1) — local x=1 shadows upvalue x=100.
    ctrl.set_breakpoint("@shadow.lua", 5, None).unwrap();
    let handle = spawn_lua(lua.clone(), code, "@shadow.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit breakpoint");
    let stack = match &evt {
        DebugEvent::Paused { stack, .. } => stack,
        other => panic!("expected Paused, got: {other:?}"),
    };

    let frame = stack
        .iter()
        .find(|f| f.name == "inner")
        .expect("stack should contain inner() frame");

    // The local x=1 should shadow the upvalue x=100.
    let result = ctrl.evaluate("x", Some(frame.id)).unwrap();
    assert_eq!(result, "1", "local x should shadow upvalue x");

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 20. Conditional breakpoint — fires only when true
// ──────────────────────────────────────────────

/// Verifies that a conditional breakpoint only pauses when the
/// condition expression evaluates to a truthy value.
#[test]
fn conditional_breakpoint_fires_only_when_true() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local result = 0
for i = 1, 20 do
    result = result + i
end
return result
"#;

    // Conditional BP on line 4 — should only fire when i == 10.
    ctrl.set_breakpoint("@cond.lua", 4, Some("i == 10"))
        .unwrap();
    let handle = spawn_lua(lua.clone(), code, "@cond.lua");

    // Should receive exactly one Paused event (when i == 10).
    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should hit conditional bp");
    let stack = match &evt {
        DebugEvent::Paused { reason, stack } => {
            assert!(
                matches!(reason, PauseReason::Breakpoint(_)),
                "expected Breakpoint reason, got: {reason:?}"
            );
            stack
        }
        other => panic!("expected Paused, got: {other:?}"),
    };

    // Verify `i` is 10 at the paused frame.
    let frame = stack
        .iter()
        .find(|f| f.source.contains("cond.lua"))
        .expect("stack should contain cond.lua frame");
    let locals = ctrl.get_locals(frame.id).unwrap();
    let i_val = locals
        .iter()
        .find(|v| v.name == "i")
        .map(|v| v.value.as_str());
    assert_eq!(i_val, Some("10"), "i should be 10 when condition fires");

    // Continue — the loop finishes without hitting the condition again
    // (i == 10 only matches once in a for loop from 1 to 20).
    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 20b. Conditional breakpoint set AFTER entry pause (MCP flow)
// ──────────────────────────────────────────────

/// Reproduces the MCP flow: stop_on_entry → set conditional BP while
/// paused → continue.  This differs from test #20 where the BP is set
/// before code starts.
#[test]
fn conditional_breakpoint_set_after_entry_pause() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.set_stop_on_entry(true);
    session.attach(&lua).unwrap();

    let code = r#"
local result = 0
for i = 1, 20 do
    result = result + i
end
return result
"#;

    let handle = spawn_lua(lua.clone(), code, "@cond_mcp.lua");

    // Wait for entry pause.
    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should stop on entry");
    assert!(matches!(
        evt,
        DebugEvent::Paused {
            reason: PauseReason::Entry,
            ..
        }
    ));

    // Set conditional breakpoint WHILE paused (MCP flow).
    ctrl.set_breakpoint("@cond_mcp.lua", 4, Some("i == 10"))
        .unwrap();

    // Continue execution.
    ctrl.continue_execution().unwrap();

    // Should pause exactly when i == 10.
    let evt2 =
        wait_paused_timeout(&ctrl, Duration::from_secs(5)).expect("should hit conditional bp");
    let stack = match &evt2 {
        DebugEvent::Paused { reason, stack } => {
            assert!(
                matches!(reason, PauseReason::Breakpoint(_)),
                "expected Breakpoint reason, got: {reason:?}"
            );
            stack
        }
        other => panic!("expected Paused, got: {other:?}"),
    };

    let frame = stack
        .iter()
        .find(|f| f.source.contains("cond_mcp.lua"))
        .expect("stack should contain cond_mcp.lua frame");
    let locals = ctrl.get_locals(frame.id).unwrap();
    let i_val = locals
        .iter()
        .find(|v| v.name == "i")
        .map(|v| v.value.as_str());
    assert_eq!(
        i_val,
        Some("10"),
        "i should be 10 when condition fires (MCP flow)"
    );

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 21. Conditional breakpoint with syntax error is skipped
// ──────────────────────────────────────────────

/// A breakpoint whose condition has a syntax error should be treated
/// as "condition false" — the VM continues without pausing.
#[test]
fn conditional_breakpoint_bad_syntax_skipped() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.set_stop_on_entry(true);
    session.attach(&lua).unwrap();

    let code = r#"
local x = 1
local y = 2
local z = 3
"#;

    // BP with invalid syntax on line 3.
    ctrl.set_breakpoint("@badsyntax.lua", 3, Some("if then end"))
        .unwrap();
    let handle = spawn_lua(lua.clone(), code, "@badsyntax.lua");

    // First event should be stop-on-entry (line 2).
    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5)).expect("should stop on entry");
    assert!(matches!(
        evt,
        DebugEvent::Paused {
            reason: PauseReason::Entry,
            ..
        }
    ));

    // Continue — the bad-syntax BP on line 3 should NOT fire.
    ctrl.continue_execution().unwrap();

    // The script should complete without another pause.
    // Any event we receive should be Continued or Terminated,
    // never a Paused/Breakpoint.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match wait_event_timeout(&ctrl, remaining) {
            Some(DebugEvent::Paused { reason, .. }) => {
                panic!("bad-syntax breakpoint should not fire, got: {reason:?}");
            }
            Some(DebugEvent::Continued) | Some(DebugEvent::Output { .. }) => continue,
            Some(DebugEvent::Terminated { .. }) | None => break,
        }
    }

    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 22. Conditional breakpoint accesses local variables
// ──────────────────────────────────────────────

/// Verifies that breakpoint conditions can reference local variables
/// that are in scope at the breakpoint line.
#[test]
fn conditional_breakpoint_accesses_locals() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local name = "alice"
local marker = name
local done = true
"#;

    // BP on line 3 — condition references local `name`.
    ctrl.set_breakpoint("@condlocal.lua", 3, Some("name == 'alice'"))
        .unwrap();
    let handle = spawn_lua(lua.clone(), code, "@condlocal.lua");

    let evt = wait_event_timeout(&ctrl, Duration::from_secs(5))
        .expect("condition referencing local should fire");
    assert!(matches!(
        evt,
        DebugEvent::Paused {
            reason: PauseReason::Breakpoint(_),
            ..
        }
    ));

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 23. Conditional breakpoint — false condition never pauses
// ──────────────────────────────────────────────

/// Verifies that a breakpoint whose condition is always false never
/// causes a pause, even though the line is executed.
#[test]
fn conditional_breakpoint_always_false_never_pauses() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local x = 1
local y = 2
local z = 3
"#;

    // BP on line 3 with always-false condition.
    ctrl.set_breakpoint("@nofire.lua", 3, Some("false"))
        .unwrap();
    let handle = spawn_lua(lua.clone(), code, "@nofire.lua");

    // The script should complete without any Paused event.
    let evt = wait_event_timeout(&ctrl, Duration::from_secs(2));
    match evt {
        Some(DebugEvent::Paused { reason, .. }) => {
            panic!("always-false condition should not fire, got: {reason:?}");
        }
        _ => { /* Terminated, Continued, or timeout — all acceptable */ }
    }

    handle.join().unwrap().unwrap();
}

// ──────────────────────────────────────────────
// 25. Conditional breakpoint on for-loop header
// ──────────────────────────────────────────────

/// Verifies that a conditional breakpoint on a for-loop header line
/// can access the loop control variable (which Lua 5.4 reports as
/// `(temporary)` at the FORLOOP instruction).
#[test]
fn conditional_breakpoint_for_loop_variable() {
    let lua = Arc::new(Lua::new());
    let (session, ctrl) = DebugSession::new();
    session.attach(&lua).unwrap();

    let code = r#"
local sum = 0
for i = 1, 10 do
  sum = sum + i
end
"#;

    // BP on line 3 (for header) — condition references `i`.
    ctrl.set_breakpoint("@forloop.lua", 3, Some("i == 5"))
        .unwrap();
    let handle = spawn_lua(lua.clone(), code, "@forloop.lua");

    let evt = wait_paused_timeout(&ctrl, Duration::from_secs(5));
    match &evt {
        Some(DebugEvent::Paused {
            reason: PauseReason::Breakpoint(_),
            ..
        }) => {
            // Check that locals include `i` with value 5
            let locals = ctrl.get_locals(0).unwrap();
            eprintln!(
                "locals: {:?}",
                locals
                    .iter()
                    .map(|v| format!("{}={}", v.name, v.value))
                    .collect::<Vec<_>>()
            );
            let i_var = locals.iter().find(|v| v.name == "i");
            assert!(
                i_var.is_some(),
                "locals should contain 'i', got: {:?}",
                locals.iter().map(|v| &v.name).collect::<Vec<_>>()
            );
            assert_eq!(i_var.unwrap().value, "5", "i should be 5 when BP fires");
        }
        other => panic!("expected Paused(Breakpoint) for 'i == 5', got: {other:?}"),
    }

    ctrl.continue_execution().unwrap();
    handle.join().unwrap().unwrap();
}
