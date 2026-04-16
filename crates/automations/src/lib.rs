use std::collections::HashMap;
use std::fs;
use std::str::FromStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use cron::Schedule;
use mlua::{Function, HookTriggers, Lua, VmState};
use serde::Serialize;
use smart_home_core::event::Event;
use smart_home_core::model::{AttributeValue, Attributes, DeviceId};
use smart_home_core::runtime::Runtime;
use smart_home_core::store::{
    AutomationExecutionHistoryEntry, AutomationRuntimeState, DeviceStore, SceneStepResult,
};
use smart_home_lua_host::{
    attribute_to_lua_value, evaluate_module, LuaExecutionContext, LuaRuntimeOptions,
};
use sunrise::{Coordinates, SolarDay, SolarEvent};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};

const MAX_CONCURRENT_AUTOMATIONS: usize = 8;
const AUTOMATION_EXECUTION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub trigger_type: &'static str,
}

#[derive(Debug, Clone)]
pub struct Automation {
    pub summary: AutomationSummary,
    path: PathBuf,
    trigger: Trigger,
    runtime_state_policy: RuntimeStatePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationExecutionResult {
    pub status: String,
    pub error: Option<String>,
    pub results: Vec<SceneStepResult>,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Default)]
struct AutomationControlState {
    enabled: HashMap<String, bool>,
}

#[derive(Debug, Clone, Default)]
pub struct AutomationCatalog {
    automations: Vec<Automation>,
    scripts_root: Option<PathBuf>,
    control: Arc<RwLock<AutomationControlState>>,
}

#[derive(Debug, Clone, PartialEq)]
enum Trigger {
    DeviceStateChange {
        device_id: String,
        attribute: Option<String>,
        equals: Option<AttributeValue>,
        threshold: Option<ThresholdTrigger>,
        debounce_secs: Option<u64>,
        duration_secs: Option<u64>,
    },
    WeatherState {
        device_id: String,
        attribute: String,
        equals: Option<AttributeValue>,
        threshold: Option<ThresholdTrigger>,
        debounce_secs: Option<u64>,
        duration_secs: Option<u64>,
    },
    DeviceRoomChange {
        device_id: Option<String>,
        room_id: Option<String>,
    },
    RoomChange {
        room_id: Option<String>,
    },
    AdapterLifecycle {
        adapter: Option<String>,
        event: AdapterLifecycleEvent,
    },
    SystemError {
        contains: Option<String>,
    },
    WallClock {
        hour: u32,
        minute: u32,
    },
    Cron {
        expression: String,
        schedule: Schedule,
    },
    Sunrise {
        offset_mins: i64,
    },
    Sunset {
        offset_mins: i64,
    },
    Interval {
        every_secs: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AdapterLifecycleEvent {
    Started,
}

#[derive(Debug, Clone, PartialEq)]
struct ThresholdTrigger {
    above: Option<f64>,
    below: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TriggerContext {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

#[derive(Clone)]
pub struct AutomationRunner {
    catalog: AutomationCatalog,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
    trigger_context: TriggerContext,
}

#[derive(Clone)]
pub struct AutomationController {
    catalog: AutomationCatalog,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
}

#[derive(Clone)]
pub struct AutomationStateStore {
    store: Arc<dyn DeviceStore>,
}

#[derive(Clone)]
struct ExecutionControl {
    semaphore: Arc<Semaphore>,
    timeout: Duration,
}

pub trait AutomationExecutionObserver: Send + Sync {
    fn record(&self, entry: AutomationExecutionHistoryEntry);
}

#[derive(Debug)]
struct AutomationExecutionRecord {
    status: String,
    error: Option<String>,
    results: Vec<SceneStepResult>,
    duration_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct RuntimeStatePolicy {
    cooldown_secs: Option<u64>,
    dedupe_window_secs: Option<u64>,
    resumable_schedule: bool,
}

#[derive(Debug, Clone, Default)]
struct LoadedAutomationRuntimeState {
    last_triggered_at: Option<DateTime<Utc>>,
    last_trigger_fingerprint: Option<String>,
    last_scheduled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
enum TriggerDecision {
    Execute,
    Skip,
}

impl AutomationCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load_from_directory(
        path: impl AsRef<Path>,
        scripts_root: Option<PathBuf>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let entries = fs::read_dir(path)
            .with_context(|| format!("failed to read automations directory {}", path.display()))?;
        let mut automations = Vec::new();
        let mut ids = HashMap::new();

        for entry in entries {
            let entry = entry.context("failed to read automations directory entry")?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            if !file_type.is_file() {
                continue;
            }

            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("lua") {
                continue;
            }

            let automation = load_automation_file(&entry.path(), scripts_root.as_deref())?;
            if ids
                .insert(automation.summary.id.clone(), automation.path.clone())
                .is_some()
            {
                bail!("duplicate automation id '{}'", automation.summary.id);
            }
            automations.push(automation);
        }

        automations.sort_by(|a, b| a.summary.id.cmp(&b.summary.id));
        Ok(Self {
            automations,
            scripts_root,
            control: Arc::new(RwLock::new(AutomationControlState::default())),
        })
    }

    pub fn summaries(&self) -> Vec<AutomationSummary> {
        self.automations
            .iter()
            .map(|automation| automation.summary.clone())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<AutomationSummary> {
        self.automations
            .iter()
            .find(|automation| automation.summary.id == id)
            .map(|automation| automation.summary.clone())
    }

    pub fn is_enabled(&self, id: &str) -> Option<bool> {
        self.automations
            .iter()
            .find(|automation| automation.summary.id == id)
            .map(|_| self.read_control().enabled.get(id).copied().unwrap_or(true))
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        if self.automations.iter().all(|automation| automation.summary.id != id) {
            bail!("automation '{id}' not found");
        }

        self.write_control().enabled.insert(id.to_string(), enabled);
        Ok(enabled)
    }

    pub fn validate(&self, id: &str) -> Result<AutomationSummary> {
        let automation = self
            .automations
            .iter()
            .find(|automation| automation.summary.id == id)
            .with_context(|| format!("automation '{id}' not found"))?;
        let reloaded = load_automation_file(&automation.path, self.scripts_root.as_deref())?;
        Ok(reloaded.summary)
    }

    pub fn execute(
        &self,
        id: &str,
        runtime: Arc<Runtime>,
        trigger_payload: AttributeValue,
    ) -> Result<AutomationExecutionResult> {
        let automation = self
            .automations
            .iter()
            .find(|automation| automation.summary.id == id)
            .with_context(|| format!("automation '{id}' not found"))?;

        let record = execute_automation(
            automation,
            runtime,
            trigger_payload,
            self.scripts_root.as_deref(),
            AUTOMATION_EXECUTION_TIMEOUT,
        )?;

        Ok(AutomationExecutionResult {
            status: record.status,
            error: record.error,
            results: record.results,
            duration_ms: record.duration_ms,
        })
    }

    fn read_control(&self) -> std::sync::RwLockReadGuard<'_, AutomationControlState> {
        match self.control.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write_control(&self) -> std::sync::RwLockWriteGuard<'_, AutomationControlState> {
        match self.control.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl AutomationRunner {
    pub fn new(catalog: AutomationCatalog) -> Self {
        Self {
            catalog,
            observer: None,
            state_store: None,
            trigger_context: TriggerContext::default(),
        }
    }

    pub fn with_observer(mut self, observer: Arc<dyn AutomationExecutionObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn with_state_store(mut self, store: Arc<dyn DeviceStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    pub fn with_trigger_context(mut self, trigger_context: TriggerContext) -> Self {
        self.trigger_context = trigger_context;
        self
    }

    pub fn controller(&self) -> AutomationController {
        AutomationController {
            catalog: self.catalog.clone(),
            observer: self.observer.clone(),
        }
    }

    pub async fn run(self, runtime: Arc<Runtime>) {
        self.run_with_options(
            runtime,
            ExecutionControl {
                semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_AUTOMATIONS)),
                timeout: AUTOMATION_EXECUTION_TIMEOUT,
            },
        )
        .await;
    }

    async fn run_with_options(self, runtime: Arc<Runtime>, execution: ExecutionControl) {
        let mut tasks = tokio::task::JoinSet::new();

        if self.catalog.automations.iter().any(trigger_uses_event_bus)
        {
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
                Trigger::Interval { every_secs } => {
                    if !self.catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
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
                Trigger::WallClock { .. }
                | Trigger::Cron { .. }
                | Trigger::Sunrise { .. }
                | Trigger::Sunset { .. } => {
                    if !self.catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
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

impl AutomationController {
    pub fn summaries(&self) -> Vec<AutomationSummary> {
        self.catalog.summaries()
    }

    pub fn get(&self, id: &str) -> Option<AutomationSummary> {
        self.catalog.get(id)
    }

    pub fn is_enabled(&self, id: &str) -> Option<bool> {
        self.catalog.is_enabled(id)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        self.catalog.set_enabled(id, enabled)
    }

    pub fn validate(&self, id: &str) -> Result<AutomationSummary> {
        self.catalog.validate(id)
    }

    pub fn execute(
        &self,
        id: &str,
        runtime: Arc<Runtime>,
        trigger_payload: AttributeValue,
    ) -> Result<AutomationExecutionResult> {
        let result = self.catalog.execute(id, runtime, trigger_payload.clone())?;
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

async fn run_event_trigger_loop(
    runtime: Arc<Runtime>,
    catalog: AutomationCatalog,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
) {
    let mut receiver = runtime.bus().subscribe();
    let mut executions = JoinSet::new();

    loop {
        match receiver.recv().await {
            Ok(event) => {
                for automation in &catalog.automations {
                    if !catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
                        continue;
                    }
                    if let Some(event_value) =
                        automation_event_from_runtime_event(automation, &event)
                    {
                        let state_store = state_store.as_ref().map(|store| AutomationStateStore {
                            store: store.clone(),
                        });
                        spawn_automation_execution(
                            &mut executions,
                            automation.clone(),
                            runtime.clone(),
                            event_value,
                            scripts_root.clone(),
                            execution.clone(),
                            observer.clone(),
                            state_store,
                        );
                    }
                }

                while executions.try_join_next().is_some() {}
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "automation event trigger loop lagged behind");
                recover_lagged_event_automations(
                    &mut executions,
                    runtime.clone(),
                    &catalog,
                    scripts_root.clone(),
                    execution.clone(),
                    skipped,
                    observer.clone(),
                    state_store.clone(),
                );
                while executions.try_join_next().is_some() {}
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn run_interval_trigger_loop(
    runtime: Arc<Runtime>,
    catalog: AutomationCatalog,
    automation: Automation,
    every_secs: u64,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(every_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        if !catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
            continue;
        }

        let event = AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("interval".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(Utc::now().to_rfc3339()),
            ),
            (
                "every_secs".to_string(),
                AttributeValue::Integer(every_secs as i64),
            ),
        ]));

        if should_skip_trigger(
            &automation,
            &event,
            state_store.as_ref(),
            Some(Utc::now()),
        )
        .await
        .is_skip()
        {
            continue;
        }

        execute_scheduled_automation(
            runtime.clone(),
            &automation,
            event,
            scripts_root.clone(),
            execution.clone(),
            observer.clone(),
            state_store.as_ref().map(|store| AutomationStateStore {
                store: store.clone(),
            }),
            Some(Utc::now()),
        )
        .await;
    }
}

async fn run_scheduled_trigger_loop(
    runtime: Arc<Runtime>,
    catalog: AutomationCatalog,
    automation: Automation,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
    trigger_context: TriggerContext,
) {
    let next_fire_at = next_scheduled_fire_after(
        &automation,
        state_store.as_ref().map(|store| AutomationStateStore { store: store.clone() }),
        Utc::now(),
        trigger_context,
    )
    .await
    .unwrap_or_else(|| next_schedule_time(&automation.trigger, Utc::now(), trigger_context));
    let mut next_fire_at = match next_fire_at {
        Some(next_fire_at) => next_fire_at,
        None => {
            tracing::error!(automation = %automation.summary.id, "scheduled automation has no next fire time");
            return;
        }
    };

    loop {
        let now = Utc::now();
        let sleep_duration = next_fire_at
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default();
        tokio::time::sleep(sleep_duration).await;

        if !catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
            next_fire_at = match next_schedule_time(&automation.trigger, Utc::now(), trigger_context) {
                Some(next_fire_at) => next_fire_at,
                None => break,
            };
            continue;
        }

        let scheduled_for = next_fire_at;
        next_fire_at = match next_schedule_time(&automation.trigger, scheduled_for, trigger_context) {
            Some(next_fire_at) => next_fire_at,
            None => break,
        };

        let event = scheduled_trigger_event(&automation.trigger, scheduled_for);
        execute_scheduled_automation(
            runtime.clone(),
            &automation,
            event,
            scripts_root.clone(),
            execution.clone(),
            observer.clone(),
            state_store.as_ref().map(|store| AutomationStateStore {
                store: store.clone(),
            }),
            Some(scheduled_for),
        )
        .await;
    }
}

async fn execute_scheduled_automation(
    runtime: Arc<Runtime>,
    automation: &Automation,
    event: AttributeValue,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<AutomationStateStore>,
    scheduled_for: Option<DateTime<Utc>>,
) {
    let delay_secs = event_number_field(&event, "duration_secs")
        .or_else(|| event_number_field(&event, "debounce_secs"));
    if let Some(delay_secs) = delay_secs {
        if !confirm_delayed_trigger(runtime.as_ref(), &event, delay_secs).await {
            return;
        }
    }

    if should_skip_trigger(automation, &event, state_store.as_ref().map(|store| &store.store), scheduled_for)
        .await
        .is_skip()
    {
        return;
    }

    let permit = match execution.semaphore.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => {
            tracing::error!(automation = %automation.summary.id, error = %error, "automation runner semaphore closed");
            return;
        }
    };

    let automation_clone = automation.clone();
    let runtime_clone = runtime.clone();
    let scripts_root_clone = scripts_root.clone();
    let timeout_duration = execution.timeout;
    let event_for_observer = event.clone();
    let event_for_task = event.clone();
    let join_handle = tokio::spawn(async move {
        let _permit = permit;
        execute_automation(
            &automation_clone,
            runtime_clone,
            event_for_task,
            scripts_root_clone.as_deref(),
            timeout_duration,
        )
    });

    tokio::pin!(join_handle);

    match timeout(timeout_duration, &mut join_handle).await {
        Ok(Ok(Ok(record))) => {
            persist_runtime_state(automation, &event, state_store.as_ref(), scheduled_for).await;
            notify_observer(observer.as_ref(), automation, event_for_observer, record);
        }
        Ok(Ok(Err(error))) => {
            tracing::error!(automation = %automation.summary.id, error = %error, "scheduled automation execution failed");
            notify_observer(
                observer.as_ref(),
                automation,
                event_for_observer,
                AutomationExecutionRecord {
                    status: "error".to_string(),
                    error: Some(error.to_string()),
                    results: Vec::new(),
                    duration_ms: timeout_duration.as_millis() as i64,
                },
            );
        }
        Ok(Err(error)) => {
            tracing::error!(automation = %automation.summary.id, error = %error, "scheduled automation task failed");
            notify_observer(
                observer.as_ref(),
                automation,
                event_for_observer,
                AutomationExecutionRecord {
                    status: "error".to_string(),
                    error: Some(error.to_string()),
                    results: Vec::new(),
                    duration_ms: timeout_duration.as_millis() as i64,
                },
            );
        }
        Err(_) => {
            join_handle.abort();
            tracing::error!(automation = %automation.summary.id, timeout_secs = timeout_duration.as_secs(), "scheduled automation execution timed out");
            notify_observer(
                observer.as_ref(),
                automation,
                event_for_observer,
                AutomationExecutionRecord {
                    status: "timeout".to_string(),
                    error: Some("automation execution timed out".to_string()),
                    results: Vec::new(),
                    duration_ms: timeout_duration.as_millis() as i64,
                },
            );
        }
    }
}

fn scheduled_trigger_event(trigger: &Trigger, scheduled_at: DateTime<Utc>) -> AttributeValue {
    match trigger {
        Trigger::WallClock { hour, minute } => AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("wall_clock".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(scheduled_at.to_rfc3339()),
            ),
            (
                "hour".to_string(),
                AttributeValue::Integer(*hour as i64),
            ),
            (
                "minute".to_string(),
                AttributeValue::Integer(*minute as i64),
            ),
            (
                "timezone".to_string(),
                AttributeValue::Text("UTC".to_string()),
            ),
        ])),
        Trigger::Cron { expression, .. } => AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("cron".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(scheduled_at.to_rfc3339()),
            ),
            (
                "expression".to_string(),
                AttributeValue::Text(expression.clone()),
            ),
            (
                "timezone".to_string(),
                AttributeValue::Text("UTC".to_string()),
            ),
        ])),
        Trigger::Sunrise { offset_mins } => AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("sunrise".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(scheduled_at.to_rfc3339()),
            ),
            (
                "offset_mins".to_string(),
                AttributeValue::Integer(*offset_mins),
            ),
            (
                "timezone".to_string(),
                AttributeValue::Text("UTC".to_string()),
            ),
        ])),
        Trigger::Sunset { offset_mins } => AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("sunset".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(scheduled_at.to_rfc3339()),
            ),
            (
                "offset_mins".to_string(),
                AttributeValue::Integer(*offset_mins),
            ),
            (
                "timezone".to_string(),
                AttributeValue::Text("UTC".to_string()),
            ),
        ])),
        _ => AttributeValue::Object(HashMap::from([(
            "scheduled_at".to_string(),
            AttributeValue::Text(scheduled_at.to_rfc3339()),
        )])),
    }
}

fn next_schedule_time(
    trigger: &Trigger,
    after: DateTime<Utc>,
    trigger_context: TriggerContext,
) -> Option<DateTime<Utc>> {
    match trigger {
        Trigger::WallClock { hour, minute } => next_wall_clock_occurrence(*hour, *minute, after),
        Trigger::Cron { schedule, .. } => schedule.after(&after).next(),
        Trigger::Sunrise { offset_mins } => next_solar_occurrence(after, trigger_context, true, *offset_mins),
        Trigger::Sunset { offset_mins } => next_solar_occurrence(after, trigger_context, false, *offset_mins),
        _ => None,
    }
}

fn next_solar_occurrence(
    after: DateTime<Utc>,
    trigger_context: TriggerContext,
    sunrise: bool,
    offset_mins: i64,
) -> Option<DateTime<Utc>> {
    let (latitude, longitude) = (trigger_context.latitude?, trigger_context.longitude?);

    for day_offset in 0..=366 {
        let date = after.date_naive().checked_add_signed(ChronoDuration::days(day_offset))?;
        let event_time = solar_event_time(date, latitude, longitude, sunrise, offset_mins)?;
        if event_time > after {
            return Some(event_time);
        }
    }

    None
}

fn solar_event_time(
    date: NaiveDate,
    latitude: f64,
    longitude: f64,
    sunrise: bool,
    offset_mins: i64,
) -> Option<DateTime<Utc>> {
    let coordinates = Coordinates::new(latitude, longitude)?;
    let solar_day = SolarDay::new(coordinates, date);
    let base = solar_day.event_time(if sunrise {
        SolarEvent::Sunrise
    } else {
        SolarEvent::Sunset
    });
    Some(base + ChronoDuration::minutes(offset_mins))
}

fn next_wall_clock_occurrence(
    hour: u32,
    minute: u32,
    after: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let scheduled_today = after
        .date_naive()
        .and_hms_opt(hour, minute, 0)?;
    let scheduled_today = DateTime::<Utc>::from_naive_utc_and_offset(scheduled_today, Utc);

    if scheduled_today > after {
        return Some(scheduled_today);
    }

    let next_day = after.date_naive().checked_add_signed(ChronoDuration::days(1))?;
    let scheduled_next = next_day.and_hms_opt(hour, minute, 0)?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(scheduled_next, Utc))
}

fn spawn_automation_execution(
    executions: &mut JoinSet<()>,
    automation: Automation,
    runtime: Arc<Runtime>,
    event: AttributeValue,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<AutomationStateStore>,
) {
    let Ok(permit) = execution.semaphore.clone().try_acquire_owned() else {
        tracing::warn!(automation = %automation.summary.id, "skipping automation execution because the runner is saturated");
        notify_observer(
            observer.as_ref(),
            &automation,
            event,
            AutomationExecutionRecord {
                status: "skipped".to_string(),
                error: Some("automation runner saturated".to_string()),
                results: Vec::new(),
                duration_ms: 0,
            },
        );
        return;
    };

    executions.spawn(async move {
        let delay_secs = event_number_field(&event, "duration_secs")
            .or_else(|| event_number_field(&event, "debounce_secs"));
        if let Some(delay_secs) = delay_secs {
            if !confirm_delayed_trigger(runtime.as_ref(), &event, delay_secs).await {
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
            return;
        }

        let timeout_duration = execution.timeout;
        let automation_for_task = automation.clone();
        let event_for_observer = event.clone();
        let state_store_for_task = state_store.clone();
        let join_handle = tokio::spawn(async move {
            let _permit = permit;
            execute_automation(
                &automation_for_task,
                runtime,
                event,
                scripts_root.as_deref(),
                timeout_duration,
            )
        });

        tokio::pin!(join_handle);

        match timeout(timeout_duration, &mut join_handle).await {
            Ok(Ok(Ok(record))) => {
                persist_runtime_state(
                    &automation,
                    &event_for_observer,
                    state_store_for_task.as_ref(),
                    event_scheduled_at(&event_for_observer),
                )
                .await;
                notify_observer(
                    observer.as_ref(),
                    &automation,
                    event_for_observer,
                    record,
                );
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
                        duration_ms: timeout_duration.as_millis() as i64,
                    },
                );
            }
            Ok(Err(error)) => {
                tracing::error!(automation = %automation.summary.id, error = %error, "automation task failed");
                notify_observer(
                    observer.as_ref(),
                    &automation,
                    event_for_observer,
                    AutomationExecutionRecord {
                        status: "error".to_string(),
                        error: Some(error.to_string()),
                        results: Vec::new(),
                        duration_ms: timeout_duration.as_millis() as i64,
                    },
                );
            }
            Err(_) => {
                join_handle.abort();
                tracing::error!(automation = %automation.summary.id, timeout_secs = timeout_duration.as_secs(), "automation execution timed out");
                notify_observer(
                    observer.as_ref(),
                    &automation,
                    event_for_observer,
                    AutomationExecutionRecord {
                        status: "timeout".to_string(),
                        error: Some("automation execution timed out".to_string()),
                        results: Vec::new(),
                        duration_ms: timeout_duration.as_millis() as i64,
                    },
                );
            }
        }
    });
}

impl AutomationStateStore {
    async fn load(&self, automation_id: &str) -> Option<LoadedAutomationRuntimeState> {
        match self.store.load_automation_runtime_state(automation_id).await {
            Ok(state) => state.map(|state| LoadedAutomationRuntimeState {
                last_triggered_at: state.last_triggered_at,
                last_trigger_fingerprint: state.last_trigger_fingerprint,
                last_scheduled_at: state.last_scheduled_at,
            }),
            Err(error) => {
                tracing::error!(automation = %automation_id, error = %error, "failed to load automation runtime state");
                None
            }
        }
    }

    async fn save(&self, state: &AutomationRuntimeState) {
        if let Err(error) = self.store.save_automation_runtime_state(state).await {
            tracing::error!(automation = %state.automation_id, error = %error, "failed to save automation runtime state");
        }
    }
}

impl TriggerDecision {
    fn is_skip(&self) -> bool {
        matches!(self, Self::Skip)
    }
}

fn event_number_field(event: &AttributeValue, field: &str) -> Option<u64> {
    let AttributeValue::Object(fields) = event else {
        return None;
    };

    match fields.get(field) {
        Some(AttributeValue::Integer(value)) if *value > 0 => Some(*value as u64),
        _ => None,
    }
}

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

async fn should_skip_trigger(
    automation: &Automation,
    event: &AttributeValue,
    state_store: Option<&Arc<dyn DeviceStore>>,
    scheduled_for: Option<DateTime<Utc>>,
) -> TriggerDecision {
    let policy = &automation.runtime_state_policy;
    if policy.cooldown_secs.is_none() && policy.dedupe_window_secs.is_none() && !policy.resumable_schedule {
        return TriggerDecision::Execute;
    }

    let Some(store) = state_store else {
        return TriggerDecision::Execute;
    };
    let state_store = AutomationStateStore {
        store: store.clone(),
    };
    let state = state_store.load(&automation.summary.id).await.unwrap_or_default();
    let now = Utc::now();

    if let Some(cooldown_secs) = policy.cooldown_secs {
        if let Some(last_triggered_at) = state.last_triggered_at {
            if now < last_triggered_at + ChronoDuration::seconds(cooldown_secs as i64) {
                return TriggerDecision::Skip;
            }
        }
    }

    if let Some(dedupe_window_secs) = policy.dedupe_window_secs {
        let fingerprint = trigger_fingerprint(event);
        if let (Some(last_triggered_at), Some(last_fingerprint)) =
            (state.last_triggered_at, state.last_trigger_fingerprint.as_deref())
        {
            if last_fingerprint == fingerprint
                && now < last_triggered_at + ChronoDuration::seconds(dedupe_window_secs as i64)
            {
                return TriggerDecision::Skip;
            }
        }
    }

    if policy.resumable_schedule {
        if let (Some(last_scheduled_at), Some(scheduled_for)) = (state.last_scheduled_at, scheduled_for) {
            if scheduled_for <= last_scheduled_at {
                return TriggerDecision::Skip;
            }
        }
    }

    TriggerDecision::Execute
}

async fn persist_runtime_state(
    automation: &Automation,
    event: &AttributeValue,
    state_store: Option<&AutomationStateStore>,
    scheduled_for: Option<DateTime<Utc>>,
) {
    let Some(state_store) = state_store else {
        return;
    };

    state_store
        .save(&AutomationRuntimeState {
            updated_at: Utc::now(),
            automation_id: automation.summary.id.clone(),
            last_triggered_at: Some(Utc::now()),
            last_trigger_fingerprint: Some(trigger_fingerprint(event).to_string()),
            last_scheduled_at: scheduled_for,
        })
        .await;
}

async fn next_scheduled_fire_after(
    automation: &Automation,
    state_store: Option<AutomationStateStore>,
    now: DateTime<Utc>,
    trigger_context: TriggerContext,
) -> Option<Option<DateTime<Utc>>> {
    if !automation.runtime_state_policy.resumable_schedule {
        return None;
    }
    let Some(state_store) = state_store else {
        return None;
    };

    let state = state_store.load(&automation.summary.id).await.unwrap_or_default();
    let anchor = state.last_scheduled_at.unwrap_or(now);
    Some(next_schedule_time(&automation.trigger, anchor, trigger_context))
}

fn parse_runtime_state_policy(module: &mlua::Table, path: &Path) -> Result<RuntimeStatePolicy> {
    let state = module.get::<Option<mlua::Table>>("state").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} has invalid optional field 'state': {error}",
            path.display()
        )
    })?;
    let Some(state) = state else {
        return Ok(RuntimeStatePolicy::default());
    };

    Ok(RuntimeStatePolicy {
        cooldown_secs: state.get::<Option<u64>>("cooldown_secs").map_err(|error| {
            anyhow::anyhow!(
                "automation file {} has invalid optional state field 'cooldown_secs': {error}",
                path.display()
            )
        })?,
        dedupe_window_secs: state.get::<Option<u64>>("dedupe_window_secs").map_err(|error| {
            anyhow::anyhow!(
                "automation file {} has invalid optional state field 'dedupe_window_secs': {error}",
                path.display()
            )
        })?,
        resumable_schedule: state.get::<Option<bool>>("resumable_schedule").map_err(|error| {
            anyhow::anyhow!(
                "automation file {} has invalid optional state field 'resumable_schedule': {error}",
                path.display()
            )
        })?.unwrap_or(false),
    })
}

fn trigger_fingerprint(event: &AttributeValue) -> String {
    canonicalize_attribute_value(event)
}

fn canonicalize_attribute_value(value: &AttributeValue) -> String {
    match value {
        AttributeValue::Integer(value) => value.to_string(),
        AttributeValue::Float(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
        }
        AttributeValue::Bool(value) => value.to_string(),
        AttributeValue::Text(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
        }
        AttributeValue::Array(values) => {
            let values = values
                .iter()
                .map(canonicalize_attribute_value)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{values}]")
        }
        AttributeValue::Object(fields) => {
            let mut entries = fields.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let entries = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_else(|_| "null".to_string()),
                        canonicalize_attribute_value(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{entries}}}")
        }
        AttributeValue::Null => "null".to_string(),
    }
}

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

fn recover_lagged_event_automations(
    executions: &mut JoinSet<()>,
    runtime: Arc<Runtime>,
    catalog: &AutomationCatalog,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    skipped: u64,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
) {
    for automation in &catalog.automations {
        if !catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
            continue;
        }
        if let Some(event_value) = automation_event_from_registry_snapshot(automation, runtime.registry(), skipped) {
            spawn_automation_execution(
                executions,
                automation.clone(),
                runtime.clone(),
                event_value,
                scripts_root.clone(),
                execution.clone(),
                observer.clone(),
                state_store.as_ref().map(|store| AutomationStateStore {
                    store: store.clone(),
                }),
            );
        }
    }
}

fn execute_automation(
    automation: &Automation,
    runtime: Arc<Runtime>,
    event: AttributeValue,
    scripts_root: Option<&Path>,
    timeout_duration: Duration,
) -> Result<AutomationExecutionRecord> {
    let started = Instant::now();
    let source = fs::read_to_string(&automation.path).with_context(|| {
        format!(
            "failed to read automation file {}",
            automation.path.display()
        )
    })?;
    let lua = Lua::new();
    install_execution_timeout_hook(&lua, timeout_duration);
    let module = evaluate_automation_module(&lua, &source, &automation.path, scripts_root)?;
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

fn install_execution_timeout_hook(lua: &Lua, timeout_duration: Duration) {
    let started = Instant::now();
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(10_000),
        move |_lua, _debug| {
            if started.elapsed() >= timeout_duration {
                Err(mlua::Error::runtime("automation execution timed out"))
            } else {
                Ok(VmState::Continue)
            }
        },
    );
}

fn automation_event_from_runtime_event(
    automation: &Automation,
    event: &Event,
) -> Option<AttributeValue> {
    match (&automation.trigger, event) {
        (
            Trigger::DeviceStateChange {
                device_id,
                attribute,
                equals,
                threshold,
                debounce_secs,
                duration_secs,
            },
            Event::DeviceStateChanged {
                id,
                attributes,
                previous_attributes,
            },
        ) => device_state_event_from_change(
            "device_state_change",
            device_id,
            attribute.as_deref(),
            equals.as_ref(),
            threshold.as_ref(),
            *debounce_secs,
            *duration_secs,
            &id.0,
            attributes,
            previous_attributes,
            None,
        ),
        (
            Trigger::WeatherState {
                device_id,
                attribute,
                equals,
                threshold,
                debounce_secs,
                duration_secs,
            },
            Event::DeviceStateChanged {
                id,
                attributes,
                previous_attributes,
            },
        ) => device_state_event_from_change(
            "weather_state",
            device_id,
            Some(attribute.as_str()),
            equals.as_ref(),
            threshold.as_ref(),
            *debounce_secs,
            *duration_secs,
            &id.0,
            attributes,
            previous_attributes,
            None,
        ),
        (
            Trigger::DeviceRoomChange { device_id, room_id },
            Event::DeviceRoomChanged { id, room_id: changed_room_id },
        ) => {
            if let Some(expected_device_id) = device_id {
                if &id.0 != expected_device_id {
                    return None;
                }
            }
            if let Some(expected_room_id) = room_id {
                if changed_room_id.as_ref().map(|value| value.0.as_str()) != Some(expected_room_id.as_str()) {
                    return None;
                }
            }

            Some(device_room_change_event(&id.0, changed_room_id.as_ref().map(|value| value.0.as_str())))
        }
        (Trigger::RoomChange { room_id }, Event::RoomAdded { room })
        | (Trigger::RoomChange { room_id }, Event::RoomUpdated { room }) => {
            if room_id.as_deref().is_some_and(|expected| expected != room.id.0) {
                return None;
            }

            Some(room_change_event(room, false))
        }
        (Trigger::RoomChange { room_id }, Event::RoomRemoved { id }) => {
            if room_id.as_deref().is_some_and(|expected| expected != id.0) {
                return None;
            }

            Some(room_removed_event(&id.0))
        }
        (
            Trigger::AdapterLifecycle { adapter, event },
            Event::AdapterStarted { adapter: started_adapter },
        ) => {
            if *event != AdapterLifecycleEvent::Started {
                return None;
            }
            if adapter.as_deref().is_some_and(|expected| expected != started_adapter) {
                return None;
            }

            Some(adapter_started_event(started_adapter))
        }
        (Trigger::SystemError { contains }, Event::SystemError { message }) => {
            if contains.as_deref().is_some_and(|needle| !message.contains(needle)) {
                return None;
            }

            Some(system_error_event(message))
        }
        _ => None,
    }
}

fn automation_event_from_registry_snapshot(
    automation: &Automation,
    registry: &smart_home_core::registry::DeviceRegistry,
    skipped: u64,
) -> Option<AttributeValue> {
    match &automation.trigger {
        Trigger::DeviceStateChange {
            device_id,
            attribute,
            equals,
            threshold,
            debounce_secs,
            duration_secs,
        } => {
            let device = registry.get(&DeviceId(device_id.clone()))?;
            device_state_event_from_snapshot(
                "device_state_change",
                &device.id.0,
                &device.attributes,
                attribute.as_deref(),
                equals.as_ref(),
                threshold.as_ref(),
                *debounce_secs,
                *duration_secs,
                skipped,
            )
        }
        Trigger::WeatherState {
            device_id,
            attribute,
            equals,
            threshold,
            debounce_secs,
            duration_secs,
        } => {
            let device = registry.get(&DeviceId(device_id.clone()))?;
            device_state_event_from_snapshot(
                "weather_state",
                &device.id.0,
                &device.attributes,
                Some(attribute.as_str()),
                equals.as_ref(),
                threshold.as_ref(),
                *debounce_secs,
                *duration_secs,
                skipped,
            )
        }
        Trigger::DeviceRoomChange { device_id, room_id } => {
            let device = match device_id {
                Some(device_id) => registry.get(&DeviceId(device_id.clone()))?,
                None => registry
                    .list()
                    .into_iter()
                    .find(|device| match room_id {
                        Some(expected_room_id) => {
                            device.room_id.as_ref().map(|value| value.0.as_str())
                                == Some(expected_room_id.as_str())
                        }
                        None => true,
                    })?,
            };

            if let Some(expected_room_id) = room_id {
                if device.room_id.as_ref().map(|value| value.0.as_str()) != Some(expected_room_id.as_str()) {
                    return None;
                }
            }

            Some(recovered_event(
                device_room_change_event(
                    &device.id.0,
                    device.room_id.as_ref().map(|value| value.0.as_str()),
                ),
                skipped,
            ))
        }
        Trigger::RoomChange { room_id } => {
            let room = match room_id {
                Some(room_id) => registry.get_room(&smart_home_core::model::RoomId(room_id.clone()))?,
                None => registry.list_rooms().into_iter().next()?,
            };

            Some(recovered_event(room_change_event(&room, false), skipped))
        }
        _ => None,
    }
}

fn device_state_event_from_change(
    event_type: &str,
    expected_device_id: &str,
    attribute_name: Option<&str>,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdTrigger>,
    debounce_secs: Option<u64>,
    duration_secs: Option<u64>,
    actual_device_id: &str,
    attributes: &Attributes,
    previous_attributes: &Attributes,
    recovered_skipped: Option<u64>,
) -> Option<AttributeValue> {
    if actual_device_id != expected_device_id {
        return None;
    }

    if let Some(attribute_name) = attribute_name {
        let value = attributes.get(attribute_name)?;
        let previous_value = previous_attributes.get(attribute_name).cloned();
        let matches_now = attribute_matches(value, equals, threshold);
        let matched_before = previous_value
            .as_ref()
            .is_some_and(|previous| attribute_matches(previous, equals, threshold));

        if duration_secs.is_some() && !matches_now {
            return None;
        }

        if duration_secs.is_none() {
            if debounce_secs.is_some() {
                if !matches_now || matched_before {
                    return None;
                }
            } else if threshold.is_some() {
                if !crossed_threshold(previous_value.as_ref(), value, threshold?) {
                    return None;
                }
            } else if !matches_now {
                return None;
            }
        }

        return Some(device_state_change_event(
            event_type,
            actual_device_id,
            attributes,
            Some(attribute_name),
            Some(value.clone()),
            previous_value,
            debounce_secs,
            duration_secs,
            threshold,
            recovered_skipped,
        ));
    }

    Some(device_state_change_event(
        event_type,
        actual_device_id,
        attributes,
        None,
        None,
        None,
        debounce_secs,
        duration_secs,
        threshold,
        recovered_skipped,
    ))
}

fn device_state_event_from_snapshot(
    event_type: &str,
    device_id: &str,
    attributes: &Attributes,
    attribute_name: Option<&str>,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdTrigger>,
    debounce_secs: Option<u64>,
    duration_secs: Option<u64>,
    skipped: u64,
) -> Option<AttributeValue> {
    if let Some(attribute_name) = attribute_name {
        let value = attributes.get(attribute_name)?;
        if !attribute_matches(value, equals, threshold) {
            return None;
        }

        return Some(device_state_change_event(
            event_type,
            device_id,
            attributes,
            Some(attribute_name),
            Some(value.clone()),
            None,
            debounce_secs,
            duration_secs,
            threshold,
            Some(skipped),
        ));
    }

    Some(device_state_change_event(
        event_type,
        device_id,
        attributes,
        None,
        None,
        None,
        debounce_secs,
        duration_secs,
        threshold,
        Some(skipped),
    ))
}

fn attribute_matches(
    value: &AttributeValue,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdTrigger>,
) -> bool {
    if let Some(expected) = equals {
        return value == expected;
    }

    if let Some(threshold) = threshold {
        let Some(number) = attribute_value_to_f64(value) else {
            return false;
        };
        if let Some(above) = threshold.above {
            if number <= above {
                return false;
            }
        }
        if let Some(below) = threshold.below {
            if number >= below {
                return false;
            }
        }
    }

    true
}

fn crossed_threshold(
    previous_value: Option<&AttributeValue>,
    current_value: &AttributeValue,
    threshold: &ThresholdTrigger,
) -> bool {
    let Some(previous_number) = previous_value.and_then(attribute_value_to_f64) else {
        return false;
    };
    let Some(current_number) = attribute_value_to_f64(current_value) else {
        return false;
    };

    if let Some(above) = threshold.above {
        if previous_number <= above && current_number > above {
            return true;
        }
    }
    if let Some(below) = threshold.below {
        if previous_number >= below && current_number < below {
            return true;
        }
    }

    false
}

fn attribute_value_to_f64(value: &AttributeValue) -> Option<f64> {
    match value {
        AttributeValue::Integer(value) => Some(*value as f64),
        AttributeValue::Float(value) => Some(*value),
        AttributeValue::Object(fields) => match fields.get("value") {
            Some(AttributeValue::Integer(value)) => Some(*value as f64),
            Some(AttributeValue::Float(value)) => Some(*value),
            _ => None,
        },
        _ => None,
    }
}

fn device_state_change_event(
    event_type: &str,
    device_id: &str,
    attributes: &Attributes,
    attribute_name: Option<&str>,
    value: Option<AttributeValue>,
    previous_value: Option<AttributeValue>,
    debounce_secs: Option<u64>,
    duration_secs: Option<u64>,
    threshold: Option<&ThresholdTrigger>,
    recovered_skipped: Option<u64>,
) -> AttributeValue {
    let mut event = HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text(event_type.to_string()),
        ),
        (
            "device_id".to_string(),
            AttributeValue::Text(device_id.to_string()),
        ),
        (
            "attributes".to_string(),
            AttributeValue::Object(attributes.clone()),
        ),
    ]);

    if let Some(attribute_name) = attribute_name {
        event.insert(
            "attribute".to_string(),
            AttributeValue::Text(attribute_name.to_string()),
        );
    }

    if let Some(value) = value {
        event.insert("value".to_string(), value);
    }

    if let Some(previous_value) = previous_value {
        event.insert("previous_value".to_string(), previous_value);
    }

    if let Some(debounce_secs) = debounce_secs {
        event.insert(
            "debounce_secs".to_string(),
            AttributeValue::Integer(debounce_secs as i64),
        );
    }

    if let Some(duration_secs) = duration_secs {
        event.insert(
            "duration_secs".to_string(),
            AttributeValue::Integer(duration_secs as i64),
        );
    }

    if let Some(threshold) = threshold {
        let mut threshold_fields = HashMap::new();
        if let Some(above) = threshold.above {
            threshold_fields.insert("above".to_string(), AttributeValue::Float(above));
        }
        if let Some(below) = threshold.below {
            threshold_fields.insert("below".to_string(), AttributeValue::Float(below));
        }
        event.insert(
            "threshold".to_string(),
            AttributeValue::Object(threshold_fields),
        );
    }

    if let Some(skipped) = recovered_skipped {
        event.insert("recovered".to_string(), AttributeValue::Bool(true));
        event.insert(
            "skipped_events".to_string(),
            AttributeValue::Integer(skipped as i64),
        );
    }

    AttributeValue::Object(event)
}

fn device_room_change_event(device_id: &str, room_id: Option<&str>) -> AttributeValue {
    let mut event = HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text("device_room_change".to_string()),
        ),
        (
            "device_id".to_string(),
            AttributeValue::Text(device_id.to_string()),
        ),
    ]);

    event.insert(
        "room_id".to_string(),
        room_id
            .map(|room_id| AttributeValue::Text(room_id.to_string()))
            .unwrap_or(AttributeValue::Null),
    );

    AttributeValue::Object(event)
}

fn room_change_event(room: &smart_home_core::model::Room, recovered: bool) -> AttributeValue {
    let mut event = HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text("room_change".to_string()),
        ),
        (
            "room_id".to_string(),
            AttributeValue::Text(room.id.0.clone()),
        ),
        (
            "name".to_string(),
            AttributeValue::Text(room.name.clone()),
        ),
    ]);

    if recovered {
        event.insert("recovered".to_string(), AttributeValue::Bool(true));
    }

    AttributeValue::Object(event)
}

fn room_removed_event(room_id: &str) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text("room_change".to_string()),
        ),
        (
            "room_id".to_string(),
            AttributeValue::Text(room_id.to_string()),
        ),
        (
            "removed".to_string(),
            AttributeValue::Bool(true),
        ),
    ]))
}

fn adapter_started_event(adapter: &str) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text("adapter_lifecycle".to_string()),
        ),
        (
            "adapter".to_string(),
            AttributeValue::Text(adapter.to_string()),
        ),
        (
            "event".to_string(),
            AttributeValue::Text("started".to_string()),
        ),
    ]))
}

fn system_error_event(message: &str) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text("system_error".to_string()),
        ),
        (
            "message".to_string(),
            AttributeValue::Text(message.to_string()),
        ),
    ]))
}

fn recovered_event(event: AttributeValue, skipped: u64) -> AttributeValue {
    let AttributeValue::Object(mut fields) = event else {
        return event;
    };
    fields.insert("recovered".to_string(), AttributeValue::Bool(true));
    fields.insert(
        "skipped_events".to_string(),
        AttributeValue::Integer(skipped as i64),
    );
    AttributeValue::Object(fields)
}

fn load_automation_file(path: &Path, scripts_root: Option<&Path>) -> Result<Automation> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read automation file {}", path.display()))?;
    let lua = Lua::new();
    let module = evaluate_automation_module(&lua, &source, path, scripts_root)?;

    let id = module.get::<String>("id").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} is missing string field 'id': {error}",
            path.display()
        )
    })?;
    let name = module.get::<String>("name").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} is missing string field 'name': {error}",
            path.display()
        )
    })?;
    let trigger_value = module.get::<mlua::Value>("trigger").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} is missing field 'trigger': {error}",
            path.display()
        )
    })?;
    let trigger = parse_trigger(trigger_value, path)?;

    if id.trim().is_empty() {
        bail!("automation file {} has empty id", path.display());
    }
    if name.trim().is_empty() {
        bail!("automation file {} has empty name", path.display());
    }

    let _: Function = module.get("execute").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} is missing function field 'execute': {error}",
            path.display()
        )
    })?;

    let description = module
        .get::<Option<String>>("description")
        .map_err(|error| {
            anyhow::anyhow!(
                "automation file {} has invalid optional field 'description': {error}",
                path.display()
            )
        })?;
    let runtime_state_policy = parse_runtime_state_policy(&module, path)?;

    Ok(Automation {
        summary: AutomationSummary {
            id,
            name,
            description,
            trigger_type: trigger_type_name(&trigger),
        },
        path: path.to_path_buf(),
        trigger,
        runtime_state_policy,
    })
}

fn evaluate_automation_module(
    lua: &Lua,
    source: &str,
    path: &Path,
    scripts_root: Option<&Path>,
) -> Result<mlua::Table> {
    evaluate_module(
        lua,
        source,
        path.to_string_lossy().as_ref(),
        &LuaRuntimeOptions {
            scripts_root: scripts_root.map(Path::to_path_buf),
        },
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "failed to evaluate automation file {}: {error}",
            path.display()
        )
    })
}

fn parse_trigger(value: mlua::Value, path: &Path) -> Result<Trigger> {
    let mlua::Value::Table(table) = value else {
        bail!(
            "automation file {} field 'trigger' must be a table",
            path.display()
        );
    };

    let trigger_type = table.get::<String>("type").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} trigger is missing string field 'type': {error}",
            path.display()
        )
    })?;

    match trigger_type.as_str() {
        "device_state_change" => {
            let device_id = table.get::<String>("device_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} device_state_change trigger requires 'device_id': {error}",
                    path.display()
                )
            })?;
            let attribute = table.get::<Option<String>>("attribute").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'attribute' is invalid: {error}",
                    path.display()
                )
            })?;
            let equals = match table.get::<mlua::Value>("equals") {
                Ok(mlua::Value::Nil) => None,
                Ok(value) => Some(
                    smart_home_lua_host::lua_value_to_attribute(value)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?,
                ),
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "automation file {} trigger field 'equals' is invalid: {error}",
                        path.display()
                    ))
                }
            };
            let threshold = parse_threshold_trigger(&table, path, "device_state_change")?;
            let debounce_secs = table.get::<Option<u64>>("debounce_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'debounce_secs' is invalid: {error}",
                    path.display()
                )
            })?;
            let duration_secs = table.get::<Option<u64>>("duration_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'duration_secs' is invalid: {error}",
                    path.display()
                )
            })?;

            validate_extended_device_trigger(
                path,
                "device_state_change",
                attribute.as_deref(),
                equals.as_ref(),
                threshold.as_ref(),
                debounce_secs,
                duration_secs,
            )?;

            Ok(Trigger::DeviceStateChange {
                device_id,
                attribute,
                equals,
                threshold,
                debounce_secs,
                duration_secs,
            })
        }
        "weather_state" => {
            let device_id = table.get::<String>("device_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} weather_state trigger requires 'device_id': {error}",
                    path.display()
                )
            })?;
            let attribute = table.get::<String>("attribute").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} weather_state trigger requires string 'attribute': {error}",
                    path.display()
                )
            })?;
            let equals = match table.get::<mlua::Value>("equals") {
                Ok(mlua::Value::Nil) => None,
                Ok(value) => Some(
                    smart_home_lua_host::lua_value_to_attribute(value)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?,
                ),
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "automation file {} trigger field 'equals' is invalid: {error}",
                        path.display()
                    ))
                }
            };
            let threshold = parse_threshold_trigger(&table, path, "weather_state")?;
            let debounce_secs = table.get::<Option<u64>>("debounce_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'debounce_secs' is invalid: {error}",
                    path.display()
                )
            })?;
            let duration_secs = table.get::<Option<u64>>("duration_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'duration_secs' is invalid: {error}",
                    path.display()
                )
            })?;

            validate_extended_device_trigger(
                path,
                "weather_state",
                Some(attribute.as_str()),
                equals.as_ref(),
                threshold.as_ref(),
                debounce_secs,
                duration_secs,
            )?;

            Ok(Trigger::WeatherState {
                device_id,
                attribute,
                equals,
                threshold,
                debounce_secs,
                duration_secs,
            })
        }
        "device_room_change" => {
            let device_id = table.get::<Option<String>>("device_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'device_id' is invalid: {error}",
                    path.display()
                )
            })?;
            let room_id = table.get::<Option<String>>("room_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'room_id' is invalid: {error}",
                    path.display()
                )
            })?;

            if device_id.is_none() && room_id.is_none() {
                bail!(
                    "automation file {} device_room_change trigger requires 'device_id' or 'room_id'",
                    path.display()
                );
            }

            Ok(Trigger::DeviceRoomChange { device_id, room_id })
        }
        "room_change" => {
            let room_id = table.get::<Option<String>>("room_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'room_id' is invalid: {error}",
                    path.display()
                )
            })?;

            Ok(Trigger::RoomChange { room_id })
        }
        "adapter_lifecycle" => {
            let adapter = table.get::<Option<String>>("adapter").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'adapter' is invalid: {error}",
                    path.display()
                )
            })?;
            let event = match table.get::<String>("event").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} adapter_lifecycle trigger requires string 'event': {error}",
                    path.display()
                )
            })?.as_str() {
                "started" => AdapterLifecycleEvent::Started,
                other => {
                    bail!(
                        "automation file {} adapter_lifecycle trigger has unsupported event '{}'; supported events are started",
                        path.display(),
                        other
                    )
                }
            };

            Ok(Trigger::AdapterLifecycle { adapter, event })
        }
        "system_error" => {
            let contains = table.get::<Option<String>>("contains").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'contains' is invalid: {error}",
                    path.display()
                )
            })?;

            Ok(Trigger::SystemError { contains })
        }
        "wall_clock" => {
            let hour = table.get::<u32>("hour").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} wall_clock trigger requires integer 'hour': {error}",
                    path.display()
                )
            })?;
            let minute = table.get::<u32>("minute").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} wall_clock trigger requires integer 'minute': {error}",
                    path.display()
                )
            })?;

            if hour > 23 {
                bail!(
                    "automation file {} wall_clock trigger 'hour' must be between 0 and 23",
                    path.display()
                );
            }
            if minute > 59 {
                bail!(
                    "automation file {} wall_clock trigger 'minute' must be between 0 and 59",
                    path.display()
                );
            }

            Ok(Trigger::WallClock { hour, minute })
        }
        "cron" => {
            let expression = table.get::<String>("expression").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} cron trigger requires string 'expression': {error}",
                    path.display()
                )
            })?;
            let schedule = Schedule::from_str(&expression).map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} cron trigger has invalid 'expression': {error}",
                    path.display()
                )
            })?;

            if schedule.after(&Utc::now()).next().is_none() {
                bail!(
                    "automation file {} cron trigger expression does not produce future occurrences",
                    path.display()
                );
            }

            Ok(Trigger::Cron {
                expression,
                schedule,
            })
        }
        "sunrise" => {
            let offset_mins = table.get::<Option<i64>>("offset_mins").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} sunrise trigger field 'offset_mins' is invalid: {error}",
                    path.display()
                )
            })?
            .unwrap_or(0);

            Ok(Trigger::Sunrise { offset_mins })
        }
        "sunset" => {
            let offset_mins = table.get::<Option<i64>>("offset_mins").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} sunset trigger field 'offset_mins' is invalid: {error}",
                    path.display()
                )
            })?
            .unwrap_or(0);

            Ok(Trigger::Sunset { offset_mins })
        }
        "interval" => {
            let every_secs = table.get::<u64>("every_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} interval trigger requires positive 'every_secs': {error}",
                    path.display()
                )
            })?;
            if every_secs == 0 {
                bail!(
                    "automation file {} interval trigger 'every_secs' must be > 0",
                    path.display()
                );
            }

            Ok(Trigger::Interval { every_secs })
        }
        _ => bail!(
            "automation file {} has unsupported trigger type '{}'; supported types are device_state_change, weather_state, device_room_change, room_change, adapter_lifecycle, system_error, wall_clock, cron, sunrise, sunset, and interval",
            path.display(),
            trigger_type
        ),
    }
}

fn parse_threshold_trigger(
    table: &mlua::Table,
    path: &Path,
    trigger_type: &str,
) -> Result<Option<ThresholdTrigger>> {
    let above = table.get::<Option<f64>>("above").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} {} trigger field 'above' is invalid: {error}",
            path.display(),
            trigger_type
        )
    })?;
    let below = table.get::<Option<f64>>("below").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} {} trigger field 'below' is invalid: {error}",
            path.display(),
            trigger_type
        )
    })?;

    if above.is_none() && below.is_none() {
        return Ok(None);
    }

    Ok(Some(ThresholdTrigger { above, below }))
}

fn validate_extended_device_trigger(
    path: &Path,
    trigger_type: &str,
    attribute: Option<&str>,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdTrigger>,
    debounce_secs: Option<u64>,
    duration_secs: Option<u64>,
) -> Result<()> {
    if threshold.is_some() && equals.is_some() {
        bail!(
            "automation file {} {} trigger cannot combine 'equals' with 'above'/'below'",
            path.display(),
            trigger_type
        );
    }

    if threshold.is_none() && equals.is_none() && attribute.is_some() && debounce_secs.is_none() && duration_secs.is_none() {
        return Ok(());
    }

    if (threshold.is_some() || debounce_secs.is_some() || duration_secs.is_some()) && attribute.is_none() {
        bail!(
            "automation file {} {} trigger requires 'attribute' when using threshold, debounce, or duration options",
            path.display(),
            trigger_type
        );
    }

    if debounce_secs == Some(0) {
        bail!(
            "automation file {} {} trigger field 'debounce_secs' must be > 0",
            path.display(),
            trigger_type
        );
    }

    if duration_secs == Some(0) {
        bail!(
            "automation file {} {} trigger field 'duration_secs' must be > 0",
            path.display(),
            trigger_type
        );
    }

    Ok(())
}

fn trigger_type_name(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::DeviceStateChange { .. } => "device_state_change",
        Trigger::WeatherState { .. } => "weather_state",
        Trigger::DeviceRoomChange { .. } => "device_room_change",
        Trigger::RoomChange { .. } => "room_change",
        Trigger::AdapterLifecycle { .. } => "adapter_lifecycle",
        Trigger::SystemError { .. } => "system_error",
        Trigger::WallClock { .. } => "wall_clock",
        Trigger::Cron { .. } => "cron",
        Trigger::Sunrise { .. } => "sunrise",
        Trigger::Sunset { .. } => "sunset",
        Trigger::Interval { .. } => "interval",
    }
}

fn trigger_uses_event_bus(automation: &Automation) -> bool {
    matches!(
        automation.trigger,
        Trigger::DeviceStateChange { .. }
            | Trigger::WeatherState { .. }
            | Trigger::DeviceRoomChange { .. }
            | Trigger::RoomChange { .. }
            | Trigger::AdapterLifecycle { .. }
            | Trigger::SystemError { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::Mutex;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use chrono::{Datelike, NaiveDate, TimeZone, Timelike};
    use cron::Schedule;
    use smart_home_core::adapter::Adapter;
    use smart_home_core::bus::EventBus;
    use smart_home_core::command::DeviceCommand;
    use smart_home_core::model::{AttributeValue, Device, DeviceId, DeviceKind, Metadata, Room, RoomId};
    use smart_home_core::registry::DeviceRegistry;
    use smart_home_core::runtime::{Runtime, RuntimeConfig};
    use smart_home_core::store::AutomationExecutionHistoryEntry;
    use tokio::sync::Semaphore;
    use tokio::time::{sleep, timeout, Duration};

    use super::*;

    const RAIN_ATTRIBUTE: &str = "custom.test.rain";

    struct CommandAdapter;

    #[derive(Default)]
    struct RecordingObserver {
        entries: Mutex<Vec<AutomationExecutionHistoryEntry>>,
    }

    #[derive(Default)]
    struct MemoryStateStore {
        automation_state: Mutex<HashMap<String, AutomationRuntimeState>>,
    }

    impl AutomationExecutionObserver for RecordingObserver {
        fn record(&self, entry: AutomationExecutionHistoryEntry) {
            self.entries.lock().expect("observer lock").push(entry);
        }
    }

    #[async_trait::async_trait]
    impl DeviceStore for MemoryStateStore {
        async fn load_all_devices(&self) -> anyhow::Result<Vec<Device>> { Ok(Vec::new()) }
        async fn load_all_rooms(&self) -> anyhow::Result<Vec<Room>> { Ok(Vec::new()) }
        async fn save_device(&self, _device: &Device) -> anyhow::Result<()> { Ok(()) }
        async fn save_room(&self, _room: &Room) -> anyhow::Result<()> { Ok(()) }
        async fn delete_device(&self, _id: &DeviceId) -> anyhow::Result<()> { Ok(()) }
        async fn delete_room(&self, _id: &RoomId) -> anyhow::Result<()> { Ok(()) }
        async fn load_device_history(&self, _id: &DeviceId, _start: Option<DateTime<Utc>>, _end: Option<DateTime<Utc>>, _limit: usize) -> anyhow::Result<Vec<smart_home_core::store::DeviceHistoryEntry>> { Ok(Vec::new()) }
        async fn load_attribute_history(&self, _id: &DeviceId, _attribute: &str, _start: Option<DateTime<Utc>>, _end: Option<DateTime<Utc>>, _limit: usize) -> anyhow::Result<Vec<smart_home_core::store::AttributeHistoryEntry>> { Ok(Vec::new()) }
        async fn save_command_audit(&self, _entry: &smart_home_core::store::CommandAuditEntry) -> anyhow::Result<()> { Ok(()) }
        async fn load_command_audit(&self, _device_id: Option<&DeviceId>, _start: Option<DateTime<Utc>>, _end: Option<DateTime<Utc>>, _limit: usize) -> anyhow::Result<Vec<smart_home_core::store::CommandAuditEntry>> { Ok(Vec::new()) }
        async fn save_scene_execution(&self, _entry: &smart_home_core::store::SceneExecutionHistoryEntry) -> anyhow::Result<()> { Ok(()) }
        async fn load_scene_history(&self, _scene_id: &str, _start: Option<DateTime<Utc>>, _end: Option<DateTime<Utc>>, _limit: usize) -> anyhow::Result<Vec<smart_home_core::store::SceneExecutionHistoryEntry>> { Ok(Vec::new()) }
        async fn save_automation_execution(&self, _entry: &AutomationExecutionHistoryEntry) -> anyhow::Result<()> { Ok(()) }
        async fn load_automation_history(&self, _automation_id: &str, _start: Option<DateTime<Utc>>, _end: Option<DateTime<Utc>>, _limit: usize) -> anyhow::Result<Vec<AutomationExecutionHistoryEntry>> { Ok(Vec::new()) }
        async fn load_automation_runtime_state(&self, automation_id: &str) -> anyhow::Result<Option<AutomationRuntimeState>> {
            Ok(self
                .automation_state
                .lock()
                .expect("automation state lock")
                .get(automation_id)
                .cloned())
        }
        async fn save_automation_runtime_state(&self, state: &AutomationRuntimeState) -> anyhow::Result<()> {
            self.automation_state
                .lock()
                .expect("automation state lock")
                .insert(state.automation_id.clone(), state.clone());
            Ok(())
        }
    }

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
            if !device_id.0.starts_with("test:") {
                return Ok(false);
            }

            let Some(mut device) = registry.get(device_id) else {
                return Ok(false);
            };
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
        fs::create_dir_all(&path).expect("create temp automations dir");
        path
    }

    fn write_automation(dir: &Path, name: &str, source: &str) {
        fs::write(dir.join(name), source).expect("write automation file");
    }

    fn sample_device(id: &str, wet: bool) -> Device {
        Device {
            id: DeviceId(id.to_string()),
            room_id: None,
            kind: DeviceKind::Sensor,
            attributes: HashMap::from([(RAIN_ATTRIBUTE.to_string(), AttributeValue::Bool(wet))]),
            metadata: Metadata {
                source: "test".to_string(),
                accuracy: None,
                vendor_specific: HashMap::new(),
            },
            updated_at: Utc::now(),
            last_seen: Utc::now(),
        }
    }

    fn numeric_device(id: &str, attribute: &str, value: f64) -> Device {
        Device {
            id: DeviceId(id.to_string()),
            room_id: None,
            kind: DeviceKind::Sensor,
            attributes: HashMap::from([(
                attribute.to_string(),
                AttributeValue::Object(HashMap::from([
                    ("value".to_string(), AttributeValue::Float(value)),
                    ("unit".to_string(), AttributeValue::Text("celsius".to_string())),
                ])),
            )]),
            metadata: Metadata {
                source: "test".to_string(),
                accuracy: None,
                vendor_specific: HashMap::new(),
            },
            updated_at: Utc::now(),
            last_seen: Utc::now(),
        }
    }

    fn target_device(id: &str) -> Device {
        Device {
            id: DeviceId(id.to_string()),
            room_id: None,
            kind: DeviceKind::Light,
            attributes: HashMap::from([("brightness".to_string(), AttributeValue::Integer(0))]),
            metadata: Metadata {
                source: "test".to_string(),
                accuracy: None,
                vendor_specific: HashMap::new(),
            },
            updated_at: Utc::now(),
            last_seen: Utc::now(),
        }
    }

    fn sample_room(id: &str, name: &str) -> Room {
        Room {
            id: RoomId(id.to_string()),
            name: name.to_string(),
        }
    }

    #[test]
    fn loads_device_trigger_automation_catalog() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "rain.lua",
            r#"return {
                id = "rain_check",
                name = "Rain Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                end
            }"#,
        );

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        assert_eq!(catalog.summaries().len(), 1);
        assert_eq!(catalog.summaries()[0].trigger_type, "device_state_change");
    }

    #[test]
    fn loads_extended_event_trigger_automation_catalog() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "room.lua",
            r#"return {
                id = "room_watch",
                name = "Room Watch",
                trigger = {
                    type = "room_change",
                    room_id = "outside",
                },
                execute = function(ctx, event)
                end
            }"#,
        );
        write_automation(
            &dir,
            "adapter.lua",
            r#"return {
                id = "adapter_watch",
                name = "Adapter Watch",
                trigger = {
                    type = "adapter_lifecycle",
                    adapter = "test",
                    event = "started",
                },
                execute = function(ctx, event)
                end
            }"#,
        );

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let trigger_types = catalog
            .summaries()
            .into_iter()
            .map(|summary| summary.trigger_type)
            .collect::<Vec<_>>();
        assert!(trigger_types.contains(&"room_change"));
        assert!(trigger_types.contains(&"adapter_lifecycle"));
    }

    #[test]
    fn loads_wall_clock_and_cron_trigger_catalog() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "clock.lua",
            r#"return {
                id = "clock_watch",
                name = "Clock Watch",
                trigger = {
                    type = "wall_clock",
                    hour = 6,
                    minute = 30,
                },
                execute = function(ctx, event)
                end
            }"#,
        );
        write_automation(
            &dir,
            "cron.lua",
            r#"return {
                id = "cron_watch",
                name = "Cron Watch",
                trigger = {
                    type = "cron",
                    expression = "0 */5 * * * * *",
                },
                execute = function(ctx, event)
                end
            }"#,
        );

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let trigger_types = catalog
            .summaries()
            .into_iter()
            .map(|summary| summary.trigger_type)
            .collect::<Vec<_>>();
        assert!(trigger_types.contains(&"wall_clock"));
        assert!(trigger_types.contains(&"cron"));
    }

    #[test]
    fn loads_sunrise_and_sunset_trigger_catalog() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "sunrise.lua",
            r#"return {
                id = "sunrise_watch",
                name = "Sunrise Watch",
                trigger = {
                    type = "sunrise",
                    offset_mins = -15,
                },
                execute = function(ctx, event)
                end
            }"#,
        );
        write_automation(
            &dir,
            "sunset.lua",
            r#"return {
                id = "sunset_watch",
                name = "Sunset Watch",
                trigger = {
                    type = "sunset",
                    offset_mins = 20,
                },
                execute = function(ctx, event)
                end
            }"#,
        );

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let trigger_types = catalog
            .summaries()
            .into_iter()
            .map(|summary| summary.trigger_type)
            .collect::<Vec<_>>();
        assert!(trigger_types.contains(&"sunrise"));
        assert!(trigger_types.contains(&"sunset"));
    }

    #[test]
    fn loads_weather_and_threshold_trigger_catalog() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "weather.lua",
            r#"return {
                id = "weather_watch",
                name = "Weather Watch",
                trigger = {
                    type = "weather_state",
                    device_id = "weather:outside",
                    attribute = "temperature_outdoor",
                    above = 25.0,
                    debounce_secs = 60,
                },
                execute = function(ctx, event)
                end
            }"#,
        );

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        assert_eq!(catalog.summaries()[0].trigger_type, "weather_state");
    }

    #[test]
    fn rejects_threshold_without_attribute() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "bad.lua",
            r#"return {
                id = "bad_trigger",
                name = "Bad Trigger",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:sensor",
                    above = 25.0,
                },
                execute = function(ctx, event)
                end
            }"#,
        );

        let error = AutomationCatalog::load_from_directory(&dir, None).expect_err("catalog should fail");
        assert!(error.to_string().contains("requires 'attribute'"));
    }

    #[test]
    fn next_wall_clock_occurrence_rolls_to_next_day_after_past_time() {
        let after = Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 4, 16)
                .expect("valid date")
                .and_hms_opt(6, 31, 0)
                .expect("valid time"),
        );

        let next = next_wall_clock_occurrence(6, 30, after).expect("next schedule exists");
        assert_eq!(next.day(), 17);
        assert_eq!(next.hour(), 6);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn next_schedule_time_uses_cron_expression() {
        let trigger = Trigger::Cron {
            expression: "0 */10 * * * * *".to_string(),
            schedule: Schedule::from_str("0 */10 * * * * *").expect("cron parses"),
        };
        let after = Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 4, 16)
                .expect("valid date")
                .and_hms_opt(8, 7, 0)
                .expect("valid time"),
        );

        let next = next_schedule_time(&trigger, after, TriggerContext::default()).expect("next schedule exists");
        assert_eq!(next.hour(), 8);
        assert_eq!(next.minute(), 10);
        assert_eq!(next.second(), 0);
    }

    #[test]
    fn next_schedule_time_uses_sunrise_and_sunset_with_context() {
        let after = Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 6, 21)
                .expect("valid date")
                .and_hms_opt(0, 0, 0)
                .expect("valid time"),
        );
        let context = TriggerContext {
            latitude: Some(51.5),
            longitude: Some(-0.1),
        };

        let sunrise = next_schedule_time(&Trigger::Sunrise { offset_mins: 0 }, after, context)
            .expect("sunrise exists");
        let sunset = next_schedule_time(&Trigger::Sunset { offset_mins: 0 }, after, context)
            .expect("sunset exists");

        assert_eq!(sunrise.date_naive(), after.date_naive());
        assert_eq!(sunset.date_naive(), after.date_naive());
        assert!(sunrise < sunset);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn event_automation_executes_on_matching_state_change() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "rain.lua",
            r#"return {
                id = "rain_check",
                name = "Rain Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 42,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device("test:rain", false))
            .await
            .expect("sensor upsert succeeds");
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device upsert succeeds");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let runtime_for_runner = runtime.clone();
        let task = tokio::spawn(async move {
            runner.run(runtime_for_runner).await;
        });

        sleep(Duration::from_millis(25)).await;

        runtime
            .registry()
            .upsert(sample_device("test:rain", true))
            .await
            .expect("sensor change succeeds");

        sleep(Duration::from_millis(100)).await;

        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(42))
        );

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn threshold_trigger_only_executes_on_crossing() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "threshold.lua",
            r#"return {
                id = "threshold_check",
                name = "Threshold Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:weather",
                    attribute = "temperature_outdoor",
                    above = 25.0,
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 90,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig { event_bus_capacity: 32 },
        ));
        runtime
            .registry()
            .upsert(numeric_device("test:weather", "temperature_outdoor", 24.0))
            .await
            .expect("sensor upsert succeeds");
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device upsert succeeds");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runner.run(runtime).await }
        });

        sleep(Duration::from_millis(25)).await;
        runtime
            .registry()
            .upsert(numeric_device("test:weather", "temperature_outdoor", 24.5))
            .await
            .expect("below-threshold change succeeds");
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(0))
        );

        runtime
            .registry()
            .upsert(numeric_device("test:weather", "temperature_outdoor", 26.0))
            .await
            .expect("crossing change succeeds");

        timeout(Duration::from_secs(2), async {
            loop {
                if runtime
                    .registry()
                    .get(&DeviceId("test:device".to_string()))
                    .expect("target device exists")
                    .attributes
                    .get("brightness")
                    == Some(&AttributeValue::Integer(90))
                {
                    break;
                }

                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("threshold crossing executes automation");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn debounce_trigger_waits_for_stable_state() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "debounce.lua",
            r#"return {
                id = "debounce_trigger",
                name = "Debounce Trigger",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                    debounce_secs = 1,
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 33,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig { event_bus_capacity: 32 },
        ));
        runtime.registry().upsert(sample_device("test:rain", false)).await.expect("sensor upsert succeeds");
        runtime.registry().upsert(target_device("test:device")).await.expect("target exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runner.run(runtime).await }
        });

        sleep(Duration::from_millis(25)).await;
        runtime.registry().upsert(sample_device("test:rain", true)).await.expect("trigger on");
        sleep(Duration::from_millis(300)).await;
        runtime.registry().upsert(sample_device("test:rain", false)).await.expect("trigger reset");
        sleep(Duration::from_millis(1000)).await;

        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(0))
        );

        runtime.registry().upsert(sample_device("test:rain", true)).await.expect("trigger on again");
        timeout(Duration::from_secs(3), async {
            loop {
                if runtime
                    .registry()
                    .get(&DeviceId("test:device".to_string()))
                    .expect("target device exists")
                    .attributes
                    .get("brightness")
                    == Some(&AttributeValue::Integer(33))
                {
                    break;
                }

                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stable debounce executes automation");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duration_trigger_requires_condition_to_hold() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "duration.lua",
            r#"return {
                id = "duration_trigger",
                name = "Duration Trigger",
                trigger = {
                    type = "weather_state",
                    device_id = "test:weather",
                    attribute = "temperature_outdoor",
                    above = 25.0,
                    duration_secs = 1,
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 66,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig { event_bus_capacity: 32 },
        ));
        runtime
            .registry()
            .upsert(numeric_device("test:weather", "temperature_outdoor", 24.0))
            .await
            .expect("sensor upsert succeeds");
        runtime.registry().upsert(target_device("test:device")).await.expect("target exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runner.run(runtime).await }
        });

        sleep(Duration::from_millis(25)).await;
        runtime
            .registry()
            .upsert(numeric_device("test:weather", "temperature_outdoor", 26.0))
            .await
            .expect("high temperature update succeeds");
        sleep(Duration::from_millis(1200)).await;

        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(66))
        );

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn automation_cooldown_skips_repeated_triggers_within_window() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "cooldown.lua",
            r#"return {
                id = "cooldown_check",
                name = "Cooldown Check",
                state = {
                    cooldown_secs = 60,
                },
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 42,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig { event_bus_capacity: 32 },
        ));
        let state_store = Arc::new(MemoryStateStore::default());
        runtime.registry().upsert(sample_device("test:rain", false)).await.expect("sensor upsert succeeds");
        runtime.registry().upsert(target_device("test:device")).await.expect("target exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog)
            .with_state_store(state_store.clone());
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runner.run(runtime).await }
        });

        sleep(Duration::from_millis(25)).await;
        runtime.registry().upsert(sample_device("test:rain", true)).await.expect("first trigger");
        sleep(Duration::from_millis(100)).await;
        let first_state = state_store
            .load_automation_runtime_state("cooldown_check")
            .await
            .expect("load state succeeds")
            .expect("state exists");

        runtime.registry().upsert(sample_device("test:rain", false)).await.expect("reset trigger");
        runtime.registry().upsert(sample_device("test:rain", true)).await.expect("second trigger");
        sleep(Duration::from_millis(100)).await;

        let second_state = state_store
            .load_automation_runtime_state("cooldown_check")
            .await
            .expect("load state succeeds")
            .expect("state exists");
        assert_eq!(second_state.last_triggered_at, first_state.last_triggered_at);

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn automation_dedupe_window_skips_identical_trigger_payloads() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "dedupe.lua",
            r#"return {
                id = "dedupe_check",
                name = "Dedupe Check",
                state = {
                    dedupe_window_secs = 60,
                },
                trigger = {
                    type = "system_error",
                    contains = "boom",
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 7,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig { event_bus_capacity: 32 },
        ));
        let state_store = Arc::new(MemoryStateStore::default());
        runtime.registry().upsert(target_device("test:device")).await.expect("target exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog)
            .with_state_store(state_store.clone());
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runner.run(runtime).await }
        });

        sleep(Duration::from_millis(25)).await;
        runtime.bus().publish(Event::SystemError { message: "boom".to_string() });
        sleep(Duration::from_millis(100)).await;
        let first_state = state_store
            .load_automation_runtime_state("dedupe_check")
            .await
            .expect("load state succeeds")
            .expect("state exists");

        runtime.bus().publish(Event::SystemError { message: "boom".to_string() });
        sleep(Duration::from_millis(100)).await;
        let second_state = state_store
            .load_automation_runtime_state("dedupe_check")
            .await
            .expect("load state succeeds")
            .expect("state exists");
        assert_eq!(second_state.last_triggered_at, first_state.last_triggered_at);
        assert_eq!(second_state.last_trigger_fingerprint, first_state.last_trigger_fingerprint);

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn resumable_schedule_uses_persisted_last_scheduled_time() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "resume.lua",
            r#"return {
                id = "resume_check",
                name = "Resume Check",
                state = {
                    resumable_schedule = true,
                },
                trigger = {
                    type = "wall_clock",
                    hour = 6,
                    minute = 30,
                },
                execute = function(ctx, event)
                end
            }"#,
        );
        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let automation = catalog.automations.first().expect("automation exists");
        let state_store = AutomationStateStore {
            store: Arc::new(MemoryStateStore {
                automation_state: Mutex::new(HashMap::from([(
                    "resume_check".to_string(),
                    AutomationRuntimeState {
                        updated_at: Utc::now(),
                        automation_id: "resume_check".to_string(),
                        last_triggered_at: None,
                        last_trigger_fingerprint: None,
                        last_scheduled_at: Some(Utc.from_utc_datetime(
                            &NaiveDate::from_ymd_opt(2026, 4, 16)
                                .expect("valid date")
                                .and_hms_opt(6, 30, 0)
                                .expect("valid time"),
                        )),
                    },
                )])),
            }),
        };
        let now = Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 4, 16)
                .expect("valid date")
                .and_hms_opt(7, 0, 0)
                .expect("valid time"),
        );

        let next = next_scheduled_fire_after(automation, Some(state_store), now, TriggerContext::default())
            .await
            .flatten()
            .expect("next resumed schedule exists");
        assert_eq!(next.day(), 17);
        assert_eq!(next.hour(), 6);
        assert_eq!(next.minute(), 30);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wall_clock_automation_executes_on_schedule() {
        let dir = temp_dir("smart-home-automations");
        let scheduled_for = Utc::now() + ChronoDuration::minutes(1);
        write_automation(
            &dir,
            "clock.lua",
            &format!(
                r#"return {{
                    id = "clock_watch",
                    name = "Clock Watch",
                    trigger = {{
                        type = "wall_clock",
                        hour = {},
                        minute = {},
                    }},
                    execute = function(ctx, event)
                        if event.type == "wall_clock" then
                            ctx:command("test:device", {{
                                capability = "brightness",
                                action = "set",
                                value = event.minute,
                            }})
                        end
                    end
                }}"#,
                scheduled_for.hour(),
                scheduled_for.minute(),
            ),
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runner.run(runtime).await;
            }
        });

        timeout(Duration::from_secs(90), async {
            loop {
                if runtime
                    .registry()
                    .get(&DeviceId("test:device".to_string()))
                    .expect("target device exists")
                    .attributes
                    .get("brightness")
                    == Some(&AttributeValue::Integer(scheduled_for.minute() as i64))
                {
                    break;
                }

                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("wall clock automation executes");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn device_room_change_automation_executes_on_room_assignment() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "room.lua",
            r#"return {
                id = "room_assignment",
                name = "Room Assignment",
                trigger = {
                    type = "device_room_change",
                    device_id = "test:roomed",
                    room_id = "outside",
                },
                execute = function(ctx, event)
                    if event.room_id == "outside" then
                        ctx:command("test:device", {
                            capability = "brightness",
                            action = "set",
                            value = 61,
                        })
                    end
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert_room(sample_room("outside", "Outside"))
            .await;
        runtime
            .registry()
            .upsert(sample_device("test:roomed", false))
            .await
            .expect("roomed device exists");
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runner.run(runtime).await;
            }
        });

        sleep(Duration::from_millis(25)).await;

        runtime
            .registry()
            .assign_device_to_room(
                &DeviceId("test:roomed".to_string()),
                Some(RoomId("outside".to_string())),
            )
            .await
            .expect("room assignment succeeds");

        timeout(Duration::from_secs(2), async {
            loop {
                if runtime
                    .registry()
                    .get(&DeviceId("test:device".to_string()))
                    .expect("target device exists")
                    .attributes
                    .get("brightness")
                    == Some(&AttributeValue::Integer(61))
                {
                    break;
                }

                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("device room change automation executes");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adapter_lifecycle_automation_executes_on_started_event() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "adapter.lua",
            r#"return {
                id = "adapter_started",
                name = "Adapter Started",
                trigger = {
                    type = "adapter_lifecycle",
                    adapter = "test",
                    event = "started",
                },
                execute = function(ctx, event)
                    if event.adapter == "test" and event.event == "started" then
                        ctx:command("test:device", {
                            capability = "brightness",
                            action = "set",
                            value = 15,
                        })
                    end
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runner.run(runtime).await;
            }
        });

        sleep(Duration::from_millis(25)).await;
        runtime.bus().publish(Event::AdapterStarted {
            adapter: "test".to_string(),
        });

        timeout(Duration::from_secs(2), async {
            loop {
                if runtime
                    .registry()
                    .get(&DeviceId("test:device".to_string()))
                    .expect("target device exists")
                    .attributes
                    .get("brightness")
                    == Some(&AttributeValue::Integer(15))
                {
                    break;
                }

                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("adapter lifecycle automation executes");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn system_error_automation_executes_on_matching_error() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "error.lua",
            r#"return {
                id = "error_watch",
                name = "Error Watch",
                trigger = {
                    type = "system_error",
                    contains = "adapter 'failing'",
                },
                execute = function(ctx, event)
                    if string.find(event.message, "failing", 1, true) then
                        ctx:command("test:device", {
                            capability = "brightness",
                            action = "set",
                            value = 88,
                        })
                    end
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runner.run(runtime).await;
            }
        });

        sleep(Duration::from_millis(25)).await;
        runtime.bus().publish(Event::SystemError {
            message: "adapter 'failing' failed: boom".to_string(),
        });

        timeout(Duration::from_secs(2), async {
            loop {
                if runtime
                    .registry()
                    .get(&DeviceId("test:device".to_string()))
                    .expect("target device exists")
                    .attributes
                    .get("brightness")
                    == Some(&AttributeValue::Integer(88))
                {
                    break;
                }

                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("system error automation executes");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn automation_can_require_script_module() {
        let dir = temp_dir("smart-home-automations");
        let scripts_dir = temp_dir("smart-home-scripts");
        fs::create_dir_all(scripts_dir.join("lighting")).expect("create script namespace dir");
        fs::write(
            scripts_dir.join("lighting/helpers.lua"),
            r#"local M = {}

            function M.level_from_event(event)
                if event.value == true then
                    return 55
                end

                return 10
            end

            return M"#,
        )
        .expect("write helper script");
        write_automation(
            &dir,
            "rain.lua",
            r#"local helpers = require("lighting.helpers")

            return {
                id = "rain_check",
                name = "Rain Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = helpers.level_from_event(event),
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device("test:rain", false))
            .await
            .expect("sensor upsert succeeds");
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device upsert succeeds");

        let catalog =
            AutomationCatalog::load_from_directory(&dir, Some(scripts_dir)).expect("catalog loads");
        let automation = catalog
            .automations
            .first()
            .expect("script-backed automation exists");

        execute_automation(
            automation,
            runtime.clone(),
            AttributeValue::Object(HashMap::from([(
                "value".to_string(),
                AttributeValue::Bool(true),
            )])),
            catalog.scripts_root.as_deref(),
            Duration::from_secs(2),
        )
        .expect("script-backed automation executes successfully");

        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(55))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn event_automation_executes_without_blocking_other_matching_automations() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "slow.lua",
            r#"return {
                id = "slow_check",
                name = "Slow Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    local started = os.clock()
                    while os.clock() - started < 0.25 do
                    end
                    ctx:command("test:slow", {
                        capability = "brightness",
                        action = "set",
                        value = 11,
                    })
                end
            }"#,
        );
        write_automation(
            &dir,
            "fast.lua",
            r#"return {
                id = "fast_check",
                name = "Fast Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    ctx:command("test:fast", {
                        capability = "brightness",
                        action = "set",
                        value = 99,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device("test:rain", false))
            .await
            .expect("sensor upsert succeeds");
        runtime
            .registry()
            .upsert(target_device("test:slow"))
            .await
            .expect("slow target device exists");
        runtime
            .registry()
            .upsert(target_device("test:fast"))
            .await
            .expect("fast target device exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runner.run(runtime).await;
            }
        });

        sleep(Duration::from_millis(25)).await;

        runtime
            .registry()
            .upsert(sample_device("test:rain", true))
            .await
            .expect("sensor change succeeds");

        timeout(Duration::from_millis(150), async {
            loop {
                if runtime
                    .registry()
                    .get(&DeviceId("test:fast".to_string()))
                    .expect("fast target device exists")
                    .attributes
                    .get("brightness")
                    == Some(&AttributeValue::Integer(99))
                {
                    break;
                }

                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fast automation should complete before slow automation finishes");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn saturated_event_runner_skips_new_automation_runs() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "slow.lua",
            r#"return {
                id = "slow_check",
                name = "Slow Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    local started = os.clock()
                    while os.clock() - started < 0.2 do
                    end
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 7,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device("test:rain", false))
            .await
            .expect("sensor upsert succeeds");
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let runner = AutomationRunner::new(catalog);
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runner
                    .run_with_options(
                        runtime,
                        ExecutionControl {
                            semaphore: Arc::new(Semaphore::new(1)),
                            timeout: Duration::from_secs(1),
                        },
                    )
                    .await;
            }
        });

        sleep(Duration::from_millis(25)).await;

        runtime
            .registry()
            .upsert(sample_device("test:rain", true))
            .await
            .expect("first matching event succeeds");
        runtime
            .registry()
            .upsert(sample_device("test:rain", false))
            .await
            .expect("reset event succeeds");
        runtime
            .registry()
            .upsert(sample_device("test:rain", true))
            .await
            .expect("second matching event succeeds");

        sleep(Duration::from_millis(350)).await;

        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(7))
        );

        task.abort();
        let _ = task.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lag_recovery_executes_matching_device_state_from_registry_snapshot() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "recover.lua",
            r#"return {
                id = "recover_check",
                name = "Recover Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    if event.recovered == true then
                        ctx:command("test:device", {
                            capability = "brightness",
                            action = "set",
                            value = event.skipped_events,
                        })
                    end
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 1,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device("test:rain", false))
            .await
            .expect("sensor upsert succeeds");
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        runtime
            .registry()
            .upsert(sample_device("test:rain", true))
            .await
            .expect("matching device state is updated in registry");

        let mut executions = JoinSet::new();
        recover_lagged_event_automations(
            &mut executions,
            runtime.clone(),
            &catalog,
            None,
            ExecutionControl {
                semaphore: Arc::new(Semaphore::new(1)),
                timeout: Duration::from_secs(1),
            },
            2,
            None,
            None,
        );

        let recovered_value = timeout(Duration::from_secs(2), async {
            executions.join_next().await;
            let brightness = runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness")
                .cloned()
                .expect("brightness is set by recovered automation");

            match brightness {
                AttributeValue::Integer(value) => value,
                other => panic!("unexpected brightness value: {other:?}"),
            }
        })
        .await
        .expect("lag recovery should execute automation");

        assert_eq!(recovered_value, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_lua_execution_times_out_before_mutating_state() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "slow.lua",
            r#"return {
                id = "slow_timeout",
                name = "Slow Timeout",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    local started = os.clock()
                    while os.clock() - started < 0.25 do
                    end
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 77,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device exists");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let automation = catalog
            .automations
            .first()
            .cloned()
            .expect("automation exists");
        let error = execute_automation(
            &automation,
            runtime.clone(),
            AttributeValue::Object(HashMap::new()),
            None,
            Duration::from_millis(50),
        )
        .expect_err("slow Lua execution should time out");

        assert!(error.to_string().contains("automation execution timed out"));
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(0))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn automation_runner_records_execution_history_via_observer() {
        let dir = temp_dir("smart-home-automations");
        write_automation(
            &dir,
            "rain.lua",
            r#"return {
                id = "rain_check",
                name = "Rain Check",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 42,
                    })
                end
            }"#,
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device("test:rain", false))
            .await
            .expect("sensor upsert succeeds");
        runtime
            .registry()
            .upsert(target_device("test:device"))
            .await
            .expect("target device upsert succeeds");

        let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
        let observer = Arc::new(RecordingObserver::default());
        let runner = AutomationRunner::new(catalog).with_observer(observer.clone());
        let runtime_for_runner = runtime.clone();
        let task = tokio::spawn(async move {
            runner.run(runtime_for_runner).await;
        });

        sleep(Duration::from_millis(25)).await;

        runtime
            .registry()
            .upsert(sample_device("test:rain", true))
            .await
            .expect("sensor change succeeds");

        timeout(Duration::from_secs(2), async {
            loop {
                if !observer.entries.lock().expect("observer lock").is_empty() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("observer should receive execution record");

        let entries = observer.entries.lock().expect("observer lock");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].automation_id, "rain_check");
        assert_eq!(entries[0].status, "ok");
        assert!(entries[0].duration_ms >= 0);
        assert_eq!(entries[0].results[0].target, "test:device");

        task.abort();
        let _ = task.await;
    }
}
