use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Result};
use homecmdr_core::runtime::Runtime;
use homecmdr_lua_host::{
    evaluate_module, parse_execution_mode, CommandExecutionResult, LuaExecutionContext,
    LuaRuntimeOptions,
};
use mlua::{Function, Lua};

use crate::types::{Scene, SceneExecutionResult, SceneSummary};

pub fn load_scene_file(path: &Path, scripts_root: Option<&Path>) -> Result<Scene> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read scene file {}", path.display()))?;
    let lua = Lua::new();
    let opts = LuaRuntimeOptions {
        scripts_root: scripts_root.map(Path::to_path_buf),
        ..Default::default()
    };
    let module = evaluate_scene_module(&lua, &source, path, &opts)?;

    let id = module.get::<String>("id").map_err(|error| {
        anyhow::anyhow!(
            "scene file {} is missing string field 'id': {error}",
            path.display()
        )
    })?;
    let name = module.get::<String>("name").map_err(|error| {
        anyhow::anyhow!(
            "scene file {} is missing string field 'name': {error}",
            path.display()
        )
    })?;

    if id.trim().is_empty() {
        anyhow::bail!("scene file {} has empty id", path.display());
    }
    if name.trim().is_empty() {
        anyhow::bail!("scene file {} has empty name", path.display());
    }

    let _: Function = module.get("execute").map_err(|error| {
        anyhow::anyhow!(
            "scene file {} is missing function field 'execute': {error}",
            path.display()
        )
    })?;

    let description = module
        .get::<Option<String>>("description")
        .map_err(|error| {
            anyhow::anyhow!(
                "scene file {} has invalid optional field 'description': {error}",
                path.display()
            )
        })?;

    let mode = parse_execution_mode(module.get("mode").unwrap_or(mlua::Value::Nil), 8)
        .with_context(|| format!("scene file {} has invalid 'mode' field", path.display()))?;

    Ok(Scene {
        summary: SceneSummary {
            id,
            name,
            description,
        },
        mode,
        path: path.to_path_buf(),
    })
}

pub fn execute_scene_inline(
    scene: &Scene,
    runtime: Arc<Runtime>,
    scripts_root: Option<&Path>,
    cancel: Arc<AtomicBool>,
    max_instructions: u64,
) -> Result<Vec<SceneExecutionResult>> {
    let source = fs::read_to_string(&scene.path)
        .with_context(|| format!("failed to read scene file {}", scene.path.display()))?;
    let lua = Lua::new();
    let opts = LuaRuntimeOptions {
        scripts_root: scripts_root.map(Path::to_path_buf),
        max_instructions,
        cancel: Some(cancel),
    };
    let module = evaluate_scene_module(&lua, &source, &scene.path, &opts)?;
    let execute = module.get::<Function>("execute").map_err(|error| {
        anyhow::anyhow!(
            "scene '{}' is missing execute function: {error}",
            scene.summary.id
        )
    })?;

    let ctx = LuaExecutionContext::new(runtime);

    execute.call::<()>(ctx.clone()).map_err(|error| {
        anyhow::anyhow!("scene '{}' execution failed: {error}", scene.summary.id)
    })?;

    Ok(ctx
        .into_results()
        .into_iter()
        .map(scene_result_from_command_result)
        .collect())
}

pub fn evaluate_scene_module(
    lua: &Lua,
    source: &str,
    path: &Path,
    opts: &LuaRuntimeOptions,
) -> Result<mlua::Table> {
    evaluate_module(lua, source, path.to_string_lossy().as_ref(), opts).map_err(|error| {
        anyhow::anyhow!("failed to evaluate scene file {}: {error}", path.display())
    })
}

fn scene_result_from_command_result(result: CommandExecutionResult) -> SceneExecutionResult {
    SceneExecutionResult {
        target: result.target,
        status: result.status,
        message: result.message,
    }
}
