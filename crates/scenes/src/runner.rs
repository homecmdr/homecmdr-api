use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use homecmdr_core::runtime::Runtime;
use homecmdr_lua_host::{ExecutionMode, DEFAULT_MAX_INSTRUCTIONS};

use crate::catalog::SceneCatalog;
use crate::loader::execute_scene_inline;
use crate::types::{Scene, SceneExecutionResult, SceneRunOutcome};

// ── per-scene concurrency tracking ───────────────────────────────────────────

/// Per-scene bookkeeping used to enforce execution modes.
#[derive(Debug, Default)]
struct PerSceneConcurrency {
    active: usize,
    /// Restart mode: cancel token for the currently running execution.
    cancel: Option<Arc<AtomicBool>>,
    /// Queued mode: pending invocations waiting to run.
    queue: VecDeque<PendingScene>,
}

/// A scene invocation that is waiting in the queue (Queued mode).
struct PendingScene {
    runtime: Arc<Runtime>,
}

impl std::fmt::Debug for PendingScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingScene").finish_non_exhaustive()
    }
}

type SceneConcurrencyMap = Arc<std::sync::Mutex<HashMap<String, PerSceneConcurrency>>>;

/// What the dispatcher decided to do with an incoming execute request.
enum SceneDispatch {
    /// Go ahead and run; use this cancel token to abort early if needed.
    Run { cancel: Arc<AtomicBool> },
    /// The scene is busy and its mode says to silently discard the request.
    Drop,
    /// The scene is busy but the request was accepted into the back-log.
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
    /// Wraps a catalog and creates empty per-scene concurrency state.
    pub fn new(catalog: SceneCatalog) -> Self {
        Self {
            catalog: Arc::new(catalog),
            concurrency: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Runs scene `id` against `runtime`, respecting its execution mode.
    /// Returns `SceneRunOutcome::NotFound` if the id is unknown.
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
    pub fn summaries(&self) -> Vec<crate::types::SceneSummary> {
        self.catalog.summaries()
    }
}

/// Looks at the scene's `ExecutionMode` and the current concurrency state to
/// decide whether to run, drop, or queue the incoming invocation.
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

/// Runs a scene on the tokio thread pool, then decrements the active counter
/// and kicks off the next queued invocation (if any).
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
