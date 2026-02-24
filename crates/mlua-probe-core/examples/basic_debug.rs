//! Minimal example: set a breakpoint, run Lua code, inspect locals.
//!
//! ```sh
//! cargo run --example basic_debug
//! ```

use mlua::prelude::*;
use mlua_probe_core::{DebugEvent, DebugSession};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let lua = Lua::new();

    // Create debug session + controller.
    let (session, controller) = DebugSession::new();
    session.attach(&lua)?;

    // Set a breakpoint on line 4.
    let bp_id = controller.set_breakpoint("@example.lua", 4, None)?;
    println!("Breakpoint {bp_id} set at @example.lua:4");

    // Run Lua code on a separate thread.
    // mlua 0.11's LuaError is not Send, so convert to a string error
    // for cross-thread transport.
    let handle = std::thread::spawn(move || -> Result<i64, String> {
        lua.load(
            r#"
local a = 10
local b = 20
local c = 30
local result = a + b + c
return result
"#,
        )
        .set_name("@example.lua")
        .eval::<i64>()
        .map_err(|e| e.to_string())
    });

    // Wait for the breakpoint hit.
    match controller.wait_event()? {
        DebugEvent::Paused { reason, stack } => {
            println!("VM paused: {reason:?}");
            for frame in &stack {
                let line_str = frame
                    .line
                    .map_or_else(|| "?".to_string(), |l| l.to_string());
                println!(
                    "  frame {}: {} ({}:{})",
                    frame.id, frame.name, frame.source, line_str
                );
            }

            // Inspect locals.
            match controller.get_locals(0) {
                Ok(locals) => {
                    println!("Locals:");
                    for var in &locals {
                        println!("  {} = {} ({})", var.name, var.value, var.type_name);
                    }
                }
                Err(e) => println!("Failed to get locals: {e}"),
            }

            // Evaluate an expression.
            match controller.evaluate("a + b", None) {
                Ok(result) => println!("Eval 'a + b' = {result}"),
                Err(e) => println!("Eval failed: {e}"),
            }
        }
        other => println!("Unexpected event: {other:?}"),
    }

    // Resume.
    controller.continue_execution()?;

    // Wait for completion.
    let result = handle.join().unwrap()?;
    println!("Lua returned: {result}");

    Ok(())
}
