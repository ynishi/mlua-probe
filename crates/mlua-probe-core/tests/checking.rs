//! Integration tests for the mlua-probe checking (static analysis) module.

use mlua::prelude::*;
use mlua_probe_core::checking;

// ── Helper ──────────────────────────────────────────────────────

fn run(code: &str) -> checking::LintResult {
    checking::framework::run_lint(code, "@check.lua").expect("lint execution should not fail")
}

/// Filter diagnostics to only UndefinedGlobal/UndefinedField (ignore UnusedVariable).
fn undefined_diagnostics(result: &checking::LintResult) -> Vec<&checking::Diagnostic> {
    result
        .diagnostics
        .iter()
        .filter(|d| {
            d.rule == checking::RuleId::UndefinedGlobal
                || d.rule == checking::RuleId::UndefinedField
        })
        .collect()
}

// ── Normal cases ────────────────────────────────────────────────

#[test]
fn clean_code_has_no_diagnostics() {
    let result = run(r#"
        local x = 1
        local y = x + 2
        print(y)
    "#);

    assert_eq!(result.diagnostics.len(), 0);
    assert_eq!(result.error_count, 0);
    assert_eq!(result.warning_count, 0);
}

#[test]
fn stdlib_globals_are_known() {
    // All locals are used via print() to avoid UnusedVariable diagnostics.
    let result = run(r#"
        local t = {}
        table.insert(t, 1)
        local s = string.format("%d", 42)
        local n = math.floor(3.14)
        print(t, s, n)
    "#);

    let undef = undefined_diagnostics(&result);
    assert!(
        undef.is_empty(),
        "unexpected undefined diagnostics: {undef:?}"
    );
}

#[test]
fn local_variables_are_resolved() {
    let result = run(r#"
        local function add(a, b)
            return a + b
        end
        local result = add(1, 2)
        print(result)
    "#);

    assert_eq!(result.diagnostics.len(), 0);
}

#[test]
fn nested_scopes_resolve_correctly() {
    let result = run(r#"
        local x = 10
        do
            local y = x + 1
            print(y)
        end
        print(x)
    "#);

    assert_eq!(result.diagnostics.len(), 0);
}

#[test]
fn numeric_for_loop_variables_are_scoped() {
    let result = run(r#"
        for i = 1, 10 do
            print(i)
        end
    "#);

    let undef = undefined_diagnostics(&result);
    assert!(
        undef.is_empty(),
        "unexpected undefined diagnostics: {undef:?}"
    );
}

#[test]
fn generic_for_loop_variables_are_false_positive() {
    // mlua-check v0.1.0 known limitation: generic for-loop iterator
    // variables (k, v in `for k, v in pairs(...)`) are not recognized
    // as local bindings, producing false-positive UndefinedGlobal warnings.
    let result = run(r#"
        for k, v in pairs({a = 1}) do
            print(k, v)
        end
    "#);

    let undef = undefined_diagnostics(&result);
    assert_eq!(
        undef.len(),
        2,
        "expected 2 false-positive diagnostics for k, v"
    );
    assert!(undef.iter().any(|d| d.message.contains("'k'")));
    assert!(undef.iter().any(|d| d.message.contains("'v'")));
}

// ── Diagnostics detection ───────────────────────────────────────

#[test]
fn detects_undefined_global() {
    let result = run("unknown_func()");

    assert!(result.warning_count > 0);
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.rule == checking::RuleId::UndefinedGlobal));
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.message.contains("unknown_func")));
}

#[test]
fn detects_undefined_field_on_custom_table() {
    let lua = Lua::new();
    let tbl = lua.create_table().unwrap();
    tbl.set("known", lua.create_function(|_, ()| Ok(())).unwrap())
        .unwrap();
    lua.globals().set("api", tbl).unwrap();

    let engine = checking::framework::register(&lua).expect("register should succeed");

    // Known field: no diagnostics
    let result = engine.lint("api.known()", "@check.lua");
    assert_eq!(result.diagnostics.len(), 0);

    // Unknown field: should produce diagnostic
    let result = engine.lint("api.nonexistent()", "@check.lua");
    assert!(
        result.diagnostics.len() > 0,
        "expected diagnostic for undefined field"
    );
}

#[test]
fn detects_multiple_undefined_globals() {
    let result = run(r#"
        unknown_a()
        unknown_b()
    "#);

    assert!(result.warning_count >= 2);
    let globals: Vec<&str> = result
        .diagnostics
        .iter()
        .filter(|d| d.rule == checking::RuleId::UndefinedGlobal)
        .map(|d| d.message.as_str())
        .collect();
    assert!(globals.iter().any(|m| m.contains("unknown_a")));
    assert!(globals.iter().any(|m| m.contains("unknown_b")));
}

#[test]
fn detects_unused_variable() {
    let result = run(r#"
        local unused = 42
        print("hi")
    "#);

    let unused: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.rule == checking::RuleId::UnusedVariable)
        .collect();
    assert_eq!(unused.len(), 1);
    assert!(unused[0].message.contains("unused"));
}

// ── Severity checks ─────────────────────────────────────────────

#[test]
fn undefined_global_is_warning_severity() {
    let result = run("some_undefined()");

    let diag = result
        .diagnostics
        .iter()
        .find(|d| d.rule == checking::RuleId::UndefinedGlobal)
        .expect("should have undefined global diagnostic");
    assert_eq!(diag.severity, checking::Severity::Warning);
}

// ── LintResult aggregate fields ─────────────────────────────────

#[test]
fn lint_result_counts_are_consistent() {
    let result = run(r#"
        unknown_a()
        unknown_b()
        print("ok")
    "#);

    let actual_warnings = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == checking::Severity::Warning)
        .count();
    let actual_errors = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == checking::Severity::Error)
        .count();

    assert_eq!(result.warning_count, actual_warnings);
    assert_eq!(result.error_count, actual_errors);
}

// ── Register on existing VM ─────────────────────────────────────

#[test]
fn register_on_existing_vm() {
    let lua = Lua::new();
    let engine = checking::framework::register(&lua).expect("register should succeed");

    let result = engine.lint("print('hello')", "@check.lua");
    assert_eq!(result.diagnostics.len(), 0);

    let result = engine.lint("nonexistent()", "@check.lua");
    assert!(result.warning_count > 0);
}

#[test]
fn register_with_custom_globals() {
    let lua = Lua::new();
    let tbl = lua.create_table().unwrap();
    tbl.set("do_thing", lua.create_function(|_, ()| Ok(())).unwrap())
        .unwrap();
    lua.globals().set("my_api", tbl).unwrap();

    let engine = checking::framework::register(&lua).expect("register should succeed");

    let result = engine.lint("my_api.do_thing()", "@check.lua");
    assert_eq!(result.diagnostics.len(), 0);

    let result = engine.lint("my_api.missing_method()", "@check.lua");
    assert!(result.diagnostics.len() > 0);
}

// ── Diagnostic field access ──────────────────────────────────────

#[test]
fn diagnostic_fields_are_populated() {
    let result = run("unknown_fn()");

    let diag = result
        .diagnostics
        .iter()
        .find(|d| d.message.contains("unknown_fn"))
        .expect("should have diagnostic for unknown_fn");

    assert_eq!(diag.rule, checking::RuleId::UndefinedGlobal);
    assert_eq!(diag.severity, checking::Severity::Warning);
    assert!(diag.line > 0);
}

// ── Error/edge cases ────────────────────────────────────────────

#[test]
fn syntax_error_produces_diagnostics_or_err() {
    // emmylua_parser is tolerant — syntax errors may not cause run_lint
    // to return Err. Verify that either an Err is returned, or diagnostics
    // are produced for the invalid code.
    let result = checking::framework::run_lint("this is not valid lua !!!", "@bad.lua");
    match result {
        Err(_) => {} // parser rejected it — fine
        Ok(r) => {
            // Tolerant parse: should at least flag undefined references
            assert!(
                r.diagnostics.len() > 0,
                "tolerant parse should produce diagnostics for invalid code"
            );
        }
    }
}

#[test]
fn empty_code_has_no_diagnostics() {
    let result = run("");
    assert_eq!(result.diagnostics.len(), 0);
}
