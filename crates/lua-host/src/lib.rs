// This crate is the Lua 5.4 scripting engine for HomeCmdr.
// It lets scenes and automations be written as small Lua scripts that can
// read device state, send commands, call plugins, and more.
//
// The four modules below cover everything involved in running a script:
//   context  — the `ctx` object available inside every script
//   convert  — translating values back and forth between Lua and Rust
//   loader   — allows scripts to `require("other.module")` shared helpers
//   runtime  — sets up the Lua VM, enforces time/instruction limits

pub mod context;
pub mod convert;
pub mod loader;
pub mod runtime;

#[cfg(test)]
mod tests;

// Re-export the most commonly used types so callers don't need to know
// which sub-module they live in.
pub use context::{CommandExecutionResult, LuaExecutionContext};
pub use convert::{attribute_to_lua_value, lua_table_to_command, lua_value_to_attribute};
pub use runtime::{
    evaluate_module, install_execution_hook, parse_execution_mode, prepare_lua,
    DEFAULT_MAX_INSTRUCTIONS, ExecutionMode, LuaRuntimeOptions,
};
