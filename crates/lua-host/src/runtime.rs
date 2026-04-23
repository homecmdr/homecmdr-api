use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use mlua::Lua;

use crate::loader::ScriptLoader;

pub const DEFAULT_MAX_INSTRUCTIONS: u64 = 5_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionMode {
    Parallel { max: usize },
    Single,
    Queued { max: usize },
    Restart,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        ExecutionMode::Parallel { max: 8 }
    }
}

/// Parse an execution mode from a Lua value.
///
/// `default_max` is used as the concurrent-execution ceiling when the Lua
/// script specifies `"parallel"` or `"queued"` as a plain string (without a
/// table with an explicit `max` key).  Pass `8` for the historical default.
pub fn parse_execution_mode(value: mlua::Value, default_max: usize) -> Result<ExecutionMode> {
    match value {
        mlua::Value::Nil => Ok(ExecutionMode::Parallel { max: default_max }),
        mlua::Value::String(s) => {
            let mode_str = s.to_str().map_err(|e| anyhow::anyhow!("{e}"))?;
            match mode_str.as_ref() {
                "parallel" => Ok(ExecutionMode::Parallel { max: default_max }),
                "single" => Ok(ExecutionMode::Single),
                "queued" => Ok(ExecutionMode::Queued { max: default_max }),
                "restart" => Ok(ExecutionMode::Restart),
                other => anyhow::bail!(
                    "unknown execution mode '{other}'; expected parallel, single, queued, or restart"
                ),
            }
        }
        mlua::Value::Table(table) => {
            let mode_type: String = table
                .get("type")
                .map_err(|e| anyhow::anyhow!("mode table is missing string field 'type': {e}"))?;
            match mode_type.as_str() {
                "parallel" => {
                    let max = table
                        .get::<Option<usize>>("max")
                        .map_err(|e| anyhow::anyhow!("mode 'max' field is invalid: {e}"))?
                        .unwrap_or(default_max);
                    Ok(ExecutionMode::Parallel { max })
                }
                "single" => Ok(ExecutionMode::Single),
                "queued" => {
                    let max = table
                        .get::<Option<usize>>("max")
                        .map_err(|e| anyhow::anyhow!("mode 'max' field is invalid: {e}"))?
                        .unwrap_or(default_max);
                    Ok(ExecutionMode::Queued { max })
                }
                "restart" => Ok(ExecutionMode::Restart),
                other => anyhow::bail!("unknown execution mode type '{other}'"),
            }
        }
        _ => anyhow::bail!("mode must be a string or table"),
    }
}

pub fn install_execution_hook(lua: &Lua, max_instructions: u64, cancel: Arc<AtomicBool>) {
    let count = Arc::new(AtomicU64::new(0));
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(10_000),
        move |_lua, _debug| {
            if cancel.load(Ordering::Relaxed) {
                return Err(mlua::Error::runtime("execution cancelled"));
            }
            let new_count = count.fetch_add(10_000, Ordering::Relaxed) + 10_000;
            if new_count >= max_instructions {
                return Err(mlua::Error::runtime("execution compute limit exceeded"));
            }
            Ok(mlua::VmState::Continue)
        },
    );
}

#[derive(Debug, Clone)]
pub struct LuaRuntimeOptions {
    pub scripts_root: Option<PathBuf>,
    pub max_instructions: u64,
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for LuaRuntimeOptions {
    fn default() -> Self {
        Self {
            scripts_root: None,
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
            cancel: None,
        }
    }
}

pub fn prepare_lua(lua: &Lua, options: &LuaRuntimeOptions) -> Result<()> {
    if let Some(root) = &options.scripts_root {
        ScriptLoader { root: root.clone() }.install(lua)?;
    }

    if let Some(cancel) = &options.cancel {
        install_execution_hook(lua, options.max_instructions, cancel.clone());
    }

    Ok(())
}

pub fn evaluate_module(
    lua: &Lua,
    source: &str,
    path_name: &str,
    options: &LuaRuntimeOptions,
) -> anyhow::Result<mlua::Table> {
    prepare_lua(lua, options)?;

    let value = lua
        .load(source)
        .set_name(path_name)
        .eval::<mlua::Value>()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    match value {
        mlua::Value::Table(table) => Ok(table),
        _ => anyhow::bail!("lua module must return a table"),
    }
}
