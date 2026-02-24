# mlua-probe

Lua debugger for [mlua](https://github.com/khvzak/mlua) — breakpoints, stepping, variable inspection, and expression evaluation.

Designed for programmatic access: attach to a running `mlua::Lua` instance and control it from any frontend (MCP server, DAP adapter, etc.).

## Crates

| Crate | Description |
|-------|-------------|
| `mlua-probe-core` | Core debug engine (breakpoints, stepping, inspector) |
| `mlua-probe-mcp` | MCP server binary (stdio transport) |

## Architecture

The engine uses a **VM-thread blocking** model:

1. A Lua debug hook pauses the VM thread when a breakpoint or step condition is met
2. While paused, inspection commands (locals, upvalues, evaluate) run **on the VM thread** — required because `lua_getlocal` and friends are not thread-safe
3. A resume command (continue / step) unblocks the hook

```text
┌──────────┐     commands      ┌──────────────┐
│ Frontend │ ───────────────► │  Controller  │
│ (MCP/..) │ ◄─────────────── │              │
└──────────┘     events        └──────┬───────┘
                                      │ mpsc
                               ┌──────▼───────┐
                               │ Debug Engine │
                               │  (VM thread) │
                               └──────────────┘
```

## Quick start

### As a library

```rust
use mlua::prelude::*;
use mlua_probe_core::{DebugSession, DebugEvent};

let lua = Lua::new();
let (session, controller) = DebugSession::new();
session.attach(&lua).unwrap();

controller.set_breakpoint("@main.lua", 3, None).unwrap();

let handle = std::thread::spawn(move || {
    lua.load(r#"
        local x = 1
        local y = 2
        local z = x + y
        return z
    "#)
    .set_name("@main.lua")
    .eval::<i64>()
});

let event = controller.wait_event().unwrap();
controller.continue_execution().unwrap();

let result = handle.join().unwrap().unwrap();
assert_eq!(result, 3);
```

### As an MCP server

```bash
cargo run --bin mlua-probe-mcp
```

Add to your MCP client configuration:

```json
{
  "mcpServers": {
    "lua-debugger": {
      "command": "mlua-probe-mcp"
    }
  }
}
```

#### MCP tools

| Tool | Description |
|------|-------------|
| `debug_launch` | Launch a Lua debug session with source code |
| `set_breakpoint` | Set a breakpoint at source:line |
| `remove_breakpoint` | Remove a breakpoint by ID |
| `list_breakpoints` | List all breakpoints |
| `continue_execution` | Resume execution |
| `step_into` | Step into the next line |
| `step_over` | Step over (skip function bodies) |
| `step_out` | Step out of current function |
| `pause` | Request VM to pause |
| `wait_event` | Block until next debug event |
| `get_stack_trace` | Get current call stack |
| `get_locals` | Get local variables at a frame |
| `get_upvalues` | Get captured variables at a frame |
| `evaluate` | Evaluate a Lua expression |
| `get_state` | Get session state |
| `disconnect` | End debug session |

## Requirements

- Rust 1.77+
- Lua 5.4 (vendored via mlua)

## Contributing

Bug reports and feature requests are welcome — please [open an issue](https://github.com/ynishi/mlua-probe/issues). Pull requests are also appreciated.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
