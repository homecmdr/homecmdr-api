//! The automation execution engine.
//!
//! [`AutomationRunner`] is the main entry point: call [`AutomationRunner::run`]
//! to start background tasks for every trigger type in the catalog.  Each task
//! monitors its own trigger source and calls [`spawn_automation_execution`]
//! whenever a trigger fires.
//!
//! [`AutomationController`] is a cheap, cloneable handle to the running catalog
//! that the HTTP API layer uses without needing to know about the background
//! tasks.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use homecmdr_core::model::{AttributeValue, DeviceId};
use homecmdr_core::runtime::Runtime;
use homecmdr_core::store::{AutomationExecutionHistoryEntry, DeviceStore, SceneStepResult};
use homecmdr_lua_host::{
    attribute_to_lua_value, LuaExecutionContext, LuaRuntimeOptions, DEFAULT_MAX_INSTRUCTIONS,
};
use mlua::{Function, Lua};
use tokio::time::{timeout, Duration};

use crate::catalog::{evaluate_automation_module, AutomationCatalog};
use crate::concurrency::{ConcurrencyMap, PendingExecution, SpawnDecision};
use crate::conditions::first_failed_condition;
use crate::state::{persist_runtime_state, should_skip_trigger, AutomationStateStore};
use crate::triggers::device::run_event_trigger_loop;
use crate::triggers::interval::run_interval_trigger_loop;
use crate::triggers::scheduled::run_scheduled_trigger_loop;
use crate::triggers::trigger_uses_event_bus;
use crate::types::{Automation, AutomationExecutionResult, TriggerContext, DEFAULT_BACKSTOP_TIMEOUT};

// ── Public observer trait ─────────────────────────────────────────────────────
// Implement this to receive a callback every time an automation finishes,
// whether it succeeded, was skipped, or errored.  The API layer uses this to
// persist execution history records to the database.

/// Receives a callback after every automation execution completes.
pub trait AutomationExecutionObserver: Send + Sync {
    fn record(&self, entry: AutomationExecutionHistoryEntry);
}

// ── AutomationExecutionRecord (crate-internal) ────────────────────────────────
// The raw execution outcome produced by `execute_automation` before it is
// converted into the public `AutomationExecutionResult` type.

/// Internal execution record — like [`AutomationExecutionResult`] but not part
/// of the public API.
#[derive(Debug)]
pub(crate) struct AutomationExecutionRecord {
    pub(crate) status: String,
    pub(crate) error: Option<String>,
    pub(crate) results: Vec<SceneStepResult>,
    pub(crate) duration_ms: i64,
}

// ── ExecutionControl (crate-internal) ────────────────────────────────────────
// A config bundle threaded through every spawn path so each spawned task has
// what it needs (concurrency map, instruction limit, trigger context, timeout)
// without having to borrow from the runner.

/// Shared execution config passed to every background task and spawn call.
#[derive(Clone)]
pub(crate) struct ExecutionControl {
    pub(crate) concurrency: ConcurrencyMap,
    pub(crate) max_instructions: u64,
    pub(crate) trigger_context: TriggerContext,
    pub(crate) backstop_timeout: Duration,
}

// ── AutomationRunner ──────────────────────────────────────────────────────────
// Starts one background task per trigger type in the catalog and drives the
// whole automation lifecycle.  Call `run()` once at server startup; it runs
// until the runtime is shut down.

/// Owns the catalog and all configuration needed to start the trigger loops.
/// Consumed by [`run`] which spawns the background tasks.
#[derive(Clone)]
pub struct AutomationRunner {
    catalog: AutomationCatalog,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
    trigger_context: TriggerContext,
    backstop_timeout: Duration,
}

impl AutomationRunner {
    /// Create a new runner with the given catalog and sensible defaults.
    pub fn new(catalog: AutomationCatalog) -> Self {
        Self {
            catalog,
            observer: None,
            state_store: None,
            trigger_context: TriggerContext::default(),
            backstop_timeout: DEFAULT_BACKSTOP_TIMEOUT,
        }
    }

    /// Register an observer that will be called after every automation run.
    pub fn with_observer(mut self, observer: Arc<dyn AutomationExecutionObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Provide a persistent store for cooldown / dedup / resumable-schedule
    /// state.  Without this, runtime state features are silently disabled.
    pub fn with_state_store(mut self, store: Arc<dyn DeviceStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Supply latitude, longitude, and timezone for solar and time-window
    /// triggers and conditions.
    pub fn with_trigger_context(mut self, trigger_context: TriggerContext) -> Self {
        self.trigger_context = trigger_context;
        self
    }

    /// Override the hard deadline after which a still-running automation is
    /// forcibly cancelled.  Defaults to [`DEFAULT_BACKSTOP_TIMEOUT`] (1 hour).
    pub fn with_backstop_timeout(mut self, duration: Duration) -> Self {
        self.backstop_timeout = duration;
        self
    }

    /// Return a cloneable controller backed by the same catalog.  The HTTP
    /// handlers receive this rather than the runner itself.
    pub fn controller(&self) -> AutomationController {
        AutomationController {
            catalog: self.catalog.clone(),
            observer: self.observer.clone(),
        }
    }

    /// Start all trigger loops and block until they all exit (which normally
    /// only happens when the server shuts down).
    pub async fn run(self, runtime: Arc<Runtime>) {
        let trigger_context = self.trigger_context;
        let concurrency = self.catalog.concurrency.clone();
        let backstop_timeout = self.backstop_timeout;
        self.run_with_options(
            runtime,
            ExecutionControl {
                concurrency,
                max_instructions: DEFAULT_MAX_INSTRUCTIONS,
                trigger_context,
                backstop_timeout,
            },
        )
        .await;
    }

    pub(crate) async fn run_with_options(self, runtime: Arc<Runtime>, execution: ExecutionControl) {
        let mut tasks = tokio::task::JoinSet::new();

        if self.catalog.automations.iter().any(trigger_uses_event_bus) {
            let runtime = runtime.clone();
            let catalog = self.catalog.clone();
            let scripts_root = self.catalog.scripts_root.clone();
            let execution = execution.clone();
            let observer = self.observer.clone();
            let state_store = self.state_store.clone();
            tasks.spawn(async move {
                run_event_trigger_loop(
                    runtime,
                    catalog,
                    scripts_root,
                    execution,
                    observer,
                    state_store,
                )
                .await;
            });
        }

        for automation in self.catalog.automations.iter().cloned() {
            match automation.trigger.clone() {
                crate::types::Trigger::Interval { every_secs } => {
                    if !self
                        .catalog
                        .is_enabled(&automation.summary.id)
                        .unwrap_or(true)
                    {
                        continue;
                    }
                    let runtime = runtime.clone();
                    let scripts_root = self.catalog.scripts_root.clone();
                    let execution = execution.clone();
                    let observer = self.observer.clone();
                    let catalog = self.catalog.clone();
                    let state_store = self.state_store.clone();
                    tasks.spawn(async move {
                        run_interval_trigger_loop(
                            runtime,
                            catalog,
                            automation,
                            every_secs,
                            scripts_root,
                            execution,
                            observer,
                            state_store,
                        )
                        .await;
                    });
                }
                crate::types::Trigger::WallClock { .. }
                | crate::types::Trigger::Cron { .. }
                | crate::types::Trigger::Sunrise { .. }
                | crate::types::Trigger::Sunset { .. } => {
                    if !self
                        .catalog
                        .is_enabled(&automation.summary.id)
                        .unwrap_or(true)
                    {
                        continue;
                    }
                    let runtime = runtime.clone();
                    let scripts_root = self.catalog.scripts_root.clone();
                    let execution = execution.clone();
                    let observer = self.observer.clone();
                    let catalog = self.catalog.clone();
                    let state_store = self.state_store.clone();
                    let trigger_context = self.trigger_context;
                    tasks.spawn(async move {
                        run_scheduled_trigger_loop(
                            runtime,
                            catalog,
                            automation,
                            scripts_root,
                            execution,
                            observer,
                            state_store,
                            trigger_context,
                        )
                        .await;
                    });
                }
                _ => {}
            }
        }

        while tasks.join_next().await.is_some() {}
    }
}

// ── AutomationController ──────────────────────────────────────────────────────
// A cheap, cloneable handle to the running catalog and observer.  The HTTP
// API layer uses this to list, get, enable/disable, validate, and manually
// execute automations without needing to touch the background tasks.

/// Cloneable handle used by the HTTP API to interact with the running catalog.
#[derive(Clone)]
pub struct AutomationController {
    catalog: AutomationCatalog,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
}

impl AutomationController {
    pub fn summaries(&self) -> Vec<crate::types::AutomationSummary> {
        self.catalog.summaries()
    }

    pub fn get(&self, id: &str) -> Option<crate::types::AutomationSummary> {
        self.catalog.get(id)
    }

    pub fn is_enabled(&self, id: &str) -> Option<bool> {
        self.catalog.is_enabled(id)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        self.catalog.set_enabled(id, enabled)
    }

    pub fn validate(&self, id: &str) -> Result<crate::types::AutomationSummary> {
        self.catalog.validate(id)
    }

    pub fn execute(
        &self,
        id: &str,
        runtime: Arc<Runtime>,
        trigger_payload: AttributeValue,
        trigger_context: TriggerContext,
    ) -> Result<AutomationExecutionResult> {
        let result = self
            .catalog
            .execute(id, runtime, trigger_payload.clone(), trigger_context)?;
        if let Some(observer) = &self.observer {
            observer.record(AutomationExecutionHistoryEntry {
                executed_at: Utc::now(),
                automation_id: id.to_string(),
                trigger_payload,
                status: result.status.clone(),
                duration_ms: result.duration_ms,
                error: result.error.clone(),
                results: result.results.clone(),
            });
        }
        Ok(result)
    }
}

// ── execute_automation ────────────────────────────────────────────────────────
// Synchronous core execution: reads the Lua file, evaluates it in a fresh Lua
// VM, calls `execute(ctx, event)`, and returns a timing record.  Everything
// above this is async orchestration; this is where the Lua actually runs.

/// Load and run a single automation's `execute` function.  Returns a record
/// with the status, any error message, scene step results, and elapsed time.
pub(crate) fn execute_automation(
    automation: &Automation,
    runtime: Arc<Runtime>,
    event: AttributeValue,
    scripts_root: Option<&Path>,
    cancel: Arc<AtomicBool>,
    max_instructions: u64,
) -> Result<AutomationExecutionRecord> {
    let started = Instant::now();
    let source = fs::read_to_string(&automation.path).with_context(|| {
        format!(
            "failed to read automation file {}",
            automation.path.display()
        )
    })?;
    let lua = Lua::new();
    let opts = LuaRuntimeOptions {
        scripts_root: scripts_root.map(Path::to_path_buf),
        max_instructions,
        cancel: Some(cancel),
    };
    let module = evaluate_automation_module(&lua, &source, &automation.path, &opts)?;
    let execute = module.get::<Function>("execute").map_err(|error| {
        anyhow::anyhow!(
            "automation '{}' is missing execute function: {error}",
            automation.summary.id
        )
    })?;

    let ctx = LuaExecutionContext::new(runtime);
    let event =
        attribute_to_lua_value(&lua, event).map_err(|error| anyhow::anyhow!(error.to_string()))?;

    execute.call::<()>((ctx.clone(), event)).map_err(|error| {
        anyhow::anyhow!(
            "automation '{}' execution failed: {error}",
            automation.summary.id
        )
    })?;

    Ok(AutomationExecutionRecord {
        status: "ok".to_string(),
        error: None,
        results: ctx
            .into_results()
            .into_iter()
            .map(|result| SceneStepResult {
                target: result.target,
                status: result.status.to_string(),
                message: result.message,
            })
            .collect(),
        duration_ms: started.elapsed().as_millis() as i64,
    })
}

// ── spawn_automation_execution ────────────────────────────────────────────────
// Enforces the concurrency mode (parallel, single, queued, restart) before
// handing off to `do_spawn_execution`.  Returns immediately — the actual
// execution runs in a Tokio task.

/// Check the concurrency mode, then either spawn the automation, queue the
/// trigger, or drop it with a warning.
pub(crate) fn spawn_automation_execution(
    automation: Automation,
    runtime: Arc<Runtime>,
    event: AttributeValue,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<AutomationStateStore>,
) {
    let id = automation.summary.id.clone();
    let decision = {
        let mut map = execution
            .concurrency
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let state = map.entry(id.clone()).or_default();
        match &automation.mode {
            homecmdr_lua_host::ExecutionMode::Parallel { max } => {
                if state.active < *max {
                    state.active += 1;
                    SpawnDecision::Spawn {
                        cancel: Arc::new(AtomicBool::new(false)),
                    }
                } else {
                    SpawnDecision::Drop
                }
            }
            homecmdr_lua_host::ExecutionMode::Single => {
                if state.active == 0 {
                    state.active += 1;
                    SpawnDecision::Spawn {
                        cancel: Arc::new(AtomicBool::new(false)),
                    }
                } else {
                    SpawnDecision::Drop
                }
            }
            homecmdr_lua_host::ExecutionMode::Queued { max } => {
                if state.active == 0 {
                    state.active += 1;
                    SpawnDecision::Spawn {
                        cancel: Arc::new(AtomicBool::new(false)),
                    }
                } else if state.queue.len() < *max {
                    state.queue.push_back(PendingExecution {
                        event: event.clone(),
                    });
                    SpawnDecision::Queue
                } else {
                    SpawnDecision::Drop
                }
            }
            homecmdr_lua_host::ExecutionMode::Restart => {
                if let Some(old_cancel) = state.cancel.take() {
                    old_cancel.store(true, Ordering::Relaxed);
                }
                state.active += 1;
                let cancel = Arc::new(AtomicBool::new(false));
                state.cancel = Some(cancel.clone());
                SpawnDecision::Spawn { cancel }
            }
        }
    };

    match decision {
        SpawnDecision::Spawn { cancel } => {
            do_spawn_execution(
                automation,
                runtime,
                event,
                scripts_root,
                execution,
                observer,
                state_store,
                cancel,
            );
        }
        SpawnDecision::Queue => {
            // event already enqueued above; nothing more to do
        }
        SpawnDecision::Drop => {
            tracing::warn!(automation = %id, "skipping automation execution due to execution mode saturation");
            notify_observer(
                observer.as_ref(),
                &automation,
                event,
                AutomationExecutionRecord {
                    status: "skipped".to_string(),
                    error: Some("execution mode saturated".to_string()),
                    results: Vec::new(),
                    duration_ms: 0,
                },
            );
        }
    }
}

// ── do_spawn_execution ────────────────────────────────────────────────────────
// The main async body of a single automation run: applies debounce/duration
// delays, checks the runtime-state policy, evaluates conditions, runs the Lua
// script, and records the result via the observer.

fn do_spawn_execution(
    automation: Automation,
    runtime: Arc<Runtime>,
    event: AttributeValue,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<AutomationStateStore>,
    cancel: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let delay_secs = event_number_field(&event, "duration_secs")
            .or_else(|| event_number_field(&event, "debounce_secs"));
        if let Some(delay_secs) = delay_secs {
            if !confirm_delayed_trigger(runtime.as_ref(), &event, delay_secs).await {
                finalize_and_maybe_dequeue(
                    automation,
                    runtime,
                    scripts_root,
                    execution,
                    observer,
                    state_store,
                );
                return;
            }
        }

        if should_skip_trigger(
            &automation,
            &event,
            state_store.as_ref().map(|store| &store.store),
            event_scheduled_at(&event),
        )
        .await
        .is_skip()
        {
            finalize_and_maybe_dequeue(
                automation,
                runtime,
                scripts_root,
                execution,
                observer,
                state_store,
            );
            return;
        }

        if let Some(reason) = first_failed_condition(
            &automation,
            runtime.as_ref(),
            &event,
            Utc::now(),
            execution.trigger_context,
        )
        .await
        {
            notify_observer(
                observer.as_ref(),
                &automation,
                event,
                AutomationExecutionRecord {
                    status: "skipped".to_string(),
                    error: Some(reason),
                    results: Vec::new(),
                    duration_ms: 0,
                },
            );
            finalize_and_maybe_dequeue(
                automation,
                runtime,
                scripts_root,
                execution,
                observer,
                state_store,
            );
            return;
        }

        let event_for_observer = event.clone();
        let automation_for_task = automation.clone();
        let scripts_root_for_task = scripts_root.clone();
        let state_store_for_task = state_store.clone();
        let max_instructions = execution.max_instructions;
        let runtime_for_task = runtime.clone();

        let join_handle = tokio::spawn(async move {
            execute_automation(
                &automation_for_task,
                runtime_for_task,
                event,
                scripts_root_for_task.as_deref(),
                cancel,
                max_instructions,
            )
        });

        tokio::pin!(join_handle);

        match timeout(execution.backstop_timeout, &mut join_handle).await {
            Ok(Ok(Ok(record))) => {
                persist_runtime_state(
                    &automation,
                    &event_for_observer,
                    state_store_for_task.as_ref(),
                    event_scheduled_at(&event_for_observer),
                )
                .await;
                notify_observer(observer.as_ref(), &automation, event_for_observer, record);
            }
            Ok(Ok(Err(error))) => {
                tracing::error!(automation = %automation.summary.id, error = %error, "automation execution failed");
                notify_observer(
                    observer.as_ref(),
                    &automation,
                    event_for_observer,
                    AutomationExecutionRecord {
                        status: "error".to_string(),
                        error: Some(error.to_string()),
                        results: Vec::new(),
                        duration_ms: 0,
                    },
                );
            }
            Ok(Err(error)) => {
                tracing::error!(automation = %automation.summary.id, error = %error, "automation task panicked");
                notify_observer(
                    observer.as_ref(),
                    &automation,
                    event_for_observer,
                    AutomationExecutionRecord {
                        status: "error".to_string(),
                        error: Some(error.to_string()),
                        results: Vec::new(),
                        duration_ms: 0,
                    },
                );
            }
            Err(_) => {
                join_handle.abort();
                tracing::error!(automation = %automation.summary.id, "automation execution exceeded backstop timeout");
                notify_observer(
                    observer.as_ref(),
                    &automation,
                    event_for_observer,
                    AutomationExecutionRecord {
                        status: "timeout".to_string(),
                        error: Some("automation execution exceeded backstop timeout".to_string()),
                        results: Vec::new(),
                        duration_ms: execution.backstop_timeout.as_millis() as i64,
                    },
                );
            }
        }

        finalize_and_maybe_dequeue(
            automation,
            runtime,
            scripts_root,
            execution,
            observer,
            state_store,
        );
    });
}

// ── finalize_and_maybe_dequeue ────────────────────────────────────────────────
// Decrements the active execution counter and, for Queued mode, pops the next
// pending trigger and starts it immediately so the queue drains in order.

fn finalize_and_maybe_dequeue(
    automation: Automation,
    runtime: Arc<Runtime>,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<AutomationStateStore>,
) {
    let next_event = {
        let mut map = execution
            .concurrency
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let state = map.entry(automation.summary.id.clone()).or_default();
        state.active = state.active.saturating_sub(1);
        if matches!(automation.mode, homecmdr_lua_host::ExecutionMode::Queued { .. }) {
            state.queue.pop_front().map(|pending| {
                state.active += 1;
                pending.event
            })
        } else {
            None
        }
    };

    if let Some(event) = next_event {
        do_spawn_execution(
            automation,
            runtime,
            event,
            scripts_root,
            execution,
            observer,
            state_store,
            Arc::new(AtomicBool::new(false)),
        );
    }
}

// ── notify_observer ───────────────────────────────────────────────────────────
// Calls the observer if one is configured, then returns.  The observer records
// execution history to the database.  Does nothing if no observer was registered.

fn notify_observer(
    observer: Option<&Arc<dyn AutomationExecutionObserver>>,
    automation: &Automation,
    trigger_payload: AttributeValue,
    record: AutomationExecutionRecord,
) {
    let Some(observer) = observer else {
        return;
    };

    observer.record(AutomationExecutionHistoryEntry {
        executed_at: Utc::now(),
        automation_id: automation.summary.id.clone(),
        trigger_payload,
        status: record.status,
        duration_ms: record.duration_ms,
        error: record.error,
        results: record.results,
    });
}

// ── Private helpers ───────────────────────────────────────────────────────────
// Small utilities used only inside this file.

/// Extract a positive integer field from an event object.  Returns `None` if
/// the field is absent, not an integer, or zero.
fn event_number_field(event: &AttributeValue, field: &str) -> Option<u64> {
    let AttributeValue::Object(fields) = event else {
        return None;
    };

    match fields.get(field) {
        Some(AttributeValue::Integer(value)) if *value > 0 => Some(*value as u64),
        _ => None,
    }
}

/// Wait `delay_secs` seconds and then check that the device attribute still
/// has the expected value.  Returns `false` (abort) if the condition is no
/// longer met, so the automation doesn't fire after the state has already
/// changed back.
async fn confirm_delayed_trigger(
    runtime: &Runtime,
    event: &AttributeValue,
    delay_secs: u64,
) -> bool {
    if delay_secs == 0 {
        return true;
    }

    let Some((device_id, attribute, expected_value)) = delayed_trigger_target(event) else {
        return true;
    };

    tokio::time::sleep(Duration::from_secs(delay_secs)).await;

    let Some(device) = runtime.registry().get(&DeviceId(device_id)) else {
        return false;
    };
    let Some(current_value) = device.attributes.get(&attribute) else {
        return false;
    };

    current_value == &expected_value
}

/// Extract the device id, attribute name, and expected value from a
/// `device_state_change` event so the delayed-trigger check knows what to
/// re-verify after the delay.
fn delayed_trigger_target(event: &AttributeValue) -> Option<(String, String, AttributeValue)> {
    let AttributeValue::Object(fields) = event else {
        return None;
    };
    let AttributeValue::Text(device_id) = fields.get("device_id")? else {
        return None;
    };
    let AttributeValue::Text(attribute) = fields.get("attribute")? else {
        return None;
    };
    let value = fields.get("value")?.clone();
    Some((device_id.clone(), attribute.clone(), value))
}

/// Extract the `scheduled_at` timestamp from a scheduled trigger event so
/// the resumable-schedule state check knows which slot this execution covers.
fn event_scheduled_at(event: &AttributeValue) -> Option<DateTime<Utc>> {
    let AttributeValue::Object(fields) = event else {
        return None;
    };
    let AttributeValue::Text(value) = fields.get("scheduled_at")? else {
        return None;
    };

    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}
