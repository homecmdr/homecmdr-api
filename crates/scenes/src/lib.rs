use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use mlua::{Function, Lua};
use serde::Serialize;
use smart_home_core::runtime::Runtime;
use smart_home_lua_host::{
    evaluate_module, parse_execution_mode, CommandExecutionResult, ExecutionMode,
    LuaExecutionContext, LuaRuntimeOptions, DEFAULT_MAX_INSTRUCTIONS,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReloadError {
    pub file: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneExecutionResult {
    pub target: String,
    pub status: &'static str,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub summary: SceneSummary,
    pub mode: ExecutionMode,
    path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct SceneCatalog {
    scenes: HashMap<String, Scene>,
    scripts_root: Option<PathBuf>,
}

// ── per-scene concurrency tracking ───────────────────────────────────────────

#[derive(Debug, Default)]
struct PerSceneConcurrency {
    active: usize,
    /// Restart mode: cancel token for the currently running execution.
    cancel: Option<Arc<AtomicBool>>,
    /// Queued mode: pending invocations waiting to run.
    queue: VecDeque<PendingScene>,
}

struct PendingScene {
    runtime: Arc<Runtime>,
}

impl std::fmt::Debug for PendingScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingScene").finish_non_exhaustive()
    }
}

type SceneConcurrencyMap = Arc<std::sync::Mutex<HashMap<String, PerSceneConcurrency>>>;

enum SceneDispatch {
    Run { cancel: Arc<AtomicBool> },
    Drop,
    Queued,
}

// ── SceneRunOutcome ───────────────────────────────────────────────────────────

/// The outcome of a `SceneRunner::execute` call.
#[derive(Debug)]
pub enum SceneRunOutcome {
    /// Scene completed; results are attached.
    Completed(Vec<SceneExecutionResult>),
    /// No scene with the given id was found.
    NotFound,
    /// Scene is already running and its mode does not allow additional executions.
    Dropped,
    /// Scene was accepted for asynchronous execution (queued mode, background run).
    Queued,
}

// ── SceneRunner ───────────────────────────────────────────────────────────────

/// Wraps a `SceneCatalog` and enforces per-scene execution modes.
#[derive(Clone)]
pub struct SceneRunner {
    catalog: Arc<SceneCatalog>,
    concurrency: SceneConcurrencyMap,
}

impl SceneRunner {
    pub fn new(catalog: SceneCatalog) -> Self {
        Self {
            catalog: Arc::new(catalog),
            concurrency: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn execute(&self, id: &str, runtime: Arc<Runtime>) -> Result<SceneRunOutcome> {
        let scene = match self.catalog.scenes.get(id) {
            Some(s) => s.clone(),
            None => return Ok(SceneRunOutcome::NotFound),
        };

        let dispatch = {
            let mut map = self.concurrency.lock().unwrap_or_else(|p| p.into_inner());
            let state = map.entry(id.to_string()).or_default();
            decide_scene_dispatch(&scene, state, runtime.clone())
        };

        match dispatch {
            SceneDispatch::Run { cancel } => {
                let result = run_and_finalize(
                    scene,
                    runtime,
                    self.catalog.clone(),
                    self.concurrency.clone(),
                    cancel,
                )
                .await;
                result.map(SceneRunOutcome::Completed)
            }
            SceneDispatch::Drop => Ok(SceneRunOutcome::Dropped),
            SceneDispatch::Queued => Ok(SceneRunOutcome::Queued),
        }
    }

    /// Expose catalog summaries.
    pub fn summaries(&self) -> Vec<SceneSummary> {
        self.catalog.summaries()
    }
}

fn decide_scene_dispatch(
    scene: &Scene,
    state: &mut PerSceneConcurrency,
    runtime: Arc<Runtime>,
) -> SceneDispatch {
    match &scene.mode {
        ExecutionMode::Parallel { max } => {
            if state.active < *max {
                state.active += 1;
                SceneDispatch::Run {
                    cancel: Arc::new(AtomicBool::new(false)),
                }
            } else {
                SceneDispatch::Drop
            }
        }
        ExecutionMode::Single => {
            if state.active == 0 {
                state.active += 1;
                SceneDispatch::Run {
                    cancel: Arc::new(AtomicBool::new(false)),
                }
            } else {
                SceneDispatch::Drop
            }
        }
        ExecutionMode::Queued { max } => {
            if state.active == 0 {
                state.active += 1;
                SceneDispatch::Run {
                    cancel: Arc::new(AtomicBool::new(false)),
                }
            } else if state.queue.len() < *max {
                state.queue.push_back(PendingScene { runtime });
                SceneDispatch::Queued
            } else {
                SceneDispatch::Drop
            }
        }
        ExecutionMode::Restart => {
            if let Some(old_cancel) = state.cancel.take() {
                old_cancel.store(true, Ordering::Relaxed);
            }
            state.active += 1;
            let cancel = Arc::new(AtomicBool::new(false));
            state.cancel = Some(cancel.clone());
            SceneDispatch::Run { cancel }
        }
    }
}

async fn run_and_finalize(
    scene: Scene,
    runtime: Arc<Runtime>,
    catalog: Arc<SceneCatalog>,
    concurrency: SceneConcurrencyMap,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<SceneExecutionResult>> {
    let scene_id = scene.summary.id.clone();
    let scripts_root = catalog.scripts_root.clone();

    let result = tokio::spawn({
        let scene = scene.clone();
        async move {
            execute_scene_inline(
                &scene,
                runtime,
                scripts_root.as_deref(),
                cancel,
                DEFAULT_MAX_INSTRUCTIONS,
            )
        }
    })
    .await?;

    // Decrement active and check for a queued next invocation.
    let next = {
        let mut map = concurrency.lock().unwrap_or_else(|p| p.into_inner());
        let state = map.entry(scene_id).or_default();
        state.active = state.active.saturating_sub(1);
        if matches!(scene.mode, ExecutionMode::Queued { .. }) {
            state.queue.pop_front().map(|pending| {
                state.active += 1;
                pending.runtime
            })
        } else {
            None
        }
    };

    if let Some(next_runtime) = next {
        spawn_queued_scene(scene, next_runtime, catalog, concurrency);
    }

    result
}

/// Fire-and-forget: runs one queued scene in the background, then recurses for
/// the next item in the queue (if any).
fn spawn_queued_scene(
    scene: Scene,
    runtime: Arc<Runtime>,
    catalog: Arc<SceneCatalog>,
    concurrency: SceneConcurrencyMap,
) {
    tokio::spawn(async move {
        let scene_id = scene.summary.id.clone();
        let scripts_root = catalog.scripts_root.clone();
        let cancel = Arc::new(AtomicBool::new(false));

        let _ = tokio::spawn({
            let scene = scene.clone();
            async move {
                execute_scene_inline(
                    &scene,
                    runtime,
                    scripts_root.as_deref(),
                    cancel,
                    DEFAULT_MAX_INSTRUCTIONS,
                )
            }
        })
        .await;

        let next = {
            let mut map = concurrency.lock().unwrap_or_else(|p| p.into_inner());
            let state = map.entry(scene_id).or_default();
            state.active = state.active.saturating_sub(1);
            if matches!(scene.mode, ExecutionMode::Queued { .. }) {
                state.queue.pop_front().map(|pending| {
                    state.active += 1;
                    pending.runtime
                })
            } else {
                None
            }
        };

        if let Some(next_runtime) = next {
            spawn_queued_scene(scene, next_runtime, catalog, concurrency);
        }
    });
}

// ── SceneCatalog ──────────────────────────────────────────────────────────────

impl SceneCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load_from_directory(
        path: impl AsRef<Path>,
        scripts_root: Option<PathBuf>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let entries = fs::read_dir(path)
            .with_context(|| format!("failed to read scenes directory {}", path.display()))?;
        let mut scenes = HashMap::new();

        for entry in entries {
            let entry = entry.context("failed to read scenes directory entry")?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            if !file_type.is_file() {
                continue;
            }

            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("lua") {
                continue;
            }

            let scene = load_scene_file(&entry.path(), scripts_root.as_deref())?;
            let scene_id = scene.summary.id.clone();
            if scenes.insert(scene_id.clone(), scene).is_some() {
                bail!("duplicate scene id '{scene_id}'");
            }
        }

        Ok(Self {
            scenes,
            scripts_root,
        })
    }

    pub fn reload_from_directory(
        path: impl AsRef<Path>,
        scripts_root: Option<PathBuf>,
    ) -> std::result::Result<Self, Vec<ReloadError>> {
        let path = path.as_ref();
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                return Err(vec![ReloadError {
                    file: path.display().to_string(),
                    message: format!("failed to read scenes directory: {error}"),
                }]);
            }
        };

        let mut files = Vec::new();
        let mut errors = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    errors.push(ReloadError {
                        file: path.display().to_string(),
                        message: format!("failed to read scenes directory entry: {error}"),
                    });
                    continue;
                }
            };

            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    errors.push(ReloadError {
                        file: entry.path().display().to_string(),
                        message: format!("failed to inspect file type: {error}"),
                    });
                    continue;
                }
            };

            if !file_type.is_file() {
                continue;
            }

            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("lua") {
                continue;
            }

            files.push(entry.path());
        }

        files.sort();

        let mut scenes = HashMap::new();
        let mut ids = HashMap::<String, PathBuf>::new();

        for file in files {
            match load_scene_file(&file, scripts_root.as_deref()) {
                Ok(scene) => {
                    let scene_id = scene.summary.id.clone();
                    if let Some(existing_path) = ids.insert(scene_id.clone(), file.clone()) {
                        errors.push(ReloadError {
                            file: file.display().to_string(),
                            message: format!(
                                "duplicate scene id '{scene_id}' (already defined in {})",
                                existing_path.display()
                            ),
                        });
                        continue;
                    }
                    scenes.insert(scene_id, scene);
                }
                Err(error) => {
                    errors.push(ReloadError {
                        file: file.display().to_string(),
                        message: error.to_string(),
                    });
                }
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(Self {
            scenes,
            scripts_root,
        })
    }

    pub fn summaries(&self) -> Vec<SceneSummary> {
        let mut scenes = self
            .scenes
            .values()
            .map(|scene| scene.summary.clone())
            .collect::<Vec<_>>();
        scenes.sort_by(|a, b| a.id.cmp(&b.id));
        scenes
    }

    /// Execute a scene by id directly (no mode enforcement).
    /// Returns `None` if the scene does not exist.
    pub fn execute(
        &self,
        id: &str,
        runtime: Arc<Runtime>,
    ) -> Result<Option<Vec<SceneExecutionResult>>> {
        let Some(scene) = self.scenes.get(id) else {
            return Ok(None);
        };

        let results = execute_scene_inline(
            scene,
            runtime,
            self.scripts_root.as_deref(),
            Arc::new(AtomicBool::new(false)),
            DEFAULT_MAX_INSTRUCTIONS,
        )?;
        Ok(Some(results))
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn execute_scene_inline(
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

fn load_scene_file(path: &Path, scripts_root: Option<&Path>) -> Result<Scene> {
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
        bail!("scene file {} has empty id", path.display());
    }
    if name.trim().is_empty() {
        bail!("scene file {} has empty name", path.display());
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

fn evaluate_scene_module(
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use smart_home_core::adapter::Adapter;
    use smart_home_core::bus::EventBus;
    use smart_home_core::command::DeviceCommand;
    use smart_home_core::model::{AttributeValue, Device, DeviceId, DeviceKind, Metadata};
    use smart_home_core::registry::DeviceRegistry;
    use smart_home_core::runtime::{Runtime, RuntimeConfig};
    use tokio::time::{sleep, timeout, Duration};

    use super::*;

    struct CommandAdapter;

    #[async_trait::async_trait]
    impl Adapter for CommandAdapter {
        fn name(&self) -> &str {
            "test"
        }

        async fn run(&self, _registry: DeviceRegistry, _bus: EventBus) -> Result<()> {
            std::future::pending::<()>().await;
            Ok(())
        }

        async fn command(
            &self,
            device_id: &DeviceId,
            command: DeviceCommand,
            registry: DeviceRegistry,
        ) -> Result<bool> {
            if device_id.0 != "test:device" {
                return Ok(false);
            }

            let mut device = registry.get(device_id).expect("test device exists");
            device.attributes.insert(
                command.capability,
                command.value.expect("test command must include value"),
            );
            registry
                .upsert(device)
                .await
                .expect("registry update succeeds");
            Ok(true)
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{unique}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn write_scene(dir: &Path, name: &str, source: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, source).expect("write scene file");
        path
    }

    fn sample_device(id: &str) -> Device {
        Device {
            id: DeviceId(id.to_string()),
            room_id: None,
            kind: DeviceKind::Light,
            attributes: HashMap::from([(
                "power".to_string(),
                AttributeValue::Text("off".to_string()),
            )]),
            metadata: Metadata {
                source: "test".to_string(),
                accuracy: None,
                vendor_specific: HashMap::new(),
            },
            updated_at: chrono::Utc::now(),
            last_seen: chrono::Utc::now(),
        }
    }

    fn make_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ))
    }

    #[test]
    fn loads_valid_scene_catalog() {
        let dir = temp_dir("smart-home-scenes");
        write_scene(
            &dir,
            "video.lua",
            r#"return {
                id = "video",
                name = "Video",
                execute = function(ctx)
                end
            }"#,
        );

        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        assert_eq!(catalog.summaries().len(), 1);
        assert_eq!(catalog.summaries()[0].id, "video");
    }

    #[test]
    fn rejects_scene_without_execute() {
        let dir = temp_dir("smart-home-scenes");
        write_scene(
            &dir,
            "broken.lua",
            r#"return {
                id = "video",
                name = "Video"
            }"#,
        );

        let error = SceneCatalog::load_from_directory(&dir, None)
            .err()
            .expect("missing execute should fail");
        assert!(error
            .to_string()
            .contains("missing function field 'execute'"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn executes_scene_commands_against_runtime() {
        let dir = temp_dir("smart-home-scenes");
        write_scene(
            &dir,
            "set-brightness.lua",
            r#"return {
                id = "set_brightness",
                name = "Set Brightness",
                execute = function(ctx)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 42,
                    })
                end
            }"#,
        );

        let runtime = make_runtime();
        runtime
            .registry()
            .upsert(sample_device("test:device"))
            .await
            .expect("test device exists");

        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        let results = catalog
            .execute("set_brightness", runtime.clone())
            .expect("scene executes")
            .expect("scene exists");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "ok");
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("updated device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(42))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn executes_scene_using_required_script_module() {
        let dir = temp_dir("smart-home-scenes");
        let scripts_dir = temp_dir("smart-home-scripts");
        write_scene(
            &dir,
            "set-brightness.lua",
            r#"local helpers = require("lighting.helpers")

            return {
                id = "set_brightness",
                name = "Set Brightness",
                execute = function(ctx)
                    helpers.set_brightness(ctx, "test:device", 33)
                end
            }"#,
        );
        fs::create_dir_all(scripts_dir.join("lighting")).expect("create scripts namespace dir");
        fs::write(
            scripts_dir.join("lighting/helpers.lua"),
            r#"local M = {}

            function M.set_brightness(ctx, device_id, value)
                ctx:command(device_id, {
                    capability = "brightness",
                    action = "set",
                    value = value,
                })
            end

            return M"#,
        )
        .expect("write helper script");

        let runtime = make_runtime();
        runtime
            .registry()
            .upsert(sample_device("test:device"))
            .await
            .expect("test device exists");

        let catalog = SceneCatalog::load_from_directory(&dir, Some(scripts_dir))
            .expect("scene catalog loads");
        let results = catalog
            .execute("set_brightness", runtime.clone())
            .expect("scene executes")
            .expect("scene exists");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "ok");
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("updated device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(33))
        );
    }

    // ── execution mode tests ──────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn single_mode_rejects_concurrent_scene_execution() {
        let dir = temp_dir("smart-home-scenes");
        write_scene(
            &dir,
            "single.lua",
            r#"return {
                id = "single_scene",
                name = "Single Scene",
                mode = "single",
                execute = function(ctx)
                    ctx:sleep(0.2)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 5,
                    })
                end
            }"#,
        );

        let runtime = make_runtime();
        runtime
            .registry()
            .upsert(sample_device("test:device"))
            .await
            .expect("device upserted");

        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        let runner = SceneRunner::new(catalog);

        // spawn first execution (will block ~200 ms inside Lua sleep)
        let runner1 = runner.clone();
        let runtime1 = runtime.clone();
        let handle1 = tokio::spawn(async move { runner1.execute("single_scene", runtime1).await });

        // let the first invocation enter Lua and start sleeping
        sleep(Duration::from_millis(20)).await;

        // second invocation should be dropped
        let outcome2 = runner
            .execute("single_scene", runtime.clone())
            .await
            .expect("second execute does not error");

        let outcome1 = handle1
            .await
            .expect("task joins")
            .expect("first execute ok");

        assert!(
            matches!(outcome1, SceneRunOutcome::Completed(_)),
            "first invocation should complete"
        );
        assert!(
            matches!(outcome2, SceneRunOutcome::Dropped),
            "second invocation should be dropped"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_mode_allows_concurrent_executions() {
        let dir = temp_dir("smart-home-scenes");
        write_scene(
            &dir,
            "parallel.lua",
            r#"return {
                id = "parallel_scene",
                name = "Parallel Scene",
                mode = { type = "parallel", max = 2 },
                execute = function(ctx)
                    ctx:sleep(0.1)
                end
            }"#,
        );

        let runtime = make_runtime();
        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        let runner = SceneRunner::new(catalog);

        let runner1 = runner.clone();
        let runtime1 = runtime.clone();
        let handle1 =
            tokio::spawn(async move { runner1.execute("parallel_scene", runtime1).await });

        let runner2 = runner.clone();
        let runtime2 = runtime.clone();
        let handle2 =
            tokio::spawn(async move { runner2.execute("parallel_scene", runtime2).await });

        let (outcome1, outcome2) =
            tokio::join!(async { handle1.await.expect("task1 joins") }, async {
                handle2.await.expect("task2 joins")
            },);

        assert!(
            matches!(
                outcome1.expect("outcome1 ok"),
                SceneRunOutcome::Completed(_)
            ),
            "first parallel invocation should complete"
        );
        assert!(
            matches!(
                outcome2.expect("outcome2 ok"),
                SceneRunOutcome::Completed(_)
            ),
            "second parallel invocation should complete"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sleep_in_scene_works() {
        let dir = temp_dir("smart-home-scenes");
        write_scene(
            &dir,
            "sleepy.lua",
            r#"return {
                id = "sleepy_scene",
                name = "Sleepy Scene",
                execute = function(ctx)
                    ctx:sleep(0.05)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 77,
                    })
                end
            }"#,
        );

        let runtime = make_runtime();
        runtime
            .registry()
            .upsert(sample_device("test:device"))
            .await
            .expect("device upserted");

        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        let runner = SceneRunner::new(catalog);

        let outcome = timeout(
            Duration::from_secs(2),
            runner.execute("sleepy_scene", runtime.clone()),
        )
        .await
        .expect("scene completes within timeout")
        .expect("execute ok");

        match outcome {
            SceneRunOutcome::Completed(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].status, "ok");
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(77))
        );
    }
}
