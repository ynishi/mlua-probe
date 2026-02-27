//! Integration tests for the mlua-probe testing framework.

use mlua::prelude::*;
use mlua_probe_core::testing;

// ── Helper ──────────────────────────────────────────────────────

fn run(code: &str) -> testing::TestSummary {
    testing::framework::run_tests(code, "@test.lua").expect("test execution should not fail")
}

// ── Normal cases ────────────────────────────────────────────────

#[test]
fn all_tests_pass() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('math', function()
            it('adds', function()
                expect(1 + 1).to.equal(2)
            end)
            it('subtracts', function()
                expect(3 - 1).to.equal(2)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.total, 2);
    assert!(summary.tests.iter().all(|t| t.passed));
}

#[test]
fn mixed_pass_and_fail() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('mixed', function()
            it('passes', function()
                expect(true).to.be.truthy()
            end)
            it('fails', function()
                expect(1).to.equal(2)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.total, 2);

    let failed = summary
        .tests
        .iter()
        .find(|t| !t.passed)
        .expect("should have a failed test");
    assert_eq!(failed.name, "fails");
    assert!(failed.error.is_some());
}

#[test]
fn suite_path_is_recorded() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('outer', function()
            describe('inner', function()
                it('test', function()
                    expect(1).to.equal(1)
                end)
            end)
        end)
    "#);

    assert_eq!(summary.total, 1);
    assert_eq!(summary.tests[0].suite, "outer > inner");
    assert_eq!(summary.tests[0].name, "test");
}

#[test]
fn spy_records_calls() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('spy', function()
            it('tracks call count', function()
                local fn = lust.spy(function(x) return x * 2 end)
                fn(5)
                fn(10)
                expect(#fn).to.equal(2)
            end)
            it('records arguments', function()
                local fn = lust.spy(function(x, y) return x + y end)
                fn(3, 4)
                expect(fn[1][1]).to.equal(3)
                expect(fn[1][2]).to.equal(4)
            end)
            it('passes through return values', function()
                local fn = lust.spy(function(x) return x * 2 end)
                expect(fn(5)).to.equal(10)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 3);
    assert_eq!(summary.failed, 0);
}

#[test]
fn before_after_hooks() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('hooks', function()
            local counter = 0
            lust.before(function() counter = counter + 1 end)

            it('first', function()
                expect(counter).to.equal(1)
            end)
            it('second', function()
                expect(counter).to.equal(2)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
}

#[test]
fn error_detection_and_pattern_match() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('errors', function()
            it('detects failure', function()
                expect(function() error("boom") end).to.fail()
            end)
            it('matches pattern', function()
                expect(function() error("something went wrong") end).to.fail.with("went wrong")
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
}

#[test]
fn negation_assertions() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('negation', function()
            it('not equal', function()
                expect(1).to_not.equal(2)
            end)
            it('not exist', function()
                expect(nil).to_not.exist()
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
}

#[test]
fn table_deep_equality() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('tables', function()
            it('deep equal', function()
                expect({1, 2, {3, 4}}).to.equal({1, 2, {3, 4}})
            end)
            it('deep not equal', function()
                expect({1, 2}).to_not.equal({1, 3})
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
}

// ── Error cases ─────────────────────────────────────────────────

#[test]
fn all_tests_fail() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('failures', function()
            it('wrong value', function()
                expect(1).to.equal(999)
            end)
            it('not truthy', function()
                expect(false).to.be.truthy()
            end)
        end)
    "#);

    assert_eq!(summary.passed, 0);
    assert_eq!(summary.failed, 2);
    assert!(summary.tests.iter().all(|t| !t.passed));
    assert!(summary.tests.iter().all(|t| t.error.is_some()));
}

#[test]
fn syntax_error_in_test_code_returns_err() {
    let result = testing::framework::run_tests("this is not valid lua !!!", "@bad.lua");
    assert!(result.is_err());
}

#[test]
fn runtime_error_outside_it_returns_err() {
    let result = testing::framework::run_tests(r#"error("top-level crash")"#, "@crash.lua");
    assert!(result.is_err());
}

#[test]
fn empty_test_suite() {
    let summary = run(r#"
        local describe = lust.describe
        describe('empty', function() end)
    "#);

    assert_eq!(summary.passed, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.total, 0);
}

// ── test_doubles: spy ───────────────────────────────────────────

#[test]
fn doubles_spy_call_count() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('test_doubles.spy', function()
            it('tracks call count', function()
                local s = test_doubles.spy(function(x) return x * 2 end)
                s(5)
                s(10)
                expect(s:call_count()).to.equal(2)
            end)
            it('#spy returns call count', function()
                local s = test_doubles.spy(function() end)
                s()
                s()
                s()
                expect(#s).to.equal(3)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
}

#[test]
fn doubles_spy_call_through() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('spy call-through', function()
            it('passes through return values', function()
                local s = test_doubles.spy(function(x) return x * 2 end)
                expect(s(5)).to.equal(10)
            end)
            it('passes through multiple return values', function()
                local s = test_doubles.spy(function(a, b) return a + b, a - b end)
                local sum, diff = s(10, 3)
                expect(sum).to.equal(13)
                expect(diff).to.equal(7)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
}

#[test]
fn doubles_spy_call_args() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('spy call_args', function()
            it('records arguments per call', function()
                local s = test_doubles.spy(function() end)
                s(1, "a")
                s(2, "b")
                local args1 = s:call_args(1)
                expect(args1[1]).to.equal(1)
                expect(args1[2]).to.equal("a")
                local args2 = s:call_args(2)
                expect(args2[1]).to.equal(2)
                expect(args2[2]).to.equal("b")
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}

#[test]
fn doubles_spy_was_called_with() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('spy was_called_with', function()
            it('detects matching call', function()
                local s = test_doubles.spy(function() end)
                s(1, "hello")
                s(2, "world")
                expect(s:was_called_with(1, "hello")).to.equal(true)
                expect(s:was_called_with(2, "world")).to.equal(true)
            end)
            it('rejects non-matching call', function()
                local s = test_doubles.spy(function() end)
                s(1, "hello")
                expect(s:was_called_with(1, "goodbye")).to.equal(false)
                expect(s:was_called_with(99)).to.equal(false)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
}

#[test]
fn doubles_spy_reset() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('spy reset', function()
            it('clears call records', function()
                local s = test_doubles.spy(function() end)
                s(1)
                s(2)
                expect(s:call_count()).to.equal(2)
                s:reset()
                expect(s:call_count()).to.equal(0)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}

#[test]
fn doubles_spy_without_original() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('spy without original', function()
            it('records calls and returns nil', function()
                local s = test_doubles.spy()
                local result = s(42)
                expect(s:call_count()).to.equal(1)
                expect(result).to_not.exist()
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}

// ── test_doubles: stub ──────────────────────────────────────────

#[test]
fn doubles_stub_returns_fixed_value() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('test_doubles.stub', function()
            it('returns the stubbed value', function()
                local st = test_doubles.stub()
                st:returns(42)
                expect(st()).to.equal(42)
                expect(st()).to.equal(42)
            end)
            it('returns multiple values', function()
                local st = test_doubles.stub()
                st:returns("a", "b")
                local x, y = st()
                expect(x).to.equal("a")
                expect(y).to.equal("b")
            end)
            it('also records calls', function()
                local st = test_doubles.stub()
                st:returns(0)
                st(1)
                st(2)
                expect(st:call_count()).to.equal(2)
                expect(st:was_called_with(1)).to.equal(true)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 3);
    assert_eq!(summary.failed, 0);
}

// ── test_doubles: spy_on ────────────────────────────────────────

#[test]
fn doubles_spy_on_table_method() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('test_doubles.spy_on', function()
            it('replaces method and calls through', function()
                local obj = { greet = function(name) return "hello " .. name end }
                local s = test_doubles.spy_on(obj, "greet")
                local result = obj.greet("world")
                expect(result).to.equal("hello world")
                expect(s:call_count()).to.equal(1)
                expect(s:was_called_with("world")).to.equal(true)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}

#[test]
fn doubles_spy_on_revert() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('spy_on revert', function()
            it('restores original method', function()
                local obj = { add = function(a, b) return a + b end }
                local original = obj.add
                local s = test_doubles.spy_on(obj, "add")
                -- After spy_on, obj.add is the spy UserData
                obj.add(1, 2)
                expect(s:call_count()).to.equal(1)
                -- Revert
                s:revert()
                -- obj.add should be the original function again
                expect(obj.add(10, 20)).to.equal(30)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}

// ── test_doubles: spy converts to stub via returns() ────────────

#[test]
fn doubles_spy_returns_converts_to_stub() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('spy returns()', function()
            it('overrides call-through with fixed value', function()
                local s = test_doubles.spy(function() return "original" end)
                expect(s()).to.equal("original")
                s:returns("stubbed")
                expect(s()).to.equal("stubbed")
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}

// ── test_doubles: error cases ───────────────────────────────────

#[test]
fn doubles_call_args_out_of_bounds() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('call_args errors', function()
            it('errors on invalid index', function()
                local s = test_doubles.spy(function() end)
                s(1)
                expect(function() s:call_args(0) end).to.fail()
                expect(function() s:call_args(5) end).to.fail()
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}

// ── test_doubles: revert on non-spy_on spy ──────────────────────

#[test]
fn doubles_revert_on_plain_spy_is_noop() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('revert on plain spy', function()
            it('does nothing and does not error', function()
                local s = test_doubles.spy(function() return 1 end)
                s:revert()
                -- spy still works after revert
                expect(s()).to.equal(1)
                expect(s:call_count()).to.equal(1)
            end)
            it('does nothing on stub', function()
                local st = test_doubles.stub()
                st:returns(99)
                st:revert()
                expect(st()).to.equal(99)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 2);
    assert_eq!(summary.failed, 0);
}

// ── test_doubles: reset preserves return_values ─────────────────

#[test]
fn doubles_reset_preserves_return_values() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('reset after returns', function()
            it('clears call history but keeps stub value', function()
                local s = test_doubles.spy(function() return "original" end)
                s:returns("stubbed")
                s(1)
                s(2)
                expect(s:call_count()).to.equal(2)
                s:reset()
                expect(s:call_count()).to.equal(0)
                -- return_values should still be active
                expect(s()).to.equal("stubbed")
            end)
        end)
    "#);

    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}

// ── Comparison assertions ───────────────────────────────────────

#[test]
fn comparison_gt() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('gt', function()
            it('passes when greater', function()
                expect(10).to.be.gt(5)
            end)
            it('fails when equal', function()
                local ok, err = pcall(function() expect(5).to.be.gt(5) end)
                expect(ok).to.equal(false)
                expect(tostring(err)).to.match('greater than')
            end)
            it('fails when less', function()
                local ok = pcall(function() expect(3).to.be.gt(5) end)
                expect(ok).to.equal(false)
            end)
            it('negation works', function()
                expect(3).to_not.be.gt(5)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 4);
    assert_eq!(summary.failed, 0);
}

#[test]
fn comparison_gte() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('gte', function()
            it('passes when greater', function()
                expect(10).to.be.gte(5)
            end)
            it('passes when equal', function()
                expect(5).to.be.gte(5)
            end)
            it('fails when less', function()
                local ok, err = pcall(function() expect(3).to.be.gte(5) end)
                expect(ok).to.equal(false)
                expect(tostring(err)).to.match('greater than or equal')
            end)
        end)
    "#);

    assert_eq!(summary.passed, 3);
    assert_eq!(summary.failed, 0);
}

#[test]
fn comparison_lt() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('lt', function()
            it('passes when less', function()
                expect(3).to.be.lt(5)
            end)
            it('fails when equal', function()
                local ok = pcall(function() expect(5).to.be.lt(5) end)
                expect(ok).to.equal(false)
            end)
            it('fails when greater', function()
                local ok, err = pcall(function() expect(10).to.be.lt(5) end)
                expect(ok).to.equal(false)
                expect(tostring(err)).to.match('less than')
            end)
            it('negation works', function()
                expect(10).to_not.be.lt(5)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 4);
    assert_eq!(summary.failed, 0);
}

#[test]
fn comparison_lte() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('lte', function()
            it('passes when less', function()
                expect(3).to.be.lte(5)
            end)
            it('passes when equal', function()
                expect(5).to.be.lte(5)
            end)
            it('fails when greater', function()
                local ok, err = pcall(function() expect(10).to.be.lte(5) end)
                expect(ok).to.equal(false)
                expect(tostring(err)).to.match('less than or equal')
            end)
        end)
    "#);

    assert_eq!(summary.passed, 3);
    assert_eq!(summary.failed, 0);
}

#[test]
fn have_key_assertion() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('have_key', function()
            it('passes when key exists', function()
                expect({name = "alice", age = 30}).to.have_key("name")
            end)
            it('fails when key missing', function()
                local ok, err = pcall(function()
                    expect({name = "alice"}).to.have_key("email")
                end)
                expect(ok).to.equal(false)
                expect(tostring(err)).to.match('key')
            end)
            it('works with numeric keys', function()
                expect({10, 20, 30}).to.have_key(1)
            end)
            it('negation works', function()
                expect({name = "alice"}).to_not.have_key("email")
            end)
            it('errors on non-table', function()
                local ok = pcall(function() expect("string").to.have_key("x") end)
                expect(ok).to.equal(false)
            end)
        end)
    "#);

    assert_eq!(summary.passed, 5);
    assert_eq!(summary.failed, 0);
}

#[test]
fn have_length_assertion() {
    let summary = run(r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect

        describe('have_length', function()
            it('passes for matching table length', function()
                expect({1, 2, 3}).to.have_length(3)
            end)
            it('passes for matching string length', function()
                expect("hello").to.have_length(5)
            end)
            it('fails for wrong length', function()
                local ok, err = pcall(function()
                    expect({1, 2}).to.have_length(5)
                end)
                expect(ok).to.equal(false)
                expect(tostring(err)).to.match('length')
            end)
            it('negation works', function()
                expect({1, 2, 3}).to_not.have_length(5)
            end)
            it('shows actual and expected in error', function()
                local ok, err = pcall(function()
                    expect({1}).to.have_length(3)
                end)
                expect(ok).to.equal(false)
                expect(tostring(err)).to.match('1')
                expect(tostring(err)).to.match('3')
            end)
        end)
    "#);

    assert_eq!(summary.passed, 5);
    assert_eq!(summary.failed, 0);
}

// ── Register on existing Lua VM ─────────────────────────────────

#[test]
fn register_on_existing_lua_vm() {
    let lua = Lua::new();
    testing::framework::register(&lua).expect("register should succeed");

    lua.load(
        r#"
        local describe, it, expect = lust.describe, lust.it, lust.expect
        describe('inline', function()
            it('works', function()
                expect(42).to.equal(42)
            end)
        end)
    "#,
    )
    .exec()
    .expect("test code should execute");

    let summary = testing::framework::collect_results(&lua).expect("collect should succeed");
    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 0);
}
