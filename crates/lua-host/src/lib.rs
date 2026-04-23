pub mod context;
pub mod convert;
pub mod loader;
pub mod runtime;

#[cfg(test)]
mod tests;

pub use context::{CommandExecutionResult, LuaExecutionContext};
pub use convert::{attribute_to_lua_value, lua_table_to_command, lua_value_to_attribute};
pub use runtime::{
    evaluate_module, install_execution_hook, parse_execution_mode, prepare_lua,
    DEFAULT_MAX_INSTRUCTIONS, ExecutionMode, LuaRuntimeOptions,
};
