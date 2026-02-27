use mlua::prelude::*;

use super::types::{TestResult, TestSummary};

const LUST_LUA: &str = include_str!("../../lua/lust.lua");

/// Register the lust test framework into the given Lua VM.
///
/// After this call, `lust` is available as a global table with
/// `describe`, `it`, `expect`, `before`, `after`, `spy`, and
/// `get_results`.
pub fn register(lua: &Lua) -> LuaResult<()> {
    let lust: LuaTable = lua.load(LUST_LUA).set_name("lust.lua").eval()?;
    lua.globals().set("lust", lust)?;
    Ok(())
}

/// Collect structured test results from the lust framework.
///
/// Call this after all `describe`/`it` blocks have executed.
pub fn collect_results(lua: &Lua) -> LuaResult<TestSummary> {
    let lust: LuaTable = lua.globals().get("lust")?;
    let get_results: LuaFunction = lust.get("get_results")?;
    let results: LuaTable = get_results.call(())?;

    let passed: usize = results.get("passed")?;
    let failed: usize = results.get("failed")?;
    let total: usize = results.get("total")?;

    let tests_table: LuaTable = results.get("tests")?;
    let mut tests = Vec::with_capacity(total);

    for pair in tests_table.pairs::<usize, LuaTable>() {
        let (_, t) = pair?;
        tests.push(TestResult {
            suite: t.get::<String>("suite").unwrap_or_default(),
            name: t.get::<String>("name")?,
            passed: t.get::<bool>("passed")?,
            error: t.get::<Option<String>>("error")?,
        });
    }

    Ok(TestSummary {
        passed,
        failed,
        total,
        tests,
    })
}

/// Run Lua test code with the lust framework pre-loaded.
///
/// Creates a fresh Lua VM, registers lust, executes `code`, and
/// returns the structured test summary.
///
/// Lua's `print` is replaced with a no-op to prevent stdout
/// pollution.  When used inside an MCP stdio transport, any stray
/// `print()` output would corrupt the JSON-RPC message stream.
/// The structured [`TestSummary`] carries all test results, so the
/// console output from lust's `describe`/`it` is redundant.
///
/// Callers who need console output should use [`register`] on their
/// own `Lua` instance where `print` remains intact.
pub fn run_tests(code: &str, chunk_name: &str) -> Result<TestSummary, String> {
    let lua = Lua::new();

    register(&lua).map_err(|e| format!("Failed to register test framework: {e}"))?;
    super::doubles::register(&lua).map_err(|e| format!("Failed to register test doubles: {e}"))?;

    // Suppress lust's print() output.  The MCP stdio transport
    // reserves stdout for JSON-RPC messages — any other output
    // corrupts the transport.  See MCP spec §Transports/stdio:
    // "The server MUST NOT write anything to its stdout that is
    //  not a valid MCP message."
    lua.globals()
        .set(
            "print",
            lua.create_function(|_, _: mlua::Variadic<LuaValue>| Ok(()))
                .map_err(|e| format!("Failed to override print: {e}"))?,
        )
        .map_err(|e| format!("Failed to override print: {e}"))?;

    lua.load(code)
        .set_name(chunk_name)
        .exec()
        .map_err(|e| format!("Test execution error: {e}"))?;

    collect_results(&lua).map_err(|e| format!("Failed to collect results: {e}"))
}
