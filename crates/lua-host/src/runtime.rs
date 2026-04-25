// This module sets up a Lua VM instance and enforces guardrails so that a
// misbehaving script can't freeze the server or eat all the CPU.
//
// Two safety mechanisms are in play:
//   1. An instruction counter — the script is killed once it has executed more
//      than `max_instructions` VM steps (think of it as a budget for how much
//      computation a single run is allowed).
//   2. A cancellation token — an atomic flag that any other thread can flip to
//      `true` in order to abort a running script immediately (e.g. on shutdown).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use mlua::Lua;

use crate::loader::ScriptLoader;

// The default maximum number of Lua VM instructions a single script execution
// may consume before being forcibly stopped.  5 million instructions is
// generous for typical automation logic but low enough to catch infinite loops
// within a second or two.
pub const DEFAULT_MAX_INSTRUCTIONS: u64 = 5_000_000;

// Controls what happens when a scene or automation is triggered while a
// previous run of the same script is still executing.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionMode {
    // Allow up to `max` concurrent runs of the same script at the same time.
    Parallel { max: usize },
    // Only one run at a time; new triggers are dropped while one is active.
    Single,
    // Queue up to `max` pending runs; each waits for the previous to finish.
    Queued { max: usize },
    // If a run is in progress, cancel it and immediately start a fresh one.
    Restart,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        // Parallel with a cap of 8 concurrent runs is a sensible default for
        // most home automation scenarios.
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
            // Table form lets scripts specify a custom `max`, e.g.:
            //   execution_mode = { type = "parallel", max = 2 }
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

// Installs a Lua debug hook that fires every 10 000 instructions.
// On each tick it checks two things:
//   - Has the cancellation token been set?  If so, abort.
//   - Has the running total exceeded `max_instructions`?  If so, abort.
// The hook is intentionally coarse-grained (every 10k) to keep overhead low.
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

// Options that control how a Lua VM instance is prepared.
#[derive(Debug, Clone)]
pub struct LuaRuntimeOptions {
    // If set, the script loader is installed so scripts can `require` modules
    // from this directory.
    pub scripts_root: Option<PathBuf>,
    // How many VM instructions are allowed before the script is killed.
    pub max_instructions: u64,
    // Optional cancellation token — set it to `true` from another thread to
    // abort a running script (e.g. on server shutdown or timeout).
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

// Applies options to an existing Lua VM: installs the script loader if a
// root directory is configured, then installs the execution hook if a
// cancellation token is provided.
pub fn prepare_lua(lua: &Lua, options: &LuaRuntimeOptions) -> Result<()> {
    if let Some(root) = &options.scripts_root {
        ScriptLoader { root: root.clone() }.install(lua)?;
    }

    if let Some(cancel) = &options.cancel {
        install_execution_hook(lua, options.max_instructions, cancel.clone());
    }

    Ok(())
}

// Loads and evaluates a Lua script, then returns the table it returned.
// All HomeCmdr scripts are expected to return a table (e.g. with `id`,
// `name`, `execute`, etc.) — this function enforces that convention.
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
