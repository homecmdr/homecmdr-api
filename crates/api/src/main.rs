use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::RawQuery;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use notify::{EventKind as NotifyEventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::json;
use smart_home_adapters as _;
use smart_home_automations::{
    AutomationCatalog, AutomationController, AutomationExecutionObserver, AutomationRunner,
    TriggerContext,
};
use smart_home_core::adapter::{registered_adapter_factories, Adapter};
use smart_home_core::capability::{
    CapabilityOwnershipPolicy, CapabilitySchema, ALL_CAPABILITIES, CAPABILITY_OWNERSHIP,
};
use smart_home_core::command::DeviceCommand;
use smart_home_core::config::{Config, PersistenceBackend, TelemetrySelectionConfig};
use smart_home_core::event::Event;
use smart_home_core::model::{DeviceGroup, DeviceId, GroupId, Room, RoomId};
use smart_home_core::runtime::Runtime;
use smart_home_core::store::{
    AttributeHistoryEntry, AutomationExecutionHistoryEntry, CommandAuditEntry, DeviceHistoryEntry,
    DeviceStore, SceneExecutionHistoryEntry, SceneStepResult,
};
use smart_home_scenes::{
    SceneCatalog, SceneExecutionResult, SceneRunOutcome, SceneRunner, SceneSummary,
};
use store_sql::{HistorySelection, SqliteDeviceStore, SqliteHistoryConfig};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::Level;

#[derive(Clone, Debug, Serialize)]
struct AdapterSummary {
    name: String,
    status: String,
    message: Option<String>,
    last_updated: Option<DateTime<Utc>>,
    last_success: Option<DateTime<Utc>>,
    last_error: Option<AdapterErrorSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
struct AdapterErrorSnapshot {
    message: String,
    observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
struct AdapterDetailResponse {
    name: String,
    runtime_status: String,
    health: ComponentStatus,
    last_success: Option<DateTime<Utc>>,
    last_error: Option<AdapterErrorSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
struct ComponentStatus {
    status: String,
    message: Option<String>,
    last_updated: Option<DateTime<Utc>>,
}

#[derive(Clone, Serialize)]
struct HealthResponse {
    status: String,
    ready: bool,
    runtime: ComponentStatus,
    persistence: ComponentStatus,
    automations: ComponentStatus,
    adapters: Vec<AdapterSummary>,
}

#[derive(Clone, Serialize)]
struct ReadyResponse {
    status: &'static str,
}

#[derive(Debug, Deserialize)]
struct CreateRoomRequest {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct AssignRoomRequest {
    room_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateGroupRequest {
    id: String,
    name: String,
    #[serde(default)]
    members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SetGroupMembersRequest {
    #[serde(default)]
    members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AutomationEnabledRequest {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct ManualAutomationRequest {
    trigger_payload: Option<smart_home_core::model::AttributeValue>,
}

#[derive(Debug, Serialize)]
struct RoomCommandResult {
    device_id: String,
    status: &'static str,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct GroupCommandResult {
    device_id: String,
    status: &'static str,
    message: Option<String>,
}

#[derive(Clone)]
struct AppState {
    runtime: Arc<Runtime>,
    scenes: Arc<RwLock<SceneRunner>>,
    automations: Arc<RwLock<Arc<AutomationCatalog>>>,
    automation_control: Arc<RwLock<Arc<AutomationController>>>,
    automation_runner_tx: watch::Sender<AutomationRunner>,
    automation_observer: Option<Arc<dyn AutomationExecutionObserver>>,
    trigger_context: TriggerContext,
    health: HealthState,
    store: Option<Arc<dyn DeviceStore>>,
    history: HistorySettings,
    scenes_enabled: bool,
    automations_enabled: bool,
    scenes_directory: String,
    automations_directory: String,
    scenes_watch: bool,
    automations_watch: bool,
    scripts_watch: bool,
    scripts_enabled: bool,
    scripts_directory: String,
}

#[derive(Clone)]
struct HistorySettings {
    enabled: bool,
    default_limit: usize,
    max_limit: usize,
}

#[derive(Debug, Serialize)]
struct SceneExecuteResponse {
    status: &'static str,
    results: Vec<SceneExecutionResult>,
}

#[derive(Debug, Serialize)]
struct ReloadErrorDetail {
    file: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct ReloadResponse {
    status: &'static str,
    target: &'static str,
    loaded_count: usize,
    errors: Vec<ReloadErrorDetail>,
    duration_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReloadTarget {
    Scenes,
    Automations,
    Scripts,
}

#[derive(Clone)]
struct ReloadController {
    scenes_enabled: bool,
    automations_enabled: bool,
    scripts_enabled: bool,
    scenes_directory: String,
    automations_directory: String,
    scripts_directory: String,
    scenes: Arc<RwLock<SceneRunner>>,
    automations: Arc<RwLock<Arc<AutomationCatalog>>>,
    automation_control: Arc<RwLock<Arc<AutomationController>>>,
    automation_runner_tx: watch::Sender<AutomationRunner>,
    automation_observer: Option<Arc<dyn AutomationExecutionObserver>>,
    store: Option<Arc<dyn DeviceStore>>,
    trigger_context: TriggerContext,
    runtime: Arc<Runtime>,
}

struct ReloadOutcome {
    loaded_count: usize,
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
struct ReloadWatchResponse {
    status: &'static str,
    watches: Vec<ReloadWatchItem>,
}

#[derive(Debug, Serialize)]
struct ReloadWatchItem {
    target: &'static str,
    enabled: bool,
    directory: String,
}

type BuiltAdapters = (Vec<Box<dyn Adapter>>, Vec<String>);

#[derive(Clone)]
struct HealthState {
    inner: Arc<RwLock<HealthSnapshot>>,
}

#[derive(Clone)]
struct HealthSnapshot {
    startup_complete: bool,
    runtime: ComponentStatus,
    persistence: ComponentStatus,
    automations: ComponentStatus,
    adapters: HashMap<String, AdapterHealth>,
}

#[derive(Clone)]
struct AdapterHealth {
    current: ComponentStatus,
    last_success: Option<DateTime<Utc>>,
    last_error: Option<AdapterErrorSnapshot>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct DeviceHistoryResponse {
    device_id: String,
    entries: Vec<DeviceHistoryEntry>,
}

#[derive(Debug, Serialize)]
struct AttributeHistoryResponse {
    device_id: String,
    attribute: String,
    entries: Vec<AttributeHistoryEntry>,
}

#[derive(Debug, Serialize)]
struct CommandAuditResponse {
    entries: Vec<CommandAuditEntry>,
}

#[derive(Debug, Serialize)]
struct SceneHistoryResponse {
    scene_id: String,
    entries: Vec<SceneExecutionHistoryEntry>,
}

#[derive(Debug, Serialize)]
struct AutomationHistoryResponse {
    automation_id: String,
    entries: Vec<AutomationExecutionHistoryEntry>,
}

#[derive(Debug, Serialize)]
struct AutomationResponse {
    id: String,
    name: String,
    description: Option<String>,
    trigger_type: &'static str,
    condition_count: usize,
    status: &'static str,
    last_run: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct AutomationValidateResponse {
    status: &'static str,
    automation: AutomationResponse,
}

#[derive(Debug, Serialize)]
struct AutomationExecuteResponse {
    status: String,
    error: Option<String>,
    duration_ms: i64,
    results: Vec<SceneStepResult>,
}

#[derive(Debug, Serialize)]
struct CapabilityCatalogResponse {
    capabilities: Vec<CapabilityResponse>,
    ownership: CapabilityOwnershipResponse,
}

#[derive(Debug, Serialize)]
struct CapabilityResponse {
    domain: &'static str,
    key: &'static str,
    schema: CapabilitySchemaResponse,
    read_only: bool,
    actions: Vec<&'static str>,
    description: &'static str,
}

#[derive(Debug, Serialize)]
struct CapabilityOwnershipResponse {
    canonical_attribute_location: &'static str,
    custom_attribute_prefix: &'static str,
    vendor_metadata_field: &'static str,
    rules: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct CapabilitySchemaResponse {
    #[serde(rename = "type")]
    kind: &'static str,
    values: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct DiagnosticsResponse {
    status: String,
    ready: bool,
    devices: usize,
    rooms: usize,
    groups: usize,
    scenes: usize,
    automations: usize,
    history_enabled: bool,
    default_history_limit: usize,
    max_history_limit: usize,
    runtime: ComponentStatus,
    persistence: ComponentStatus,
    automations_component: ComponentStatus,
    adapters: Vec<AdapterSummary>,
}

#[derive(Clone)]
struct StoreAutomationObserver {
    store: Arc<dyn DeviceStore>,
}

impl ComponentStatus {
    fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            message: None,
            last_updated: Some(Utc::now()),
        }
    }

    fn starting(message: impl Into<String>) -> Self {
        Self {
            status: "starting".to_string(),
            message: Some(message.into()),
            last_updated: Some(Utc::now()),
        }
    }

    fn set_ok(&mut self) {
        self.status = "ok".to_string();
        self.message = None;
        self.last_updated = Some(Utc::now());
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.status = "error".to_string();
        self.message = Some(message.into());
        self.last_updated = Some(Utc::now());
    }
}

impl AdapterHealth {
    fn starting(message: impl Into<String>) -> Self {
        Self {
            current: ComponentStatus::starting(message),
            last_success: None,
            last_error: None,
        }
    }

    fn ok() -> Self {
        let current = ComponentStatus::ok();
        Self {
            last_success: current.last_updated,
            current,
            last_error: None,
        }
    }
}

impl HealthState {
    fn new(adapter_names: &[String], persistence_enabled: bool, automations_enabled: bool) -> Self {
        let adapters = adapter_names
            .iter()
            .cloned()
            .map(|name| (name, AdapterHealth::starting("waiting for adapter start")))
            .collect();

        Self {
            inner: Arc::new(RwLock::new(HealthSnapshot {
                startup_complete: false,
                runtime: ComponentStatus::starting("starting runtime"),
                persistence: if persistence_enabled {
                    ComponentStatus::starting("starting persistence worker")
                } else {
                    ComponentStatus::ok()
                },
                automations: if automations_enabled {
                    ComponentStatus::starting("starting automation runner")
                } else {
                    ComponentStatus::ok()
                },
                adapters,
            })),
        }
    }

    fn mark_startup_complete(&self) {
        let mut snapshot = self.write();
        snapshot.startup_complete = true;
        snapshot.runtime.set_ok();
    }

    fn adapter_started(&self, adapter: &str) {
        let mut snapshot = self.write();
        let status = snapshot
            .adapters
            .entry(adapter.to_string())
            .or_insert_with(AdapterHealth::ok);
        status.current.set_ok();
        status.last_success = status.current.last_updated;
    }

    fn adapter_error(&self, adapter: &str, message: impl Into<String>) {
        let mut snapshot = self.write();
        let message = message.into();
        let status = snapshot
            .adapters
            .entry(adapter.to_string())
            .or_insert_with(|| AdapterHealth::starting("adapter not started"));
        status.current.set_error(message.clone());
        status.last_error = status
            .current
            .last_updated
            .map(|observed_at| AdapterErrorSnapshot {
                message,
                observed_at,
            });
    }

    fn runtime_error(&self, message: impl Into<String>) {
        self.write().runtime.set_error(message);
    }

    fn persistence_ok(&self) {
        self.write().persistence.set_ok();
    }

    fn persistence_error(&self, message: impl Into<String>) {
        self.write().persistence.set_error(message);
    }

    fn automations_ok(&self) {
        self.write().automations.set_ok();
    }

    fn automations_error(&self, message: impl Into<String>) {
        self.write().automations.set_error(message);
    }

    fn response(&self) -> HealthResponse {
        let snapshot = self.read();
        let mut adapters = snapshot
            .adapters
            .iter()
            .map(|(name, status)| AdapterSummary {
                name: name.clone(),
                status: status.current.status.clone(),
                message: status.current.message.clone(),
                last_updated: status.current.last_updated,
                last_success: status.last_success,
                last_error: status.last_error.clone(),
            })
            .collect::<Vec<_>>();
        adapters.sort_by(|a, b| a.name.cmp(&b.name));

        let ready = snapshot.startup_complete
            && snapshot.runtime.status == "ok"
            && snapshot.persistence.status == "ok"
            && snapshot.automations.status == "ok"
            && snapshot
                .adapters
                .values()
                .all(|status| status.current.status == "ok");
        let overall_status = if ready { "ok" } else { "degraded" };

        HealthResponse {
            status: overall_status.to_string(),
            ready,
            runtime: snapshot.runtime.clone(),
            persistence: snapshot.persistence.clone(),
            automations: snapshot.automations.clone(),
            adapters,
        }
    }

    fn is_ready(&self) -> bool {
        self.response().ready
    }

    fn adapter_detail(&self, adapter: &str) -> Option<AdapterDetailResponse> {
        let snapshot = self.read();
        snapshot
            .adapters
            .get(adapter)
            .map(|status| AdapterDetailResponse {
                name: adapter.to_string(),
                runtime_status: status.current.status.clone(),
                health: status.current.clone(),
                last_success: status.last_success,
                last_error: status.last_error.clone(),
            })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HealthSnapshot> {
        match self.inner.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HealthSnapshot> {
        match self.inner.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl HistorySettings {
    fn from_config(config: &Config) -> Self {
        Self {
            enabled: config.persistence.enabled && config.persistence.history.enabled,
            default_limit: config.persistence.history.default_query_limit,
            max_limit: config.persistence.history.max_query_limit,
        }
    }

    fn resolve_limit(&self, requested: Option<usize>) -> Result<usize, ApiError> {
        let limit = requested.unwrap_or(self.default_limit);
        if limit == 0 {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "history query limit must be greater than zero",
            ));
        }
        if limit > self.max_limit {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("history query limit must be <= {}", self.max_limit),
            ));
        }

        Ok(limit)
    }
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = config_path_from_args()?;
    let config = Config::load_from_file(&config_path)
        .with_context(|| format!("failed to load configuration from {config_path}"))?;

    init_tracing(&config.logging.level)?;

    let device_store = create_device_store(&config)
        .await
        .context("failed to create persistence store")?;

    let (adapters, adapter_summaries) =
        build_adapters(&config).context("failed to build adapters")?;
    let health = HealthState::new(
        &adapter_summaries,
        device_store.is_some(),
        config.automations.enabled,
    );

    let runtime = Arc::new(Runtime::new(adapters, config.runtime));
    let scripts_root = config
        .scripts
        .enabled
        .then(|| std::path::PathBuf::from(&config.scripts.directory));
    let scenes = if config.scenes.enabled {
        SceneRunner::new(
            SceneCatalog::load_from_directory(&config.scenes.directory, scripts_root.clone())
                .with_context(|| {
                    format!("failed to load scenes from {}", config.scenes.directory)
                })?,
        )
    } else {
        SceneRunner::new(SceneCatalog::empty())
    };
    let automations = if config.automations.enabled {
        Arc::new(
            AutomationCatalog::load_from_directory(&config.automations.directory, scripts_root)
                .with_context(|| {
                    format!(
                        "failed to load automations from {}",
                        config.automations.directory
                    )
                })?,
        )
    } else {
        Arc::new(AutomationCatalog::empty())
    };

    if let Some(store) = &device_store {
        let rooms = store
            .load_all_rooms()
            .await
            .context("failed to load persisted rooms")?;
        let groups = store
            .load_all_groups()
            .await
            .context("failed to load persisted groups")?;
        let devices = store
            .load_all_devices()
            .await
            .context("failed to load persisted devices")?;
        runtime.registry().restore_rooms(rooms);
        runtime
            .registry()
            .restore(devices)
            .context("failed to restore persisted devices into registry")?;
        runtime.registry().restore_groups(groups);
    }

    let automation_observer =
        device_store.clone().and_then(|store| {
            if config.persistence.history.enabled {
                Some(Arc::new(StoreAutomationObserver { store })
                    as Arc<dyn AutomationExecutionObserver>)
            } else {
                None
            }
        });
    let trigger_context = trigger_context_from_config(&config);
    let automation_runner = build_automation_runner(
        (*automations).clone(),
        automation_observer.clone(),
        device_store.clone(),
        trigger_context,
    );
    let automation_control = Arc::new(automation_runner.controller());
    let (automation_runner_tx, automation_runner_rx) = watch::channel(automation_runner.clone());
    let app_state = AppState {
        runtime: runtime.clone(),
        scenes: Arc::new(RwLock::new(scenes)),
        automations: Arc::new(RwLock::new(automations.clone())),
        automation_control: Arc::new(RwLock::new(automation_control.clone())),
        automation_runner_tx,
        automation_observer: automation_observer.clone(),
        trigger_context,
        health: health.clone(),
        store: device_store.clone(),
        history: HistorySettings::from_config(&config),
        scenes_enabled: config.scenes.enabled,
        automations_enabled: config.automations.enabled,
        scenes_directory: config.scenes.directory.clone(),
        automations_directory: config.automations.directory.clone(),
        scenes_watch: config.scenes.watch,
        automations_watch: config.automations.watch,
        scripts_watch: config.scripts.watch,
        scripts_enabled: config.scripts.enabled,
        scripts_directory: config.scripts.directory.clone(),
    };
    let app = app(app_state.clone(), &config);
    let reload_controller = ReloadController {
        scenes_enabled: config.scenes.enabled,
        automations_enabled: config.automations.enabled,
        scripts_enabled: config.scripts.enabled,
        scenes_directory: config.scenes.directory.clone(),
        automations_directory: config.automations.directory.clone(),
        scripts_directory: config.scripts.directory.clone(),
        scenes: app_state.scenes.clone(),
        automations: app_state.automations.clone(),
        automation_control: app_state.automation_control.clone(),
        automation_runner_tx: app_state.automation_runner_tx.clone(),
        automation_observer: app_state.automation_observer.clone(),
        store: app_state.store.clone(),
        trigger_context,
        runtime: runtime.clone(),
    };
    let listener = tokio::net::TcpListener::bind(&config.api.bind_address)
        .await
        .with_context(|| format!("failed to bind API listener on {}", config.api.bind_address))?;

    let (persistence_shutdown_tx, persistence_shutdown_rx) = tokio::sync::oneshot::channel();
    let mut persistence_shutdown_tx = Some(persistence_shutdown_tx);
    let persistence_task = device_store.clone().map(|store| {
        let runtime = runtime.clone();
        let health = health.clone();
        tokio::spawn(async move {
            run_persistence_worker(runtime, store, health, persistence_shutdown_rx).await;
        })
    });

    let runtime_task = {
        let runtime = runtime.clone();
        let health = health.clone();
        tokio::spawn(async move {
            monitor_runtime_health(runtime, health).await;
        })
    };
    let automation_task = {
        let runtime = runtime.clone();
        let health = health.clone();
        let mut runner_rx = automation_runner_rx;
        tokio::spawn(async move {
            health.automations_ok();
            let mut active_task = {
                let runtime = runtime.clone();
                let initial = runner_rx.borrow().clone();
                tokio::spawn(async move {
                    initial.run(runtime).await;
                })
            };

            loop {
                tokio::select! {
                    _ = &mut active_task => {
                        health.automations_error("automation runner exited unexpectedly");
                        break;
                    }
                    changed = runner_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        active_task.abort();
                        let _ = active_task.await;

                        let runtime = runtime.clone();
                        let runner = runner_rx.borrow().clone();
                        active_task = tokio::spawn(async move {
                            runner.run(runtime).await;
                        });
                    }
                }
            }

            active_task.abort();
            let _ = active_task.await;
        })
    };

    let watcher_task = spawn_reload_watchers_if_enabled(&config, reload_controller.clone());

    health.mark_startup_complete();

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("API server failed")?;

    runtime_task.abort();
    let _ = runtime_task.await;

    automation_task.abort();
    let _ = automation_task.await;

    if let Some(task) = watcher_task {
        task.abort();
        let _ = task.await;
    }

    if let Some(task) = persistence_task {
        if let Some(shutdown) = persistence_shutdown_tx.take() {
            let _ = shutdown.send(());
        }
        let _ = task.await;
    }

    Ok(())
}

fn build_adapters(config: &Config) -> Result<BuiltAdapters> {
    let mut factories = std::collections::HashMap::new();

    for factory in registered_adapter_factories() {
        if factories.insert(factory.name(), factory).is_some() {
            anyhow::bail!(
                "duplicate adapter factory registration for '{}'",
                factory.name()
            );
        }
    }

    let mut adapters = Vec::new();
    let mut summaries = Vec::new();

    for (name, adapter_config) in &config.adapters {
        let factory = factories
            .get(name.as_str())
            .copied()
            .with_context(|| format!("no adapter factory registered for '{name}'"))?;

        if let Some(adapter) = factory
            .build(adapter_config.clone())
            .with_context(|| format!("failed to build adapter '{name}'"))?
        {
            summaries.push(name.clone());
            adapters.push(adapter);
        }
    }

    Ok((adapters, summaries))
}

async fn create_device_store(config: &Config) -> Result<Option<Arc<dyn DeviceStore>>> {
    if !config.persistence.enabled {
        return Ok(None);
    }

    let database_url = config
        .persistence
        .database_url
        .as_deref()
        .context("persistence.database_url is required when persistence is enabled")?;

    let store: Arc<dyn DeviceStore> = match config.persistence.backend {
        PersistenceBackend::Sqlite => Arc::new(
            SqliteDeviceStore::new_with_history(
                database_url,
                config.persistence.auto_create,
                SqliteHistoryConfig {
                    enabled: config.persistence.history.enabled,
                    retention: config
                        .persistence
                        .history
                        .retention_days
                        .map(|days| Duration::from_secs(days.saturating_mul(24 * 60 * 60))),
                    selection: history_selection_from_config(config),
                },
            )
            .await
            .with_context(|| format!("failed to initialize SQLite store '{database_url}'"))?,
        ),
        PersistenceBackend::Postgres => {
            anyhow::bail!("persistence backend 'postgres' is not implemented yet")
        }
    };

    Ok(Some(store))
}

fn history_selection_from_config(config: &Config) -> HistorySelection {
    if !config.telemetry.enabled {
        return HistorySelection::default();
    }

    history_selection_from_telemetry(&config.telemetry.selection)
}

fn history_selection_from_telemetry(selection: &TelemetrySelectionConfig) -> HistorySelection {
    HistorySelection {
        device_ids: selection.device_ids.clone(),
        capabilities: selection.capabilities.clone(),
        adapter_names: selection.adapter_names.clone(),
    }
}

fn trigger_context_from_config(config: &Config) -> TriggerContext {
    TriggerContext {
        latitude: config.locale.latitude,
        longitude: config.locale.longitude,
        timezone: config.locale.timezone.parse().ok(),
    }
}

fn build_automation_runner(
    catalog: AutomationCatalog,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    store: Option<Arc<dyn DeviceStore>>,
    trigger_context: TriggerContext,
) -> AutomationRunner {
    let runner = if let Some(observer) = observer {
        AutomationRunner::new(catalog)
            .with_observer(observer)
            .with_trigger_context(trigger_context)
    } else {
        AutomationRunner::new(catalog).with_trigger_context(trigger_context)
    };

    if let Some(store) = store {
        runner.with_state_store(store)
    } else {
        runner
    }
}

#[cfg(test)]
fn make_state(
    runtime: Arc<Runtime>,
    scenes: SceneRunner,
    automations: Arc<AutomationCatalog>,
    trigger_context: TriggerContext,
    health: HealthState,
    store: Option<Arc<dyn DeviceStore>>,
    history: HistorySettings,
    scenes_enabled: bool,
    automations_enabled: bool,
    scenes_directory: String,
    automations_directory: String,
    scripts_root: Option<PathBuf>,
) -> AppState {
    let observer = if history.enabled {
        store.clone().map(|store| {
            Arc::new(StoreAutomationObserver { store }) as Arc<dyn AutomationExecutionObserver>
        })
    } else {
        None
    };
    let runner = build_automation_runner(
        (*automations).clone(),
        observer.clone(),
        store.clone(),
        trigger_context,
    );
    let control = Arc::new(runner.controller());
    let (automation_runner_tx, _automation_runner_rx) = watch::channel(runner);

    AppState {
        runtime,
        scenes: Arc::new(RwLock::new(scenes)),
        automations: Arc::new(RwLock::new(automations)),
        automation_control: Arc::new(RwLock::new(control)),
        automation_runner_tx,
        automation_observer: observer,
        trigger_context,
        health,
        store,
        history,
        scenes_enabled,
        automations_enabled,
        scenes_directory,
        automations_directory,
        scenes_watch: false,
        automations_watch: false,
        scripts_watch: false,
        scripts_enabled: scripts_root.is_some(),
        scripts_directory: scripts_root
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default(),
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|p| p.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|p| p.into_inner())
}

fn scripts_root_from_controller(controller: &ReloadController) -> Option<PathBuf> {
    controller
        .scripts_enabled
        .then(|| PathBuf::from(&controller.scripts_directory))
}

fn reload_controller_from_state(state: &AppState) -> ReloadController {
    ReloadController {
        scenes_enabled: state.scenes_enabled,
        automations_enabled: state.automations_enabled,
        scripts_enabled: state.scripts_enabled,
        scenes_directory: state.scenes_directory.clone(),
        automations_directory: state.automations_directory.clone(),
        scripts_directory: state.scripts_directory.clone(),
        scenes: state.scenes.clone(),
        automations: state.automations.clone(),
        automation_control: state.automation_control.clone(),
        automation_runner_tx: state.automation_runner_tx.clone(),
        automation_observer: state.automation_observer.clone(),
        store: state.store.clone(),
        trigger_context: state.trigger_context,
        runtime: state.runtime.clone(),
    }
}

fn scripts_loaded_count(controller: &ReloadController) -> usize {
    match std::fs::read_dir(&controller.scripts_directory) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.path().is_file()
                    && entry.path().extension().and_then(|ext| ext.to_str()) == Some("lua")
            })
            .count(),
        Err(_) => 0,
    }
}

fn reload_response_from_result(
    target: &'static str,
    result: std::result::Result<ReloadOutcome, Vec<ReloadErrorDetail>>,
) -> ReloadResponse {
    match result {
        Ok(outcome) => ReloadResponse {
            status: "ok",
            target,
            loaded_count: outcome.loaded_count,
            errors: Vec::new(),
            duration_ms: outcome.duration_ms,
        },
        Err(errors) => ReloadResponse {
            status: "error",
            target,
            loaded_count: 0,
            errors,
            duration_ms: 0,
        },
    }
}

fn reload_scenes_internal(
    controller: &ReloadController,
) -> std::result::Result<ReloadOutcome, Vec<ReloadErrorDetail>> {
    if !controller.scenes_enabled {
        return Err(vec![ReloadErrorDetail {
            file: controller.scenes_directory.clone(),
            message: "scene reload is not supported when scenes are disabled".to_string(),
        }]);
    }

    let started = Instant::now();
    controller
        .runtime
        .bus()
        .publish(Event::SceneCatalogReloadStarted);
    match SceneCatalog::reload_from_directory(
        &controller.scenes_directory,
        scripts_root_from_controller(controller),
    ) {
        Ok(catalog) => {
            let loaded_count = catalog.summaries().len();
            let duration_ms = started.elapsed().as_millis() as u64;
            let mut runner = write_lock(&controller.scenes);
            *runner = SceneRunner::new(catalog);
            controller
                .runtime
                .bus()
                .publish(Event::SceneCatalogReloaded {
                    loaded_count,
                    duration_ms,
                });
            Ok(ReloadOutcome {
                loaded_count,
                duration_ms: duration_ms as u128,
            })
        }
        Err(errors) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            controller
                .runtime
                .bus()
                .publish(Event::SceneCatalogReloadFailed {
                    duration_ms,
                    errors: errors
                        .iter()
                        .map(|error| smart_home_core::event::ReloadError {
                            file: error.file.clone(),
                            message: error.message.clone(),
                        })
                        .collect(),
                });
            Err(errors
                .into_iter()
                .map(|error| ReloadErrorDetail {
                    file: error.file,
                    message: error.message,
                })
                .collect())
        }
    }
}

fn reload_automations_internal(
    controller: &ReloadController,
) -> std::result::Result<ReloadOutcome, Vec<ReloadErrorDetail>> {
    if !controller.automations_enabled {
        return Err(vec![ReloadErrorDetail {
            file: controller.automations_directory.clone(),
            message: "automation reload is not supported when automations are disabled".to_string(),
        }]);
    }

    let started = Instant::now();
    controller
        .runtime
        .bus()
        .publish(Event::AutomationCatalogReloadStarted);

    let previous_controller = {
        let guard = read_lock(&controller.automation_control);
        guard.clone()
    };
    let previous_enabled = previous_controller
        .summaries()
        .into_iter()
        .filter_map(|summary| {
            previous_controller
                .is_enabled(&summary.id)
                .map(|enabled| (summary.id, enabled))
        })
        .collect::<Vec<_>>();

    match AutomationCatalog::reload_from_directory(
        &controller.automations_directory,
        scripts_root_from_controller(controller),
    ) {
        Ok(catalog) => {
            for (id, enabled) in previous_enabled {
                if catalog.get(&id).is_some() {
                    let _ = catalog.set_enabled(&id, enabled);
                }
            }

            let loaded_count = catalog.summaries().len();
            let duration_ms = started.elapsed().as_millis() as u64;
            let runner = build_automation_runner(
                catalog.clone(),
                controller.automation_observer.clone(),
                controller.store.clone(),
                controller.trigger_context,
            );
            let next_controller = Arc::new(runner.controller());

            {
                let mut catalog_guard = write_lock(&controller.automations);
                *catalog_guard = Arc::new(catalog);
            }
            {
                let mut controller_guard = write_lock(&controller.automation_control);
                *controller_guard = next_controller;
            }

            if controller.automation_runner_tx.send(runner).is_err() {
                tracing::warn!("automation reload completed but no active runner supervisor was available to receive the updated runner");
            }
            controller
                .runtime
                .bus()
                .publish(Event::AutomationCatalogReloaded {
                    loaded_count,
                    duration_ms,
                });

            Ok(ReloadOutcome {
                loaded_count,
                duration_ms: duration_ms as u128,
            })
        }
        Err(errors) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            controller
                .runtime
                .bus()
                .publish(Event::AutomationCatalogReloadFailed {
                    duration_ms,
                    errors: errors
                        .iter()
                        .map(|error| smart_home_core::event::ReloadError {
                            file: error.file.clone(),
                            message: error.message.clone(),
                        })
                        .collect(),
                });
            Err(errors
                .into_iter()
                .map(|error| ReloadErrorDetail {
                    file: error.file,
                    message: error.message,
                })
                .collect())
        }
    }
}

fn reload_scripts_internal(
    controller: &ReloadController,
) -> std::result::Result<ReloadOutcome, Vec<ReloadErrorDetail>> {
    if !controller.scripts_enabled {
        return Err(vec![ReloadErrorDetail {
            file: controller.scripts_directory.clone(),
            message: "scripts reload is not supported when scripts are disabled".to_string(),
        }]);
    }

    let started = Instant::now();
    controller
        .runtime
        .bus()
        .publish(Event::ScriptsReloadStarted);
    if let Err(error) = std::fs::read_dir(&controller.scripts_directory) {
        let duration_ms = started.elapsed().as_millis() as u64;
        let errors = vec![ReloadErrorDetail {
            file: controller.scripts_directory.clone(),
            message: format!("failed to read scripts directory: {error}"),
        }];
        controller
            .runtime
            .bus()
            .publish(Event::ScriptsReloadFailed {
                duration_ms,
                errors: errors
                    .iter()
                    .map(|error| smart_home_core::event::ReloadError {
                        file: error.file.clone(),
                        message: error.message.clone(),
                    })
                    .collect(),
            });
        return Err(errors);
    }

    let loaded_count = scripts_loaded_count(controller);
    let duration_ms = started.elapsed().as_millis() as u64;
    controller.runtime.bus().publish(Event::ScriptsReloaded {
        loaded_count,
        duration_ms,
    });
    Ok(ReloadOutcome {
        loaded_count,
        duration_ms: duration_ms as u128,
    })
}

async fn run_reload_target(controller: ReloadController, target: ReloadTarget) {
    let result = tokio::task::spawn_blocking(move || match target {
        ReloadTarget::Scenes => reload_scenes_internal(&controller),
        ReloadTarget::Automations => reload_automations_internal(&controller),
        ReloadTarget::Scripts => reload_scripts_internal(&controller),
    })
    .await;

    if let Err(error) = result {
        tracing::warn!("reload task join error: {error}");
    }
}

fn spawn_reload_watchers_if_enabled(
    config: &Config,
    controller: ReloadController,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.scenes.watch && !config.automations.watch && !config.scripts.watch {
        return None;
    }

    let mut watched = Vec::<(String, ReloadTarget)>::new();
    if config.scenes.watch {
        watched.push((config.scenes.directory.clone(), ReloadTarget::Scenes));
    }
    if config.automations.watch {
        watched.push((
            config.automations.directory.clone(),
            ReloadTarget::Automations,
        ));
    }
    if config.scripts.watch {
        watched.push((config.scripts.directory.clone(), ReloadTarget::Scripts));
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<ReloadTarget>();
    let watched_map = watched.clone();
    let mut watcher =
        match notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else {
                return;
            };
            let trigger = matches!(
                event.kind,
                NotifyEventKind::Create(_)
                    | NotifyEventKind::Modify(_)
                    | NotifyEventKind::Remove(_)
            );
            if !trigger {
                return;
            }
            for path in event.paths {
                if path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
                    continue;
                }
                for (dir, target) in &watched_map {
                    if path.starts_with(std::path::Path::new(dir)) {
                        let _ = tx.send(*target);
                        return;
                    }
                }
            }
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!("failed to start reload watcher: {error}");
                return None;
            }
        };

    for (dir, _) in &watched {
        if let Err(error) = watcher.watch(std::path::Path::new(dir), RecursiveMode::Recursive) {
            tracing::warn!("failed to watch directory '{}': {error}", dir);
        }
    }

    Some(tokio::spawn(async move {
        let _watcher = watcher;
        let mut last = HashMap::<ReloadTarget, Instant>::new();
        while let Some(target) = rx.recv().await {
            let now = Instant::now();
            if let Some(previous) = last.get(&target) {
                if now.duration_since(*previous) < Duration::from_millis(400) {
                    continue;
                }
            }
            last.insert(target, now);
            run_reload_target(controller.clone(), target).await;
        }
    }))
}

async fn monitor_runtime_health(runtime: Arc<Runtime>, health: HealthState) {
    let mut receiver = runtime.bus().subscribe();

    let monitor_task = tokio::spawn({
        let health = health.clone();
        async move {
            loop {
                match receiver.recv().await {
                    Ok(Event::AdapterStarted { adapter }) => health.adapter_started(&adapter),
                    Ok(Event::SystemError { message }) => {
                        if let Some(adapter) = adapter_name_from_system_error(&message) {
                            health.adapter_error(adapter, message.clone());
                        } else {
                            health.runtime_error(message);
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        health.runtime_error(format!(
                            "health monitor lagged and dropped {skipped} runtime events"
                        ));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    runtime.run().await;

    monitor_task.abort();
    let _ = monitor_task.await;
}

async fn run_persistence_worker(
    runtime: Arc<Runtime>,
    store: Arc<dyn DeviceStore>,
    health: HealthState,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let mut receiver = runtime.bus().subscribe();
    tokio::pin!(shutdown);

    loop {
        let event = tokio::select! {
            _ = &mut shutdown => {
                if let Err(error) = reconcile_device_store(
                    runtime.registry().list_rooms(),
                    runtime.registry().list_groups(),
                    runtime.registry().list(),
                    store.clone(),
                )
                .await
                {
                    health.persistence_error(format!(
                        "failed to flush persisted registry state during shutdown: {error}"
                    ));
                    tracing::error!(error = %error, "failed to flush persisted registry state during shutdown");
                } else {
                    health.persistence_ok();
                }
                break;
            }
            event = receiver.recv() => event,
        };

        match event {
            Ok(Event::DeviceAdded { device }) => {
                if let Err(error) = store.save_device(&device).await {
                    health.persistence_error(format!(
                        "failed to persist added device '{}': {error}",
                        device.id.0
                    ));
                    tracing::error!(device_id = %device.id.0, error = %error, "failed to persist added device");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::DeviceRoomChanged { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!(
                            "failed to persist room change for '{}': {error}",
                            id.0
                        ));
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device room change");
                    } else {
                        health.persistence_ok();
                    }
                }
            }
            Ok(Event::DeviceStateChanged { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!(
                            "failed to persist state change for '{}': {error}",
                            id.0
                        ));
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device state change");
                    } else {
                        health.persistence_ok();
                    }
                } else {
                    tracing::warn!(device_id = %id.0, "device state changed event received after device disappeared from registry");
                }
            }
            Ok(Event::DeviceRemoved { id }) => {
                if let Err(error) = store.delete_device(&id).await {
                    health.persistence_error(format!(
                        "failed to delete persisted device '{}': {error}",
                        id.0
                    ));
                    tracing::error!(device_id = %id.0, error = %error, "failed to delete persisted device");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::DeviceSeen { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!(
                            "failed to persist last_seen for '{}': {error}",
                            id.0
                        ));
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device last_seen update");
                    } else {
                        health.persistence_ok();
                    }
                }
            }
            Ok(Event::RoomAdded { room } | Event::RoomUpdated { room }) => {
                if let Err(error) = store.save_room(&room).await {
                    health.persistence_error(format!(
                        "failed to persist room '{}': {error}",
                        room.id.0
                    ));
                    tracing::error!(room_id = %room.id.0, error = %error, "failed to persist room");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::GroupAdded { group } | Event::GroupUpdated { group }) => {
                if let Err(error) = store.save_group(&group).await {
                    health.persistence_error(format!(
                        "failed to persist group '{}': {error}",
                        group.id.0
                    ));
                    tracing::error!(group_id = %group.id.0, error = %error, "failed to persist group");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::GroupMembersChanged { id, .. }) => {
                if let Some(group) = runtime.registry().get_group(&id) {
                    if let Err(error) = store.save_group(&group).await {
                        health.persistence_error(format!(
                            "failed to persist group membership change for '{}': {error}",
                            id.0
                        ));
                        tracing::error!(group_id = %id.0, error = %error, "failed to persist group membership change");
                    } else {
                        health.persistence_ok();
                    }
                }
            }
            Ok(Event::GroupRemoved { id }) => {
                if let Err(error) = store.delete_group(&id).await {
                    health.persistence_error(format!(
                        "failed to delete persisted group '{}': {error}",
                        id.0
                    ));
                    tracing::error!(group_id = %id.0, error = %error, "failed to delete persisted group");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::RoomRemoved { id }) => {
                if let Err(error) = store.delete_room(&id).await {
                    health.persistence_error(format!("failed to delete room '{}': {error}", id.0));
                    tracing::error!(room_id = %id.0, error = %error, "failed to delete persisted room");
                } else {
                    health.persistence_ok();
                }

                for device in runtime.registry().list() {
                    if device.room_id.is_none() {
                        if let Err(error) = store.save_device(&device).await {
                            health.persistence_error(format!(
                                "failed to persist room removal update for '{}': {error}",
                                device.id.0
                            ));
                            tracing::error!(device_id = %device.id.0, error = %error, "failed to persist room removal device update");
                        } else {
                            health.persistence_ok();
                        }
                    }
                }
            }
            Ok(
                Event::AdapterStarted { .. }
                | Event::SceneCatalogReloadStarted
                | Event::SceneCatalogReloaded { .. }
                | Event::SceneCatalogReloadFailed { .. }
                | Event::AutomationCatalogReloadStarted
                | Event::AutomationCatalogReloaded { .. }
                | Event::AutomationCatalogReloadFailed { .. }
                | Event::ScriptsReloadStarted
                | Event::ScriptsReloaded { .. }
                | Event::ScriptsReloadFailed { .. }
                | Event::SystemError { .. },
            ) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                health.persistence_error(format!(
                    "persistence worker lagged and dropped {skipped} runtime events"
                ));
                tracing::warn!(
                    skipped,
                    "persistence subscriber lagged; reconciling registry state"
                );

                if let Err(error) = reconcile_device_store(
                    runtime.registry().list_rooms(),
                    runtime.registry().list_groups(),
                    runtime.registry().list(),
                    store.clone(),
                )
                .await
                {
                    health.persistence_error(format!(
                        "failed to reconcile persisted registry state after lag: {error}"
                    ));
                    tracing::error!(error = %error, "failed to reconcile persisted registry state after lag");
                } else {
                    health.persistence_ok();
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

fn adapter_name_from_system_error(message: &str) -> Option<&str> {
    message
        .split_once(' ')
        .map(|(prefix, _)| prefix)
        .filter(|prefix| prefix.contains('_'))
}

async fn reconcile_device_store(
    rooms: Vec<Room>,
    groups: Vec<DeviceGroup>,
    devices: Vec<smart_home_core::model::Device>,
    store: Arc<dyn DeviceStore>,
) -> Result<()> {
    let persisted_room_ids = store
        .load_all_rooms()
        .await
        .context("failed to load persisted rooms for reconciliation")?
        .into_iter()
        .map(|room| room.id)
        .collect::<std::collections::HashSet<_>>();
    let current_room_ids = rooms
        .iter()
        .map(|room| room.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let persisted_group_ids = store
        .load_all_groups()
        .await
        .context("failed to load persisted groups for reconciliation")?
        .into_iter()
        .map(|group| group.id)
        .collect::<std::collections::HashSet<_>>();
    let current_group_ids = groups
        .iter()
        .map(|group| group.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let persisted_ids = store
        .load_all_devices()
        .await
        .context("failed to load persisted devices for reconciliation")?
        .into_iter()
        .map(|device| device.id)
        .collect::<std::collections::HashSet<_>>();
    let current_ids = devices
        .iter()
        .map(|device| device.id.clone())
        .collect::<std::collections::HashSet<_>>();

    for stale_id in persisted_room_ids.difference(&current_room_ids) {
        store.delete_room(stale_id).await.with_context(|| {
            format!(
                "failed to delete stale room '{}' during reconciliation",
                stale_id.0
            )
        })?;
    }

    for room in rooms {
        store.save_room(&room).await.with_context(|| {
            format!("failed to save room '{}' during reconciliation", room.id.0)
        })?;
    }

    for stale_id in persisted_group_ids.difference(&current_group_ids) {
        store.delete_group(stale_id).await.with_context(|| {
            format!(
                "failed to delete stale group '{}' during reconciliation",
                stale_id.0
            )
        })?;
    }

    for group in groups {
        store.save_group(&group).await.with_context(|| {
            format!(
                "failed to save group '{}' during reconciliation",
                group.id.0
            )
        })?;
    }

    for stale_id in persisted_ids.difference(&current_ids) {
        store.delete_device(stale_id).await.with_context(|| {
            format!(
                "failed to delete stale device '{}' during reconciliation",
                stale_id.0
            )
        })?;
    }

    for device in devices {
        store.save_device(&device).await.with_context(|| {
            format!(
                "failed to save device '{}' during reconciliation",
                device.id.0
            )
        })?;
    }

    Ok(())
}

fn app(state: AppState, config: &Config) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/adapters", get(adapters))
        .route("/adapters/{id}", get(get_adapter))
        .route("/capabilities", get(list_capabilities))
        .route("/diagnostics", get(diagnostics))
        .route("/diagnostics/reload_watch", get(reload_watch_diagnostics))
        .route("/scenes", get(list_scenes))
        .route("/scenes/reload", post(reload_scenes))
        .route("/automations", get(list_automations))
        .route("/automations/reload", post(reload_automations))
        .route("/scripts/reload", post(reload_scripts))
        .route("/automations/{id}", get(get_automation))
        .route("/automations/{id}/enabled", post(set_automation_enabled))
        .route("/automations/{id}/validate", post(validate_automation))
        .route(
            "/automations/{id}/execute",
            post(execute_automation_manually),
        )
        .route("/scenes/{id}/history", get(get_scene_history))
        .route("/scenes/{id}/execute", post(execute_scene))
        .route("/automations/{id}/history", get(get_automation_history))
        .route("/rooms", get(list_rooms).post(create_room))
        .route("/rooms/{id}", get(get_room).delete(delete_room))
        .route("/rooms/{id}/devices", get(list_room_devices))
        .route("/rooms/{id}/command", post(command_room_devices))
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/{id}", get(get_group).delete(delete_group))
        .route("/groups/{id}/devices", get(list_group_devices))
        .route("/groups/{id}/members", post(set_group_members))
        .route("/groups/{id}/command", post(command_group_devices))
        .route("/devices", get(list_devices))
        .route("/devices/{id}", get(get_device))
        .route("/devices/{id}/history", get(get_device_history))
        .route(
            "/devices/{id}/history/{attribute}",
            get(get_attribute_history),
        )
        .route("/audit/commands", get(get_command_audit))
        .route("/devices/{id}/room", post(assign_device_room))
        .route("/devices/{id}/command", post(command_device))
        .route("/events", get(events))
        .layer(cors_layer(config))
        .with_state(state)
}

fn cors_layer(config: &Config) -> CorsLayer {
    if !config.api.cors.enabled {
        return CorsLayer::new();
    }

    let allowed_origins = config
        .api
        .cors
        .allowed_origins
        .iter()
        .map(|origin| {
            origin
                .parse()
                .expect("validated CORS origin parses as header value")
        })
        .collect::<Vec<_>>();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed_origins))
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(state.health.response())
}

async fn ready(State(state): State<AppState>) -> Result<Json<ReadyResponse>, ApiError> {
    if state.health.is_ready() {
        Ok(Json(ReadyResponse { status: "ok" }))
    } else {
        Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "system is not ready",
        ))
    }
}

async fn adapters(State(state): State<AppState>) -> Json<Vec<AdapterSummary>> {
    Json(state.health.response().adapters)
}

async fn get_adapter(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AdapterDetailResponse>, ApiError> {
    state
        .health
        .adapter_detail(&id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("adapter '{id}' not found")))
}

async fn list_capabilities() -> Json<CapabilityCatalogResponse> {
    let mut capabilities = ALL_CAPABILITIES
        .iter()
        .flat_map(|group| group.iter())
        .map(|definition| CapabilityResponse {
            domain: definition.domain,
            key: definition.key,
            schema: capability_schema_response(definition.schema),
            read_only: definition.read_only,
            actions: definition.actions.to_vec(),
            description: definition.description,
        })
        .collect::<Vec<_>>();
    capabilities.sort_by(|a, b| a.key.cmp(b.key));

    Json(CapabilityCatalogResponse {
        capabilities,
        ownership: capability_ownership_response(CAPABILITY_OWNERSHIP),
    })
}

async fn diagnostics(State(state): State<AppState>) -> Json<DiagnosticsResponse> {
    let health = state.health.response();
    let scene_count = {
        let runner = read_lock(&state.scenes);
        runner.summaries().len()
    };
    let automation_count = {
        let catalog = read_lock(&state.automations);
        catalog.summaries().len()
    };

    Json(DiagnosticsResponse {
        status: health.status.clone(),
        ready: health.ready,
        devices: state.runtime.registry().list().len(),
        rooms: state.runtime.registry().list_rooms().len(),
        groups: state.runtime.registry().list_groups().len(),
        scenes: scene_count,
        automations: automation_count,
        history_enabled: state.history.enabled,
        default_history_limit: state.history.default_limit,
        max_history_limit: state.history.max_limit,
        runtime: health.runtime,
        persistence: health.persistence,
        automations_component: health.automations,
        adapters: health.adapters,
    })
}

async fn reload_watch_diagnostics(State(state): State<AppState>) -> Json<ReloadWatchResponse> {
    Json(ReloadWatchResponse {
        status: "ok",
        watches: vec![
            ReloadWatchItem {
                target: "scenes",
                enabled: state.scenes_watch,
                directory: state.scenes_directory.clone(),
            },
            ReloadWatchItem {
                target: "automations",
                enabled: state.automations_watch,
                directory: state.automations_directory.clone(),
            },
            ReloadWatchItem {
                target: "scripts",
                enabled: state.scripts_watch,
                directory: state.scripts_directory.clone(),
            },
        ],
    })
}

async fn list_scenes(State(state): State<AppState>) -> Json<Vec<SceneSummary>> {
    let runner = read_lock(&state.scenes);
    Json(runner.summaries())
}

async fn reload_scenes(State(state): State<AppState>) -> Result<Json<ReloadResponse>, ApiError> {
    let controller = reload_controller_from_state(&state);

    let result = tokio::task::spawn_blocking(move || reload_scenes_internal(&controller))
        .await
        .map_err(|error| internal_api_error(anyhow::anyhow!(error.to_string())))?;
    Ok(Json(reload_response_from_result("scenes", result)))
}

async fn list_automations(
    State(state): State<AppState>,
) -> Result<Json<Vec<AutomationResponse>>, ApiError> {
    let controller = {
        let guard = read_lock(&state.automation_control);
        guard.clone()
    };
    let mut automations = Vec::new();
    for summary in controller.summaries() {
        automations.push(automation_response(&state, summary).await?);
    }
    Ok(Json(automations))
}

async fn reload_automations(
    State(state): State<AppState>,
) -> Result<Json<ReloadResponse>, ApiError> {
    let controller = reload_controller_from_state(&state);

    let result = tokio::task::spawn_blocking(move || reload_automations_internal(&controller))
        .await
        .map_err(|error| internal_api_error(anyhow::anyhow!(error.to_string())))?;
    Ok(Json(reload_response_from_result("automations", result)))
}

async fn reload_scripts(State(state): State<AppState>) -> Result<Json<ReloadResponse>, ApiError> {
    let controller = reload_controller_from_state(&state);

    let result = tokio::task::spawn_blocking(move || reload_scripts_internal(&controller))
        .await
        .map_err(|error| internal_api_error(anyhow::anyhow!(error.to_string())))?;
    Ok(Json(reload_response_from_result("scripts", result)))
}

async fn get_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AutomationResponse>, ApiError> {
    let summary = state
        .automation_control
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("automation '{id}' not found")))?;

    Ok(Json(automation_response(&state, summary).await?))
}

async fn set_automation_enabled(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<AutomationEnabledRequest>,
) -> Result<Json<AutomationResponse>, ApiError> {
    let controller = {
        let guard = read_lock(&state.automation_control);
        guard.clone()
    };
    controller
        .set_enabled(&id, request.enabled)
        .map_err(|error| {
            if error.to_string().contains("not found") {
                ApiError::not_found(error.to_string())
            } else {
                ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
            }
        })?;

    let summary = controller
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("automation '{id}' not found")))?;
    Ok(Json(automation_response(&state, summary).await?))
}

async fn validate_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AutomationValidateResponse>, ApiError> {
    let controller = {
        let guard = read_lock(&state.automation_control);
        guard.clone()
    };
    let summary = controller.validate(&id).map_err(|error| {
        if error.to_string().contains("not found") {
            ApiError::not_found(error.to_string())
        } else {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
    })?;

    Ok(Json(AutomationValidateResponse {
        status: "ok",
        automation: automation_response(&state, summary).await?,
    }))
}

async fn execute_automation_manually(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ManualAutomationRequest>,
) -> Result<Json<AutomationExecuteResponse>, ApiError> {
    let trigger_payload = request.trigger_payload.unwrap_or_else(|| {
        smart_home_core::model::AttributeValue::Object(HashMap::from([(
            "type".to_string(),
            smart_home_core::model::AttributeValue::Text("manual".to_string()),
        )]))
    });

    let controller = {
        let guard = read_lock(&state.automation_control);
        guard.clone()
    };
    let execution = controller
        .execute(
            &id,
            state.runtime.clone(),
            trigger_payload,
            state.trigger_context,
        )
        .map_err(|error| {
            if error.to_string().contains("not found") {
                ApiError::not_found(error.to_string())
            } else {
                ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
            }
        })?;

    Ok(Json(AutomationExecuteResponse {
        status: execution.status,
        error: execution.error,
        duration_ms: execution.duration_ms,
        results: execution.results,
    }))
}

async fn execute_scene(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let executed_at = Utc::now();
    let runner = {
        let guard = read_lock(&state.scenes);
        guard.clone()
    };
    let outcome = runner
        .execute(&id, state.runtime.clone())
        .await
        .map_err(|error| {
            persist_scene_history(
                &state,
                SceneExecutionHistoryEntry {
                    executed_at,
                    scene_id: id.clone(),
                    status: "error".to_string(),
                    error: Some(error.to_string()),
                    results: Vec::new(),
                },
            );
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        })?;

    match outcome {
        SceneRunOutcome::NotFound => Err(ApiError::not_found(format!("scene '{id}' not found"))),
        SceneRunOutcome::Dropped => {
            persist_scene_history(
                &state,
                SceneExecutionHistoryEntry {
                    executed_at,
                    scene_id: id.clone(),
                    status: "skipped".to_string(),
                    error: Some("scene already running (execution mode saturated)".to_string()),
                    results: Vec::new(),
                },
            );
            Err(ApiError::new(
                StatusCode::LOCKED,
                format!("scene '{id}' is already running"),
            ))
        }
        SceneRunOutcome::Queued => {
            persist_scene_history(
                &state,
                SceneExecutionHistoryEntry {
                    executed_at,
                    scene_id: id.clone(),
                    status: "queued".to_string(),
                    error: None,
                    results: Vec::new(),
                },
            );
            Ok(StatusCode::ACCEPTED.into_response())
        }
        SceneRunOutcome::Completed(results) => {
            persist_scene_history(
                &state,
                SceneExecutionHistoryEntry {
                    executed_at,
                    scene_id: id.clone(),
                    status: "ok".to_string(),
                    error: None,
                    results: results
                        .iter()
                        .map(|r| SceneStepResult {
                            target: r.target.clone(),
                            status: r.status.to_string(),
                            message: r.message.clone(),
                        })
                        .collect(),
                },
            );
            Ok(Json(SceneExecuteResponse {
                status: "ok",
                results,
            })
            .into_response())
        }
    }
}

async fn get_scene_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<SceneHistoryResponse>, ApiError> {
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_scene_history(&id, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(SceneHistoryResponse {
        scene_id: id,
        entries,
    }))
}

async fn get_automation_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<AutomationHistoryResponse>, ApiError> {
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_automation_history(&id, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(AutomationHistoryResponse {
        automation_id: id,
        entries,
    }))
}

async fn list_rooms(State(state): State<AppState>) -> Json<Vec<Room>> {
    Json(state.runtime.registry().list_rooms())
}

async fn create_room(
    State(state): State<AppState>,
    Json(request): Json<CreateRoomRequest>,
) -> Result<Json<Room>, ApiError> {
    let id = request.id.trim();
    let name = request.name.trim();

    if id.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "room id must not be empty",
        ));
    }
    if name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "room name must not be empty",
        ));
    }

    let room = Room {
        id: RoomId(id.to_string()),
        name: name.to_string(),
    };
    state.runtime.registry().upsert_room(room.clone()).await;
    Ok(Json(room))
}

async fn get_room(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Room>, ApiError> {
    state
        .runtime
        .registry()
        .get_room(&RoomId(id.clone()))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("room '{id}' not found")))
}

async fn delete_room(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let room_id = RoomId(id.clone());
    if !state.runtime.registry().remove_room(&room_id).await {
        return Err(ApiError::not_found(format!("room '{id}' not found")));
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn list_room_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<smart_home_core::model::Device>>, ApiError> {
    let room_id = RoomId(id.clone());
    if state.runtime.registry().get_room(&room_id).is_none() {
        return Err(ApiError::not_found(format!("room '{id}' not found")));
    }

    Ok(Json(
        state.runtime.registry().list_devices_in_room(&room_id),
    ))
}

fn validate_group_payload(id: &str, name: &str, members: &[String]) -> Result<(), ApiError> {
    if id.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "group id must not be empty",
        ));
    }
    if name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "group name must not be empty",
        ));
    }
    for member in members {
        if member.trim().is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "group members must not contain empty device IDs",
            ));
        }
    }

    Ok(())
}

async fn list_groups(State(state): State<AppState>) -> Json<Vec<DeviceGroup>> {
    Json(state.runtime.registry().list_groups())
}

async fn create_group(
    State(state): State<AppState>,
    Json(request): Json<CreateGroupRequest>,
) -> Result<Json<DeviceGroup>, ApiError> {
    let id = request.id.trim();
    let name = request.name.trim();
    validate_group_payload(id, name, &request.members)?;

    let group = DeviceGroup {
        id: GroupId(id.to_string()),
        name: name.to_string(),
        members: request.members.into_iter().map(DeviceId).collect(),
    };

    state
        .runtime
        .registry()
        .upsert_group(group.clone())
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    Ok(Json(group))
}

async fn get_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DeviceGroup>, ApiError> {
    state
        .runtime
        .registry()
        .get_group(&GroupId(id.clone()))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("group '{id}' not found")))
}

async fn delete_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let group_id = GroupId(id.clone());
    if !state.runtime.registry().remove_group(&group_id).await {
        return Err(ApiError::not_found(format!("group '{id}' not found")));
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn set_group_members(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<SetGroupMembersRequest>,
) -> Result<Json<DeviceGroup>, ApiError> {
    let group_id = GroupId(id.clone());
    if state.runtime.registry().get_group(&group_id).is_none() {
        return Err(ApiError::not_found(format!("group '{id}' not found")));
    }

    state
        .runtime
        .registry()
        .set_group_members(
            &group_id,
            request.members.into_iter().map(DeviceId).collect(),
        )
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    state
        .runtime
        .registry()
        .get_group(&group_id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("group '{id}' not found")))
}

async fn list_group_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<smart_home_core::model::Device>>, ApiError> {
    let group_id = GroupId(id.clone());
    if state.runtime.registry().get_group(&group_id).is_none() {
        return Err(ApiError::not_found(format!("group '{id}' not found")));
    }

    Ok(Json(
        state.runtime.registry().list_devices_in_group(&group_id),
    ))
}

async fn list_devices(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> Json<Vec<smart_home_core::model::Device>> {
    let ids = requested_device_ids(raw_query.as_deref());
    if ids.is_empty() {
        return Json(state.runtime.registry().list());
    }

    let mut seen = std::collections::HashSet::new();
    let devices = ids
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .filter_map(|id| state.runtime.registry().get(&DeviceId(id)))
        .collect();

    Json(devices)
}

fn requested_device_ids(raw_query: Option<&str>) -> Vec<String> {
    raw_query
        .into_iter()
        .flat_map(|query| url::form_urlencoded::parse(query.as_bytes()))
        .filter_map(|(key, value)| (key == "ids").then(|| value.into_owned()))
        .collect()
}

async fn get_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<smart_home_core::model::Device>, ApiError> {
    state
        .runtime
        .registry()
        .get(&DeviceId(id.clone()))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("device '{id}' not found")))
}

async fn get_device_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<DeviceHistoryResponse>, ApiError> {
    let device_id = DeviceId(id.clone());
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    ensure_device_exists(&state, &device_id, &id)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_device_history(&device_id, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(DeviceHistoryResponse {
        device_id: id,
        entries,
    }))
}

async fn get_attribute_history(
    State(state): State<AppState>,
    Path((id, attribute)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<AttributeHistoryResponse>, ApiError> {
    let device_id = DeviceId(id.clone());
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    ensure_device_exists(&state, &device_id, &id)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_attribute_history(&device_id, &attribute, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(AttributeHistoryResponse {
        device_id: id,
        attribute,
        entries,
    }))
}

async fn assign_device_room(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<AssignRoomRequest>,
) -> Result<Json<smart_home_core::model::Device>, ApiError> {
    let device_id = DeviceId(id.clone());

    if state.runtime.registry().get(&device_id).is_none() {
        return Err(ApiError::not_found(format!("device '{id}' not found")));
    }

    let room_id = request.room_id.map(RoomId);
    state
        .runtime
        .registry()
        .assign_device_to_room(&device_id, room_id)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    state
        .runtime
        .registry()
        .get(&device_id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("device '{id}' not found")))
}

async fn command_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(command): Json<DeviceCommand>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let device_id = DeviceId(id.clone());

    if state.runtime.registry().get(&device_id).is_none() {
        return Err(ApiError::not_found(format!("device '{id}' not found")));
    }

    command
        .validate()
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    let command_to_run = command.clone();
    match state
        .runtime
        .command_device(&device_id, command_to_run)
        .await
    {
        Ok(true) => {
            persist_command_audit(
                &state,
                CommandAuditEntry {
                    recorded_at: Utc::now(),
                    source: "device".to_string(),
                    room_id: state
                        .runtime
                        .registry()
                        .get(&device_id)
                        .and_then(|device| device.room_id),
                    device_id: device_id.clone(),
                    command,
                    status: "ok".to_string(),
                    message: None,
                },
            );
            Ok(Json(json!({ "status": "ok" })))
        }
        Ok(false) => Err(ApiError::not_implemented(format!(
            "device commands are not implemented for '{id}'"
        ))),
        Err(error) => {
            persist_command_audit(
                &state,
                CommandAuditEntry {
                    recorded_at: Utc::now(),
                    source: "device".to_string(),
                    room_id: state
                        .runtime
                        .registry()
                        .get(&device_id)
                        .and_then(|device| device.room_id),
                    device_id: device_id.clone(),
                    command,
                    status: "error".to_string(),
                    message: Some(error.to_string()),
                },
            );
            Err(ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))
        }
    }
}

async fn command_room_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(command): Json<DeviceCommand>,
) -> Result<Json<Vec<RoomCommandResult>>, ApiError> {
    let room_id = RoomId(id.clone());
    if state.runtime.registry().get_room(&room_id).is_none() {
        return Err(ApiError::not_found(format!("room '{id}' not found")));
    }

    command
        .validate()
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    let devices = state.runtime.registry().list_devices_in_room(&room_id);
    let mut results = Vec::with_capacity(devices.len());

    for device in devices {
        let device_id = device.id.clone();
        let audit_command = command.clone();
        match state
            .runtime
            .command_device(&device_id, command.clone())
            .await
        {
            Ok(true) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "room".to_string(),
                        room_id: Some(room_id.clone()),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "ok".to_string(),
                        message: None,
                    },
                );
                results.push(RoomCommandResult {
                    device_id: device_id.0,
                    status: "ok",
                    message: None,
                });
            }
            Ok(false) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "room".to_string(),
                        room_id: Some(room_id.clone()),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "unsupported".to_string(),
                        message: Some("device commands are not implemented".to_string()),
                    },
                );
                results.push(RoomCommandResult {
                    device_id: device_id.0,
                    status: "unsupported",
                    message: Some("device commands are not implemented".to_string()),
                });
            }
            Err(error) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "room".to_string(),
                        room_id: Some(room_id.clone()),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "error".to_string(),
                        message: Some(error.to_string()),
                    },
                );
                results.push(RoomCommandResult {
                    device_id: device_id.0,
                    status: "error",
                    message: Some(error.to_string()),
                });
            }
        }
    }

    Ok(Json(results))
}

async fn command_group_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(command): Json<DeviceCommand>,
) -> Result<Json<Vec<GroupCommandResult>>, ApiError> {
    let group_id = GroupId(id.clone());
    if state.runtime.registry().get_group(&group_id).is_none() {
        return Err(ApiError::not_found(format!("group '{id}' not found")));
    }

    command
        .validate()
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    let devices = state.runtime.registry().list_devices_in_group(&group_id);
    let mut results = Vec::with_capacity(devices.len());

    for device in devices {
        let device_id = device.id.clone();
        let audit_command = command.clone();
        match state
            .runtime
            .command_device(&device_id, command.clone())
            .await
        {
            Ok(true) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "group".to_string(),
                        room_id: device.room_id.clone(),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "ok".to_string(),
                        message: None,
                    },
                );
                results.push(GroupCommandResult {
                    device_id: device_id.0,
                    status: "ok",
                    message: None,
                });
            }
            Ok(false) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "group".to_string(),
                        room_id: device.room_id.clone(),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "unsupported".to_string(),
                        message: Some("device commands are not implemented".to_string()),
                    },
                );
                results.push(GroupCommandResult {
                    device_id: device_id.0,
                    status: "unsupported",
                    message: Some("device commands are not implemented".to_string()),
                });
            }
            Err(error) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "group".to_string(),
                        room_id: device.room_id.clone(),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "error".to_string(),
                        message: Some(error.to_string()),
                    },
                );
                results.push(GroupCommandResult {
                    device_id: device_id.0,
                    status: "error",
                    message: Some(error.to_string()),
                });
            }
        }
    }

    Ok(Json(results))
}

async fn get_command_audit(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<CommandAuditResponse>, ApiError> {
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_command_audit(None, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(CommandAuditResponse { entries }))
}

async fn events(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_events_socket(socket, state.runtime))
}

fn history_store(state: &AppState) -> Result<Arc<dyn DeviceStore>, ApiError> {
    if !state.history.enabled {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "history is disabled"));
    }

    state.store.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "history is unavailable because persistence is disabled",
        )
    })
}

fn ensure_history_query_range(query: &HistoryQuery) -> Result<(), ApiError> {
    if let (Some(start), Some(end)) = (query.start, query.end) {
        if start > end {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "history query start must be <= end",
            ));
        }
    }

    Ok(())
}

fn ensure_device_exists(
    state: &AppState,
    device_id: &DeviceId,
    raw_id: &str,
) -> Result<(), ApiError> {
    if state.runtime.registry().get(device_id).is_none() {
        return Err(ApiError::not_found(format!("device '{raw_id}' not found")));
    }

    Ok(())
}

fn internal_api_error(error: anyhow::Error) -> ApiError {
    tracing::error!(error = %error, "internal API error");
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
}

fn capability_schema_response(schema: CapabilitySchema) -> CapabilitySchemaResponse {
    match schema {
        CapabilitySchema::Measurement => CapabilitySchemaResponse {
            kind: "measurement",
            values: Vec::new(),
        },
        CapabilitySchema::Accumulation => CapabilitySchemaResponse {
            kind: "accumulation",
            values: Vec::new(),
        },
        CapabilitySchema::Number => CapabilitySchemaResponse {
            kind: "number",
            values: Vec::new(),
        },
        CapabilitySchema::Integer => CapabilitySchemaResponse {
            kind: "integer",
            values: Vec::new(),
        },
        CapabilitySchema::String => CapabilitySchemaResponse {
            kind: "string",
            values: Vec::new(),
        },
        CapabilitySchema::IntegerOrString => CapabilitySchemaResponse {
            kind: "integer_or_string",
            values: Vec::new(),
        },
        CapabilitySchema::Boolean => CapabilitySchemaResponse {
            kind: "boolean",
            values: Vec::new(),
        },
        CapabilitySchema::Percentage => CapabilitySchemaResponse {
            kind: "percentage",
            values: Vec::new(),
        },
        CapabilitySchema::RgbColor => CapabilitySchemaResponse {
            kind: "rgb_color",
            values: Vec::new(),
        },
        CapabilitySchema::HexColor => CapabilitySchemaResponse {
            kind: "hex_color",
            values: Vec::new(),
        },
        CapabilitySchema::XyColor => CapabilitySchemaResponse {
            kind: "xy_color",
            values: Vec::new(),
        },
        CapabilitySchema::HsColor => CapabilitySchemaResponse {
            kind: "hs_color",
            values: Vec::new(),
        },
        CapabilitySchema::ColorTemperature => CapabilitySchemaResponse {
            kind: "color_temperature",
            values: Vec::new(),
        },
        CapabilitySchema::Enum(values) => CapabilitySchemaResponse {
            kind: "enum",
            values: values.to_vec(),
        },
    }
}

fn capability_ownership_response(policy: CapabilityOwnershipPolicy) -> CapabilityOwnershipResponse {
    CapabilityOwnershipResponse {
        canonical_attribute_location: policy.canonical_attribute_location,
        custom_attribute_prefix: policy.custom_attribute_prefix,
        vendor_metadata_field: policy.vendor_metadata_field,
        rules: policy.rules.to_vec(),
    }
}

async fn automation_response(
    state: &AppState,
    summary: smart_home_automations::AutomationSummary,
) -> Result<AutomationResponse, ApiError> {
    let automation_id = summary.id.clone();
    let latest_run = if state.history.enabled {
        if let Some(store) = &state.store {
            store
                .load_automation_history(&automation_id, None, None, 1)
                .await
                .map_err(internal_api_error)?
                .into_iter()
                .next()
        } else {
            None
        }
    } else {
        None
    };

    Ok(AutomationResponse {
        id: summary.id,
        name: summary.name,
        description: summary.description,
        trigger_type: summary.trigger_type,
        condition_count: summary.condition_count,
        status: if {
            let controller = read_lock(&state.automation_control);
            controller.is_enabled(&automation_id).unwrap_or(true)
        } {
            "enabled"
        } else {
            "disabled"
        },
        last_run: latest_run.as_ref().map(|entry| entry.executed_at),
        last_error: latest_run.and_then(|entry| entry.error),
    })
}

fn persist_command_audit(state: &AppState, entry: CommandAuditEntry) {
    let Some(store) = state.store.clone() else {
        return;
    };
    if !state.history.enabled {
        return;
    }

    tokio::spawn(async move {
        if let Err(error) = store.save_command_audit(&entry).await {
            tracing::error!(error = %error, device_id = %entry.device_id.0, "failed to persist command audit history");
        }
    });
}

fn persist_scene_history(state: &AppState, entry: SceneExecutionHistoryEntry) {
    let Some(store) = state.store.clone() else {
        return;
    };
    if !state.history.enabled {
        return;
    }

    tokio::spawn(async move {
        if let Err(error) = store.save_scene_execution(&entry).await {
            tracing::error!(error = %error, scene_id = %entry.scene_id, "failed to persist scene execution history");
        }
    });
}

impl AutomationExecutionObserver for StoreAutomationObserver {
    fn record(&self, entry: AutomationExecutionHistoryEntry) {
        let store = self.store.clone();
        tokio::spawn(async move {
            if let Err(error) = store.save_automation_execution(&entry).await {
                tracing::error!(error = %error, automation_id = %entry.automation_id, "failed to persist automation execution history");
            }
        });
    }
}

async fn handle_events_socket(mut socket: WebSocket, runtime: Arc<Runtime>) {
    let mut receiver = runtime.bus().subscribe();

    loop {
        let frame = match receiver.recv().await {
            Ok(Event::DeviceSeen { .. }) => continue,
            Ok(event) => event_to_frame(event),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                json!({
                    "type": "system.error",
                    "message": "subscriber lagged, events dropped"
                })
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        if socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

fn event_to_frame(event: Event) -> serde_json::Value {
    match event {
        Event::DeviceStateChanged { id, attributes, .. } => {
            json!({ "type": "device.state_changed", "id": id.0, "state": attributes })
        }
        Event::DeviceAdded { device } => {
            json!({ "type": "device.state_changed", "id": device.id.0, "state": device.attributes })
        }
        Event::DeviceRemoved { id } => json!({ "type": "device.removed", "id": id.0 }),
        Event::DeviceSeen { .. } => json!({ "type": "internal.device_seen" }),
        Event::RoomAdded { room } => {
            json!({ "type": "room.added", "room": room })
        }
        Event::RoomUpdated { room } => {
            json!({ "type": "room.updated", "room": room })
        }
        Event::RoomRemoved { id } => json!({ "type": "room.removed", "id": id.0 }),
        Event::GroupAdded { group } => {
            json!({ "type": "group.added", "group": group })
        }
        Event::GroupUpdated { group } => {
            json!({ "type": "group.updated", "group": group })
        }
        Event::GroupRemoved { id } => json!({ "type": "group.removed", "id": id.0 }),
        Event::GroupMembersChanged { id, members } => {
            json!({
                "type": "group.members_changed",
                "id": id.0,
                "members": members.into_iter().map(|member| member.0).collect::<Vec<_>>()
            })
        }
        Event::DeviceRoomChanged { id, room_id } => {
            json!({ "type": "device.room_changed", "id": id.0, "room_id": room_id.map(|room| room.0) })
        }
        Event::AdapterStarted { adapter } => {
            json!({ "type": "adapter.started", "adapter": adapter })
        }
        Event::SceneCatalogReloadStarted => {
            json!({ "type": "scene.catalog_reload_started", "target": "scenes" })
        }
        Event::SceneCatalogReloaded {
            loaded_count,
            duration_ms,
        } => {
            json!({
                "type": "scene.catalog_reloaded",
                "target": "scenes",
                "loaded_count": loaded_count,
                "duration_ms": duration_ms,
                "errors": []
            })
        }
        Event::SceneCatalogReloadFailed {
            duration_ms,
            errors,
        } => {
            json!({
                "type": "scene.catalog_reload_failed",
                "target": "scenes",
                "loaded_count": 0,
                "duration_ms": duration_ms,
                "errors": errors
            })
        }
        Event::AutomationCatalogReloadStarted => {
            json!({ "type": "automation.catalog_reload_started", "target": "automations" })
        }
        Event::AutomationCatalogReloaded {
            loaded_count,
            duration_ms,
        } => {
            json!({
                "type": "automation.catalog_reloaded",
                "target": "automations",
                "loaded_count": loaded_count,
                "duration_ms": duration_ms,
                "errors": []
            })
        }
        Event::AutomationCatalogReloadFailed {
            duration_ms,
            errors,
        } => {
            json!({
                "type": "automation.catalog_reload_failed",
                "target": "automations",
                "loaded_count": 0,
                "duration_ms": duration_ms,
                "errors": errors
            })
        }
        Event::ScriptsReloadStarted => {
            json!({ "type": "scripts.reload_started", "target": "scripts" })
        }
        Event::ScriptsReloaded {
            loaded_count,
            duration_ms,
        } => {
            json!({
                "type": "scripts.reloaded",
                "target": "scripts",
                "loaded_count": loaded_count,
                "duration_ms": duration_ms,
                "errors": []
            })
        }
        Event::ScriptsReloadFailed {
            duration_ms,
            errors,
        } => {
            json!({
                "type": "scripts.reload_failed",
                "target": "scripts",
                "loaded_count": 0,
                "duration_ms": duration_ms,
                "errors": errors
            })
        }
        Event::SystemError { message } => {
            json!({ "type": "system.error", "message": message })
        }
    }
}

fn config_path_from_args() -> Result<String> {
    let mut args = std::env::args().skip(1);

    match (args.next().as_deref(), args.next()) {
        (Some("--config"), Some(path)) => Ok(path),
        (None, _) => Ok("config/default.toml".to_string()),
        _ => Err(anyhow::anyhow!("usage: api [--config <path>]")),
    }
}

fn init_tracing(level: &str) -> Result<()> {
    let level = match level {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        other => return Err(anyhow::anyhow!("invalid logging.level '{other}'")),
    };

    tracing_subscriber::fmt().with_max_level(level).init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::fs;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use futures_util::StreamExt;
    use reqwest::StatusCode;
    use serde_json::Value;
    use smart_home_core::capability::{measurement_value, TEMPERATURE_OUTDOOR, WIND_SPEED};
    use smart_home_core::command::DeviceCommand;
    use smart_home_core::config::{Config, HistoryConfig, PersistenceBackend};
    use smart_home_core::model::{
        AttributeValue, Device, DeviceGroup, DeviceId, DeviceKind, GroupId, Metadata, Room, RoomId,
    };
    use smart_home_core::runtime::RuntimeConfig;
    use smart_home_core::store::AutomationRuntimeState;
    use smart_home_scenes::{SceneCatalog, SceneRunner};
    use store_sql::{SqliteDeviceStore, SqliteHistoryConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::{oneshot, Notify};
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;

    use super::*;

    struct MockResponse {
        status_line: &'static str,
        body: &'static str,
    }

    struct CommandAdapter;

    #[derive(Default)]
    struct LaggyMemoryStore {
        devices: Mutex<HashMap<DeviceId, Device>>,
        rooms: Mutex<HashMap<RoomId, Room>>,
        groups: Mutex<HashMap<GroupId, DeviceGroup>>,
        device_history: Mutex<HashMap<DeviceId, Vec<DeviceHistoryEntry>>>,
        attribute_history: Mutex<HashMap<(DeviceId, String), Vec<AttributeHistoryEntry>>>,
        command_audit: Mutex<Vec<CommandAuditEntry>>,
        scene_history: Mutex<HashMap<String, Vec<SceneExecutionHistoryEntry>>>,
        automation_history: Mutex<HashMap<String, Vec<AutomationExecutionHistoryEntry>>>,
        automation_runtime_state: Mutex<HashMap<String, AutomationRuntimeState>>,
        first_save_started: Notify,
        first_save_seen: Mutex<bool>,
        delay_first_save: Mutex<bool>,
    }

    #[derive(Default)]
    struct RecordingAutomationObserver {
        entries: Mutex<Vec<AutomationExecutionHistoryEntry>>,
    }

    impl AutomationExecutionObserver for RecordingAutomationObserver {
        fn record(&self, entry: AutomationExecutionHistoryEntry) {
            self.entries.lock().expect("observer lock").push(entry);
        }
    }

    impl LaggyMemoryStore {
        fn new() -> Self {
            Self {
                devices: Mutex::new(HashMap::new()),
                rooms: Mutex::new(HashMap::new()),
                groups: Mutex::new(HashMap::new()),
                device_history: Mutex::new(HashMap::new()),
                attribute_history: Mutex::new(HashMap::new()),
                command_audit: Mutex::new(Vec::new()),
                scene_history: Mutex::new(HashMap::new()),
                automation_history: Mutex::new(HashMap::new()),
                automation_runtime_state: Mutex::new(HashMap::new()),
                first_save_started: Notify::new(),
                first_save_seen: Mutex::new(false),
                delay_first_save: Mutex::new(true),
            }
        }

        async fn wait_for_first_save_started(&self) {
            if *self.first_save_seen.lock().expect("first save lock") {
                return;
            }

            self.first_save_started.notified().await;
        }
    }

    #[async_trait::async_trait]
    impl DeviceStore for LaggyMemoryStore {
        async fn load_all_devices(&self) -> anyhow::Result<Vec<Device>> {
            let mut devices = self
                .devices
                .lock()
                .expect("devices lock")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            devices.sort_by(|a, b| a.id.0.cmp(&b.id.0));
            Ok(devices)
        }

        async fn load_all_rooms(&self) -> anyhow::Result<Vec<Room>> {
            let mut rooms = self
                .rooms
                .lock()
                .expect("rooms lock")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            rooms.sort_by(|a, b| a.id.0.cmp(&b.id.0));
            Ok(rooms)
        }

        async fn load_all_groups(&self) -> anyhow::Result<Vec<DeviceGroup>> {
            let mut groups = self
                .groups
                .lock()
                .expect("groups lock")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            groups.sort_by(|a, b| a.id.0.cmp(&b.id.0));
            Ok(groups)
        }

        async fn save_device(&self, device: &Device) -> anyhow::Result<()> {
            let should_delay = {
                let mut seen = self.first_save_seen.lock().expect("first save lock");
                if !*seen {
                    *seen = true;
                    self.first_save_started.notify_waiters();
                }

                let mut delay = self.delay_first_save.lock().expect("delay lock");
                let should_delay = *delay;
                *delay = false;
                should_delay
            };

            if should_delay {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }

            self.devices
                .lock()
                .expect("devices lock")
                .insert(device.id.clone(), device.clone());

            self.device_history
                .lock()
                .expect("device history lock")
                .entry(device.id.clone())
                .or_default()
                .push(DeviceHistoryEntry {
                    observed_at: device.updated_at,
                    device: device.clone(),
                });

            let mut attribute_history = self
                .attribute_history
                .lock()
                .expect("attribute history lock");
            for (attribute, value) in &device.attributes {
                attribute_history
                    .entry((device.id.clone(), attribute.clone()))
                    .or_default()
                    .push(AttributeHistoryEntry {
                        observed_at: device.updated_at,
                        device_id: device.id.clone(),
                        attribute: attribute.clone(),
                        value: value.clone(),
                    });
            }

            Ok(())
        }

        async fn save_room(&self, room: &Room) -> anyhow::Result<()> {
            self.rooms
                .lock()
                .expect("rooms lock")
                .insert(room.id.clone(), room.clone());
            Ok(())
        }

        async fn save_group(&self, group: &DeviceGroup) -> anyhow::Result<()> {
            self.groups
                .lock()
                .expect("groups lock")
                .insert(group.id.clone(), group.clone());
            Ok(())
        }

        async fn delete_device(&self, id: &DeviceId) -> anyhow::Result<()> {
            self.devices.lock().expect("devices lock").remove(id);
            Ok(())
        }

        async fn delete_room(&self, id: &RoomId) -> anyhow::Result<()> {
            self.rooms.lock().expect("rooms lock").remove(id);
            Ok(())
        }

        async fn delete_group(&self, id: &GroupId) -> anyhow::Result<()> {
            self.groups.lock().expect("groups lock").remove(id);
            Ok(())
        }

        async fn load_device_history(
            &self,
            id: &DeviceId,
            start: Option<DateTime<Utc>>,
            end: Option<DateTime<Utc>>,
            limit: usize,
        ) -> anyhow::Result<Vec<DeviceHistoryEntry>> {
            let entries = self
                .device_history
                .lock()
                .expect("device history lock")
                .get(id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    start
                        .map(|value| entry.observed_at >= value)
                        .unwrap_or(true)
                })
                .filter(|entry| end.map(|value| entry.observed_at <= value).unwrap_or(true))
                .rev()
                .take(limit)
                .collect();
            Ok(entries)
        }

        async fn load_attribute_history(
            &self,
            id: &DeviceId,
            attribute: &str,
            start: Option<DateTime<Utc>>,
            end: Option<DateTime<Utc>>,
            limit: usize,
        ) -> anyhow::Result<Vec<AttributeHistoryEntry>> {
            let entries = self
                .attribute_history
                .lock()
                .expect("attribute history lock")
                .get(&(id.clone(), attribute.to_string()))
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    start
                        .map(|value| entry.observed_at >= value)
                        .unwrap_or(true)
                })
                .filter(|entry| end.map(|value| entry.observed_at <= value).unwrap_or(true))
                .rev()
                .take(limit)
                .collect();
            Ok(entries)
        }

        async fn save_command_audit(&self, entry: &CommandAuditEntry) -> anyhow::Result<()> {
            self.command_audit
                .lock()
                .expect("command audit lock")
                .push(entry.clone());
            Ok(())
        }

        async fn load_command_audit(
            &self,
            device_id: Option<&DeviceId>,
            start: Option<DateTime<Utc>>,
            end: Option<DateTime<Utc>>,
            limit: usize,
        ) -> anyhow::Result<Vec<CommandAuditEntry>> {
            let mut entries = self
                .command_audit
                .lock()
                .expect("command audit lock")
                .clone()
                .into_iter()
                .filter(|entry| {
                    device_id
                        .map(|value| &entry.device_id == value)
                        .unwrap_or(true)
                })
                .filter(|entry| {
                    start
                        .map(|value| entry.recorded_at >= value)
                        .unwrap_or(true)
                })
                .filter(|entry| end.map(|value| entry.recorded_at <= value).unwrap_or(true))
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| b.recorded_at.cmp(&a.recorded_at));
            entries.truncate(limit);
            Ok(entries)
        }

        async fn save_scene_execution(
            &self,
            entry: &SceneExecutionHistoryEntry,
        ) -> anyhow::Result<()> {
            self.scene_history
                .lock()
                .expect("scene history lock")
                .entry(entry.scene_id.clone())
                .or_default()
                .push(entry.clone());
            Ok(())
        }

        async fn load_scene_history(
            &self,
            scene_id: &str,
            start: Option<DateTime<Utc>>,
            end: Option<DateTime<Utc>>,
            limit: usize,
        ) -> anyhow::Result<Vec<SceneExecutionHistoryEntry>> {
            let mut entries = self
                .scene_history
                .lock()
                .expect("scene history lock")
                .get(scene_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    start
                        .map(|value| entry.executed_at >= value)
                        .unwrap_or(true)
                })
                .filter(|entry| end.map(|value| entry.executed_at <= value).unwrap_or(true))
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| b.executed_at.cmp(&a.executed_at));
            entries.truncate(limit);
            Ok(entries)
        }

        async fn save_automation_execution(
            &self,
            entry: &AutomationExecutionHistoryEntry,
        ) -> anyhow::Result<()> {
            self.automation_history
                .lock()
                .expect("automation history lock")
                .entry(entry.automation_id.clone())
                .or_default()
                .push(entry.clone());
            Ok(())
        }

        async fn load_automation_history(
            &self,
            automation_id: &str,
            start: Option<DateTime<Utc>>,
            end: Option<DateTime<Utc>>,
            limit: usize,
        ) -> anyhow::Result<Vec<AutomationExecutionHistoryEntry>> {
            let mut entries = self
                .automation_history
                .lock()
                .expect("automation history lock")
                .get(automation_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    start
                        .map(|value| entry.executed_at >= value)
                        .unwrap_or(true)
                })
                .filter(|entry| end.map(|value| entry.executed_at <= value).unwrap_or(true))
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| b.executed_at.cmp(&a.executed_at));
            entries.truncate(limit);
            Ok(entries)
        }

        async fn load_automation_runtime_state(
            &self,
            automation_id: &str,
        ) -> anyhow::Result<Option<AutomationRuntimeState>> {
            Ok(self
                .automation_runtime_state
                .lock()
                .expect("automation runtime state lock")
                .get(automation_id)
                .cloned())
        }

        async fn save_automation_runtime_state(
            &self,
            state: &AutomationRuntimeState,
        ) -> anyhow::Result<()> {
            self.automation_runtime_state
                .lock()
                .expect("automation runtime state lock")
                .insert(state.automation_id.clone(), state.clone());
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Adapter for CommandAdapter {
        fn name(&self) -> &str {
            "test"
        }

        async fn run(
            &self,
            _registry: smart_home_core::registry::DeviceRegistry,
            _bus: smart_home_core::bus::EventBus,
        ) -> Result<()> {
            std::future::pending::<()>().await;
            Ok(())
        }

        async fn command(
            &self,
            device_id: &DeviceId,
            command: DeviceCommand,
            registry: smart_home_core::registry::DeviceRegistry,
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

    struct MockServer {
        addr: SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock server");
            let addr = listener.local_addr().expect("get mock server address");
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

            let handle = tokio::spawn({
                let responses = Arc::clone(&responses);
                async move {
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            accept_result = listener.accept() => {
                                let (mut socket, _) = accept_result.expect("accept mock connection");
                                let responses = Arc::clone(&responses);

                                tokio::spawn(async move {
                                    let mut buffer = [0_u8; 2048];
                                    let _ = socket.read(&mut buffer).await;

                                    let response = responses
                                        .lock()
                                        .expect("mock response queue lock")
                                        .pop_front()
                                        .unwrap_or(MockResponse {
                                            status_line: "HTTP/1.1 500 Internal Server Error",
                                            body: "{\"error\":\"no queued response\"}",
                                        });

                                    let reply = format!(
                                        "{}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                        response.status_line,
                                        response.body.len(),
                                        response.body,
                                    );

                                    let _ = socket.write_all(reply.as_bytes()).await;
                                });
                            }
                        }
                    }
                }
            });

            Self {
                addr,
                shutdown: Some(shutdown_tx),
                handle,
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            self.handle.abort();
        }
    }

    fn sample_device(id: &str, attribute_name: &str, value: AttributeValue) -> Device {
        let mut attributes = HashMap::new();
        attributes.insert(attribute_name.to_string(), value);

        Device {
            id: DeviceId(id.to_string()),
            room_id: None,
            kind: DeviceKind::Sensor,
            attributes,
            metadata: Metadata {
                source: "test".to_string(),
                accuracy: Some(1.0),
                vendor_specific: HashMap::new(),
            },
            updated_at: chrono::Utc::now(),
            last_seen: chrono::Utc::now(),
        }
    }

    fn temp_sqlite_url() -> String {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("smart-home-api-{unique}.db"));
        format!("sqlite://{}", path.display())
    }

    fn test_config(adapters: serde_json::Map<String, serde_json::Value>) -> Config {
        Config {
            runtime: RuntimeConfig {
                event_bus_capacity: 16,
            },
            api: smart_home_core::config::ApiConfig {
                bind_address: "127.0.0.1:3001".to_string(),
                cors: smart_home_core::config::ApiCorsConfig::default(),
            },
            locale: smart_home_core::config::LocaleConfig::default(),
            logging: smart_home_core::config::LoggingConfig {
                level: "info".to_string(),
            },
            persistence: smart_home_core::config::PersistenceConfig {
                enabled: false,
                backend: PersistenceBackend::Sqlite,
                database_url: Some(temp_sqlite_url()),
                auto_create: true,
                history: HistoryConfig::default(),
            },
            scenes: smart_home_core::config::ScenesConfig::default(),
            automations: smart_home_core::config::AutomationsConfig::default(),
            scripts: smart_home_core::config::ScriptsConfig::default(),
            telemetry: smart_home_core::config::TelemetryConfig::default(),
            adapters: adapters.into_iter().collect(),
        }
    }

    fn empty_scenes() -> SceneRunner {
        SceneRunner::new(SceneCatalog::empty())
    }

    fn history_settings(enabled: bool) -> HistorySettings {
        HistorySettings {
            enabled,
            default_limit: 200,
            max_limit: 1000,
        }
    }

    async fn spawn_test_server_with_config(
        runtime: Arc<Runtime>,
        config: Config,
    ) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let scripts_root = config
            .scripts
            .enabled
            .then(|| PathBuf::from(&config.scripts.directory));
        let scenes = if config.scenes.enabled
            && std::path::Path::new(&config.scenes.directory).exists()
        {
            SceneRunner::new(
                SceneCatalog::load_from_directory(&config.scenes.directory, scripts_root.clone())
                    .expect("test scenes load"),
            )
        } else {
            empty_scenes()
        };
        let automations = if config.automations.enabled
            && std::path::Path::new(&config.automations.directory).exists()
        {
            Arc::new(
                AutomationCatalog::load_from_directory(
                    &config.automations.directory,
                    scripts_root.clone(),
                )
                .expect("test automations load"),
            )
        } else {
            Arc::new(AutomationCatalog::empty())
        };
        let app = app(
            make_state(
                runtime,
                scenes,
                automations.clone(),
                TriggerContext::default(),
                test_health(&["open_meteo"]),
                None,
                history_settings(false),
                config.scenes.enabled,
                config.automations.enabled,
                config.scenes.directory.clone(),
                config.automations.directory.clone(),
                scripts_root,
            ),
            &config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("test server exits cleanly");
        });

        (addr, shutdown_tx, handle)
    }

    #[test]
    fn telemetry_selection_only_applies_when_telemetry_is_enabled() {
        let mut config = test_config(serde_json::Map::new());
        config.telemetry.enabled = false;
        config.telemetry.selection.device_ids = vec!["test:device".to_string()];
        config.telemetry.selection.capabilities = vec!["brightness".to_string()];
        config.telemetry.selection.adapter_names = vec!["test".to_string()];

        let selection = history_selection_from_config(&config);
        assert!(selection.device_ids.is_empty());
        assert!(selection.capabilities.is_empty());
        assert!(selection.adapter_names.is_empty());

        config.telemetry.enabled = true;
        let selection = history_selection_from_config(&config);
        assert_eq!(selection.device_ids, vec!["test:device"]);
        assert_eq!(selection.capabilities, vec!["brightness"]);
        assert_eq!(selection.adapter_names, vec!["test"]);
    }

    fn test_health(adapter_names: &[&str]) -> HealthState {
        let health = HealthState::new(
            &adapter_names
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>(),
            false,
            false,
        );
        for adapter_name in adapter_names {
            health.adapter_started(adapter_name);
        }
        health.mark_startup_complete();
        health
    }

    async fn create_runtime_with_hydrated_store(device: Device) -> Arc<Runtime> {
        let database_url = temp_sqlite_url();
        let store = SqliteDeviceStore::new(&database_url, true)
            .await
            .expect("sqlite store initializes");
        if let Some(room_id) = &device.room_id {
            store
                .save_room(&Room {
                    id: room_id.clone(),
                    name: room_id.0.clone(),
                })
                .await
                .expect("seed room persists");
        }
        store
            .save_device(&device)
            .await
            .expect("seed device persists");

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let rooms = store
            .load_all_rooms()
            .await
            .expect("load hydrated rooms succeeds");
        let devices = store
            .load_all_devices()
            .await
            .expect("load hydrated devices succeeds");
        runtime.registry().restore_rooms(rooms);
        runtime
            .registry()
            .restore(devices)
            .expect("registry restore succeeds");

        runtime
    }

    async fn spawn_test_server(
        runtime: Arc<Runtime>,
    ) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let automations = Arc::new(AutomationCatalog::empty());
        let config = test_config(serde_json::Map::new());
        let app = app(
            make_state(
                runtime,
                empty_scenes(),
                automations.clone(),
                TriggerContext::default(),
                test_health(&["open_meteo"]),
                None,
                history_settings(false),
                config.scenes.enabled,
                config.automations.enabled,
                config.scenes.directory.clone(),
                config.automations.directory.clone(),
                config
                    .scripts
                    .enabled
                    .then(|| PathBuf::from(&config.scripts.directory)),
            ),
            &config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("test server exits cleanly");
        });

        (addr, shutdown_tx, handle)
    }

    async fn spawn_test_server_with_scenes(
        runtime: Arc<Runtime>,
        scenes: SceneRunner,
    ) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let automations = Arc::new(AutomationCatalog::empty());
        let config = test_config(serde_json::Map::new());
        let app = app(
            make_state(
                runtime,
                scenes,
                automations.clone(),
                TriggerContext::default(),
                test_health(&["open_meteo"]),
                None,
                history_settings(false),
                config.scenes.enabled,
                config.automations.enabled,
                config.scenes.directory.clone(),
                config.automations.directory.clone(),
                config
                    .scripts
                    .enabled
                    .then(|| PathBuf::from(&config.scripts.directory)),
            ),
            &config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("test server exits cleanly");
        });

        (addr, shutdown_tx, handle)
    }

    async fn spawn_test_server_with_store(
        runtime: Arc<Runtime>,
        store: Arc<dyn DeviceStore>,
        history_enabled: bool,
    ) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let automations = Arc::new(AutomationCatalog::empty());
        let config = test_config(serde_json::Map::new());
        let app = app(
            make_state(
                runtime,
                empty_scenes(),
                automations.clone(),
                TriggerContext::default(),
                test_health(&["open_meteo"]),
                Some(store),
                history_settings(history_enabled),
                config.scenes.enabled,
                config.automations.enabled,
                config.scenes.directory.clone(),
                config.automations.directory.clone(),
                config
                    .scripts
                    .enabled
                    .then(|| PathBuf::from(&config.scripts.directory)),
            ),
            &config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("test server exits cleanly");
        });

        (addr, shutdown_tx, handle)
    }

    fn write_temp_scene_dir(files: &[(&str, &str)]) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("smart-home-api-scenes-{unique}"));
        fs::create_dir_all(&path).expect("create temp scene dir");

        for (name, source) in files {
            fs::write(path.join(name), source).expect("write temp scene file");
        }

        path
    }

    fn write_temp_automation_dir(files: &[(&str, &str)]) -> std::path::PathBuf {
        write_temp_scene_dir(files)
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let response = reqwest::get(format!("http://{addr}/health"))
            .await
            .expect("health request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<Value>().await.expect("health JSON body");
        assert_eq!(body["status"], "ok");
        assert_eq!(body["ready"], true);
        assert_eq!(body["runtime"]["status"], "ok");

        let ready = reqwest::get(format!("http://{addr}/ready"))
            .await
            .expect("ready request succeeds");
        assert_eq!(ready.status(), StatusCode::OK);
        assert_eq!(
            ready.json::<Value>().await.expect("ready JSON body"),
            json!({ "status": "ok" })
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn monitor_runtime_health_marks_adapter_outage_as_degraded() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let health = HealthState::new(&["open_meteo".to_string()], false, false);
        health.adapter_started("open_meteo");
        health.mark_startup_complete();

        let monitor_task = {
            let runtime = runtime.clone();
            let health = health.clone();
            tokio::spawn(async move {
                monitor_runtime_health(runtime, health).await;
            })
        };

        tokio::task::yield_now().await;

        runtime.bus().publish(Event::SystemError {
            message: "open_meteo poll failed: timeout".to_string(),
        });

        timeout(Duration::from_secs(2), async {
            loop {
                let response = health.response();
                if response
                    .adapters
                    .iter()
                    .any(|adapter| adapter.name == "open_meteo" && adapter.status == "error")
                {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("adapter outage should degrade health");

        let response = health.response();
        assert_eq!(response.status, "degraded");
        assert!(!response.ready);
        assert_eq!(response.adapters[0].status, "error");
        assert_eq!(
            response.adapters[0].message.as_deref(),
            Some("open_meteo poll failed: timeout")
        );

        monitor_task.abort();
        let _ = monitor_task.await;
    }

    #[tokio::test]
    async fn devices_endpoint_returns_registry_contents() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(22.0, "celsius"),
            ))
            .await
            .expect("valid test device upsert succeeds");
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let response = reqwest::get(format!("http://{addr}/devices"))
            .await
            .expect("devices request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<Value>().await.expect("devices JSON body");
        assert_eq!(body.as_array().expect("devices array").len(), 1);
        assert_eq!(body[0]["id"], Value::String("test:device".to_string()));

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn cors_allows_configured_origin_on_regular_request() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let mut config = test_config(serde_json::Map::new());
        config.api.cors.enabled = true;
        config.api.cors.allowed_origins = vec!["http://127.0.0.1:8080".to_string()];
        let (addr, shutdown, handle) = spawn_test_server_with_config(runtime, config).await;

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/devices"))
            .header("Origin", "http://127.0.0.1:8080")
            .send()
            .await
            .expect("cors request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .expect("allow origin header present"),
            "http://127.0.0.1:8080"
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn cors_rejects_disallowed_origin_and_handles_preflight_for_allowed_origin() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let mut config = test_config(serde_json::Map::new());
        config.api.cors.enabled = true;
        config.api.cors.allowed_origins = vec!["http://127.0.0.1:8080".to_string()];
        let (addr, shutdown, handle) = spawn_test_server_with_config(runtime, config).await;

        let denied = reqwest::Client::new()
            .get(format!("http://{addr}/devices"))
            .header("Origin", "http://evil.example")
            .send()
            .await
            .expect("disallowed origin request succeeds");
        assert!(denied
            .headers()
            .get("access-control-allow-origin")
            .is_none());

        let preflight = reqwest::Client::new()
            .request(reqwest::Method::OPTIONS, format!("http://{addr}/devices"))
            .header("Origin", "http://127.0.0.1:8080")
            .header("Access-Control-Request-Method", "GET")
            .send()
            .await
            .expect("preflight request succeeds");

        assert_eq!(preflight.status(), StatusCode::OK);
        assert_eq!(
            preflight
                .headers()
                .get("access-control-allow-origin")
                .expect("preflight allow origin header present"),
            "http://127.0.0.1:8080"
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn devices_endpoint_can_filter_by_multiple_ids_in_request_order() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:first",
                TEMPERATURE_OUTDOOR,
                measurement_value(22.0, "celsius"),
            ))
            .await
            .expect("first test device upsert succeeds");
        runtime
            .registry()
            .upsert(sample_device(
                "test:second",
                WIND_SPEED,
                measurement_value(11.5, "km/h"),
            ))
            .await
            .expect("second test device upsert succeeds");
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let response = reqwest::get(format!(
            "http://{addr}/devices?ids=test:second&ids=test:first"
        ))
        .await
        .expect("filtered devices request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .json::<Value>()
            .await
            .expect("filtered devices JSON body");
        let ids: Vec<&str> = body
            .as_array()
            .expect("filtered devices array")
            .iter()
            .filter_map(|device| device["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["test:second", "test:first"]);

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn devices_endpoint_ignores_missing_ids_and_deduplicates_matches() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(22.0, "celsius"),
            ))
            .await
            .expect("valid test device upsert succeeds");
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let response = reqwest::get(format!(
            "http://{addr}/devices?ids=missing&ids=test:device&ids=test:device"
        ))
        .await
        .expect("filtered devices request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .json::<Value>()
            .await
            .expect("filtered devices JSON body");
        let devices = body.as_array().expect("filtered devices array");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0]["id"], Value::String("test:device".to_string()));

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn rooms_can_be_created_listed_and_assigned() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(22.0, "celsius"),
            ))
            .await
            .expect("valid test device upsert succeeds");
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let client = reqwest::Client::new();
        let create = client
            .post(format!("http://{addr}/rooms"))
            .json(&json!({ "id": "outside", "name": "Outside" }))
            .send()
            .await
            .expect("create room request succeeds");
        assert_eq!(create.status(), StatusCode::OK);

        let assign = client
            .post(format!("http://{addr}/devices/test:device/room"))
            .json(&json!({ "room_id": "outside" }))
            .send()
            .await
            .expect("assign room request succeeds");
        assert_eq!(assign.status(), StatusCode::OK);
        assert_eq!(
            assign.json::<Value>().await.expect("assigned device json")["room_id"],
            "outside"
        );

        let rooms = reqwest::get(format!("http://{addr}/rooms"))
            .await
            .expect("rooms request succeeds")
            .json::<Value>()
            .await
            .expect("rooms json body");
        assert_eq!(rooms.as_array().expect("rooms array").len(), 1);

        let room_devices = reqwest::get(format!("http://{addr}/rooms/outside/devices"))
            .await
            .expect("room devices request succeeds")
            .json::<Value>()
            .await
            .expect("room devices json body");
        assert_eq!(
            room_devices.as_array().expect("room devices array").len(),
            1
        );
        assert_eq!(room_devices[0]["id"], "test:device");

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn room_can_be_deleted_and_unassigns_devices() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert_room(Room {
                id: RoomId("outside".to_string()),
                name: "Outside".to_string(),
            })
            .await;
        let mut device = sample_device(
            "test:device",
            TEMPERATURE_OUTDOOR,
            measurement_value(22.0, "celsius"),
        );
        device.room_id = Some(RoomId("outside".to_string()));
        runtime
            .registry()
            .upsert(device)
            .await
            .expect("device exists");
        let (addr, shutdown, handle) = spawn_test_server(runtime.clone()).await;

        let response = reqwest::Client::new()
            .delete(format!("http://{addr}/rooms/outside"))
            .send()
            .await
            .expect("delete room request succeeds");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(runtime
            .registry()
            .get_room(&RoomId("outside".to_string()))
            .is_none());
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("device remains present")
                .room_id,
            None
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn room_command_fans_out_to_room_devices() {
        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert_room(Room {
                id: RoomId("outside".to_string()),
                name: "Outside".to_string(),
            })
            .await;

        let mut device_a = sample_device(
            "test:device",
            TEMPERATURE_OUTDOOR,
            measurement_value(20.0, "celsius"),
        );
        device_a.room_id = Some(RoomId("outside".to_string()));
        runtime
            .registry()
            .upsert(device_a)
            .await
            .expect("device a exists");

        let mut device_b = sample_device(
            "test:device-b",
            TEMPERATURE_OUTDOOR,
            measurement_value(21.0, "celsius"),
        );
        device_b.room_id = Some(RoomId("outside".to_string()));
        runtime
            .registry()
            .upsert(device_b)
            .await
            .expect("device b exists");

        let (addr, shutdown, handle) = spawn_test_server(runtime.clone()).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/rooms/outside/command"))
            .json(&json!({
                "capability": "brightness",
                "action": "set",
                "value": 42
            }))
            .send()
            .await
            .expect("room command request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .json::<Value>()
            .await
            .expect("room command json body");
        assert_eq!(
            body.as_array().expect("room command results array").len(),
            2
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn groups_can_be_created_listed_and_commanded() {
        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));

        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("device upsert succeeds");
        runtime
            .registry()
            .upsert(sample_device(
                "test:device-b",
                TEMPERATURE_OUTDOOR,
                measurement_value(21.0, "celsius"),
            ))
            .await
            .expect("device-b upsert succeeds");

        let (addr, shutdown, handle) = spawn_test_server(runtime).await;
        let client = reqwest::Client::new();

        let create = client
            .post(format!("http://{addr}/groups"))
            .json(&json!({
                "id": "bedroom_lamps",
                "name": "Bedroom Lamps",
                "members": ["test:device", "test:device-b"]
            }))
            .send()
            .await
            .expect("create group request succeeds");
        assert_eq!(create.status(), StatusCode::OK);

        let groups = reqwest::get(format!("http://{addr}/groups"))
            .await
            .expect("groups request succeeds")
            .json::<Value>()
            .await
            .expect("groups json body");
        assert_eq!(groups.as_array().expect("groups array").len(), 1);
        assert_eq!(groups[0]["id"], "bedroom_lamps");

        let command = client
            .post(format!("http://{addr}/groups/bedroom_lamps/command"))
            .json(&json!({
                "capability": "brightness",
                "action": "set",
                "value": 42
            }))
            .send()
            .await
            .expect("group command request succeeds");
        assert_eq!(command.status(), StatusCode::OK);

        let command_body = command
            .json::<Value>()
            .await
            .expect("group command json body");
        assert_eq!(
            command_body.as_array().expect("group results array").len(),
            2
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn group_members_can_be_updated() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("device upsert succeeds");
        runtime
            .registry()
            .upsert(sample_device(
                "test:device-b",
                TEMPERATURE_OUTDOOR,
                measurement_value(21.0, "celsius"),
            ))
            .await
            .expect("device-b upsert succeeds");
        runtime
            .registry()
            .upsert_group(DeviceGroup {
                id: GroupId("bedroom_lamps".to_string()),
                name: "Bedroom Lamps".to_string(),
                members: vec![DeviceId("test:device".to_string())],
            })
            .await
            .expect("group upsert succeeds");

        let (addr, shutdown, handle) = spawn_test_server(runtime).await;
        let client = reqwest::Client::new();

        let updated = client
            .post(format!("http://{addr}/groups/bedroom_lamps/members"))
            .json(&json!({"members": ["test:device-b"]}))
            .send()
            .await
            .expect("set group members request succeeds");

        assert_eq!(updated.status(), StatusCode::OK);
        let body = updated
            .json::<Value>()
            .await
            .expect("updated group json body");
        assert_eq!(body["members"][0], "test:device-b");

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn scenes_endpoint_lists_loaded_scenes() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let scene_dir = write_temp_scene_dir(&[(
            "video.lua",
            r#"return {
                id = "video",
                name = "Video",
                description = "Prepare devices for a video call",
                execute = function(ctx)
                end
            }"#,
        )]);
        let scenes = SceneRunner::new(
            SceneCatalog::load_from_directory(&scene_dir, None).expect("scenes load"),
        );
        let (addr, shutdown, handle) = spawn_test_server_with_scenes(runtime, scenes).await;

        let response = reqwest::get(format!("http://{addr}/scenes"))
            .await
            .expect("scenes request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<Value>().await.expect("scenes JSON body");
        assert_eq!(body.as_array().expect("scenes array").len(), 1);
        assert_eq!(body[0]["id"], "video");
        assert_eq!(body[0]["name"], "Video");

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn automations_endpoint_lists_loaded_automations() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let automation_dir = write_temp_automation_dir(&[(
            "rain.lua",
            r#"return {
                id = "rain_check",
                name = "Rain Check",
                description = "Respond to rain sensor changes",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                execute = function(ctx, event)
                end
            }"#,
        )]);
        let automations = Arc::new(
            AutomationCatalog::load_from_directory(&automation_dir, None)
                .expect("automations load"),
        );
        let automation_control =
            Arc::new(AutomationRunner::new((*automations).clone()).controller());
        let config = test_config(serde_json::Map::new());
        let (runner_tx, _runner_rx) = watch::channel(AutomationRunner::new((*automations).clone()));
        let app = app(
            AppState {
                runtime,
                scenes: Arc::new(RwLock::new(empty_scenes())),
                automations: Arc::new(RwLock::new(automations)),
                automation_control: Arc::new(RwLock::new(automation_control)),
                automation_runner_tx: runner_tx,
                automation_observer: None,
                trigger_context: TriggerContext::default(),
                health: test_health(&["open_meteo"]),
                store: None,
                history: history_settings(false),
                scenes_enabled: false,
                automations_enabled: true,
                scenes_directory: String::new(),
                automations_directory: automation_dir.to_string_lossy().to_string(),
                scenes_watch: false,
                automations_watch: false,
                scripts_watch: false,
                scripts_enabled: false,
                scripts_directory: String::new(),
            },
            &config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("test server exits cleanly");
        });

        let response = reqwest::get(format!("http://{addr}/automations"))
            .await
            .expect("automations request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .json::<Value>()
            .await
            .expect("automations json body");
        assert_eq!(body.as_array().expect("automations array").len(), 1);
        assert_eq!(body[0]["id"], "rain_check");
        assert_eq!(body[0]["trigger_type"], "device_state_change");
        assert_eq!(body[0]["condition_count"], 0);
        assert_eq!(body[0]["status"], "enabled");
        assert!(body[0]["last_run"].is_null());

        let detail = reqwest::get(format!("http://{addr}/automations/rain_check"))
            .await
            .expect("automation detail request succeeds");
        assert_eq!(detail.status(), StatusCode::OK);
        let detail_body = detail
            .json::<Value>()
            .await
            .expect("automation detail json body");
        assert_eq!(detail_body["id"], "rain_check");

        let _ = shutdown_tx.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn automation_control_endpoints_toggle_validate_and_execute() {
        let automation_dir = write_temp_automation_dir(&[(
            "rain.lua",
            r#"return {
                id = "rain_check",
                name = "Rain Check",
                description = "Respond to rain sensor changes",
                trigger = {
                    type = "device_state_change",
                    device_id = "test:rain",
                    attribute = "custom.test.rain",
                    equals = true,
                },
                conditions = {
                    {
                        type = "device_state",
                        device_id = "test:presence",
                        attribute = "occupancy",
                        equals = "occupied",
                    },
                },
                execute = function(ctx, event)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 42,
                    })
                end
            }"#,
        )]);
        let automations = Arc::new(
            AutomationCatalog::load_from_directory(&automation_dir, None)
                .expect("automations load"),
        );
        let observer = Arc::new(RecordingAutomationObserver::default())
            as Arc<dyn AutomationExecutionObserver>;
        let automation_runner =
            AutomationRunner::new((*automations).clone()).with_observer(observer.clone());
        let automation_control = Arc::new(automation_runner.controller());
        let (runner_tx, _runner_rx) = watch::channel(automation_runner.clone());

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:rain",
                "custom.test.rain",
                AttributeValue::Bool(false),
            ))
            .await
            .expect("sensor exists");
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("target exists");
        runtime
            .registry()
            .upsert(sample_device(
                "test:presence",
                "occupancy",
                AttributeValue::Text("unoccupied".to_string()),
            ))
            .await
            .expect("presence exists");

        let config = test_config(serde_json::Map::new());
        let app = app(
            AppState {
                runtime: runtime.clone(),
                scenes: Arc::new(RwLock::new(empty_scenes())),
                automations: Arc::new(RwLock::new(automations)),
                automation_control: Arc::new(RwLock::new(automation_control)),
                automation_runner_tx: runner_tx,
                automation_observer: Some(observer),
                trigger_context: TriggerContext::default(),
                health: test_health(&["open_meteo"]),
                store: None,
                history: history_settings(false),
                scenes_enabled: false,
                automations_enabled: true,
                scenes_directory: String::new(),
                automations_directory: automation_dir.to_string_lossy().to_string(),
                scenes_watch: false,
                automations_watch: false,
                scripts_watch: false,
                scripts_enabled: false,
                scripts_directory: String::new(),
            },
            &config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("test server exits cleanly");
        });

        let client = reqwest::Client::new();

        let disable = client
            .post(format!("http://{addr}/automations/rain_check/enabled"))
            .json(&json!({ "enabled": false }))
            .send()
            .await
            .expect("disable request succeeds");
        assert_eq!(disable.status(), StatusCode::OK);
        assert_eq!(
            disable.json::<Value>().await.expect("disable json body")["status"],
            "disabled"
        );

        let validate = client
            .post(format!("http://{addr}/automations/rain_check/validate"))
            .send()
            .await
            .expect("validate request succeeds");
        assert_eq!(validate.status(), StatusCode::OK);
        let validate_body = validate.json::<Value>().await.expect("validate json body");
        assert_eq!(validate_body["status"], "ok");
        assert_eq!(validate_body["automation"]["condition_count"], 1);

        let execute = client
            .post(format!("http://{addr}/automations/rain_check/execute"))
            .json(&json!({
                "trigger_payload": {
                    "type": "manual",
                    "source": "test"
                }
            }))
            .send()
            .await
            .expect("manual execute request succeeds");
        assert_eq!(execute.status(), StatusCode::OK);
        let execute_body = execute.json::<Value>().await.expect("execute json body");
        assert_eq!(execute_body["status"], "skipped");
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("updated device exists")
                .attributes
                .get("brightness"),
            None
        );

        runtime
            .registry()
            .upsert(sample_device(
                "test:presence",
                "occupancy",
                AttributeValue::Text("occupied".to_string()),
            ))
            .await
            .expect("presence update succeeds");

        let execute = client
            .post(format!("http://{addr}/automations/rain_check/execute"))
            .json(&json!({
                "trigger_payload": {
                    "type": "manual",
                    "source": "test"
                }
            }))
            .send()
            .await
            .expect("manual execute request succeeds");
        assert_eq!(execute.status(), StatusCode::OK);
        let execute_body = execute.json::<Value>().await.expect("execute json body");
        assert_eq!(execute_body["status"], "ok");
        assert_eq!(execute_body["results"][0]["target"], "test:device");

        let enable = client
            .post(format!("http://{addr}/automations/rain_check/enabled"))
            .json(&json!({ "enabled": true }))
            .send()
            .await
            .expect("enable request succeeds");
        assert_eq!(enable.status(), StatusCode::OK);
        assert_eq!(
            enable.json::<Value>().await.expect("enable json body")["status"],
            "enabled"
        );

        let _ = shutdown_tx.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn reload_endpoints_return_structured_status() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let client = reqwest::Client::new();

        let scene_reload = client
            .post(format!("http://{addr}/scenes/reload"))
            .send()
            .await
            .expect("scene reload request succeeds");
        assert_eq!(scene_reload.status(), StatusCode::OK);
        let scene_body = scene_reload
            .json::<Value>()
            .await
            .expect("scene reload json body");
        assert_eq!(scene_body["target"], "scenes");
        assert!(scene_body["status"] == "ok" || scene_body["status"] == "error");
        assert!(scene_body["duration_ms"].is_number());

        let automation_reload = client
            .post(format!("http://{addr}/automations/reload"))
            .send()
            .await
            .expect("automation reload request succeeds");
        assert_eq!(automation_reload.status(), StatusCode::OK);
        let automation_body = automation_reload
            .json::<Value>()
            .await
            .expect("automation reload json body");
        assert_eq!(automation_body["target"], "automations");
        assert!(automation_body["status"] == "ok" || automation_body["status"] == "error");
        assert!(automation_body["duration_ms"].is_number());

        let scripts_reload = client
            .post(format!("http://{addr}/scripts/reload"))
            .send()
            .await
            .expect("scripts reload request succeeds");
        assert_eq!(scripts_reload.status(), StatusCode::OK);
        let scripts_body = scripts_reload
            .json::<Value>()
            .await
            .expect("scripts reload json body");
        assert_eq!(scripts_body["target"], "scripts");
        assert!(scripts_body["status"] == "ok" || scripts_body["status"] == "error");
        assert!(scripts_body["duration_ms"].is_number());

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn capabilities_and_diagnostics_endpoints_expose_api_metadata() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(22.0, "celsius"),
            ))
            .await
            .expect("valid test device upsert succeeds");
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let capabilities = reqwest::get(format!("http://{addr}/capabilities"))
            .await
            .expect("capabilities request succeeds");
        assert_eq!(capabilities.status(), StatusCode::OK);
        let capability_body = capabilities
            .json::<Value>()
            .await
            .expect("capabilities json body");
        let entries = capability_body["capabilities"]
            .as_array()
            .expect("capabilities array");
        assert!(entries.iter().any(|entry| {
            entry["key"] == "brightness"
                && entry["domain"] == "lighting"
                && entry["schema"]["type"] == "percentage"
                && entry["actions"].as_array().is_some()
        }));
        assert!(entries.iter().any(|entry| {
            entry["key"] == "hvac_mode"
                && entry["domain"] == "climate"
                && entry["schema"]["type"] == "enum"
        }));
        assert_eq!(
            capability_body["ownership"]["canonical_attribute_location"],
            "device.attributes.<capability_key>"
        );
        assert_eq!(
            capability_body["ownership"]["custom_attribute_prefix"],
            "custom."
        );
        assert_eq!(
            capability_body["ownership"]["vendor_metadata_field"],
            "metadata.vendor_specific"
        );
        assert!(capability_body["ownership"]["rules"].is_array());

        let diagnostics = reqwest::get(format!("http://{addr}/diagnostics"))
            .await
            .expect("diagnostics request succeeds");
        assert_eq!(diagnostics.status(), StatusCode::OK);
        let diagnostics_body = diagnostics
            .json::<Value>()
            .await
            .expect("diagnostics json body");
        assert_eq!(diagnostics_body["ready"], true);
        assert_eq!(diagnostics_body["devices"], 1);
        assert_eq!(diagnostics_body["rooms"], 0);
        assert_eq!(diagnostics_body["automations"], 0);

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn adapter_detail_endpoint_reports_last_error() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let health = test_health(&["open_meteo"]);
        health.adapter_error("open_meteo", "open_meteo poll failed: timeout");

        let config = test_config(serde_json::Map::new());
        let (runner_tx, _runner_rx) =
            watch::channel(AutomationRunner::new(AutomationCatalog::empty()));
        let app = app(
            AppState {
                runtime,
                scenes: Arc::new(RwLock::new(empty_scenes())),
                automations: Arc::new(RwLock::new(Arc::new(AutomationCatalog::empty()))),
                automation_control: Arc::new(RwLock::new(Arc::new(
                    AutomationRunner::new(AutomationCatalog::empty()).controller(),
                ))),
                automation_runner_tx: runner_tx,
                automation_observer: None,
                trigger_context: TriggerContext::default(),
                health,
                store: None,
                history: history_settings(false),
                scenes_enabled: false,
                automations_enabled: false,
                scenes_directory: String::new(),
                automations_directory: String::new(),
                scenes_watch: false,
                automations_watch: false,
                scripts_watch: false,
                scripts_enabled: false,
                scripts_directory: String::new(),
            },
            &config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("test server exits cleanly");
        });

        let response = reqwest::get(format!("http://{addr}/adapters/open_meteo"))
            .await
            .expect("adapter detail request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .json::<Value>()
            .await
            .expect("adapter detail json body");
        assert_eq!(body["name"], "open_meteo");
        assert_eq!(body["runtime_status"], "error");
        assert_eq!(
            body["last_error"]["message"],
            "open_meteo poll failed: timeout"
        );

        let _ = shutdown_tx.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn reload_automations_preserves_enabled_state_for_same_id() {
        let automation_dir = write_temp_automation_dir(&[(
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
        )]);
        let mut config = test_config(serde_json::Map::new());
        config.automations.enabled = true;
        config.automations.directory = automation_dir.to_string_lossy().to_string();

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (addr, shutdown, handle) = spawn_test_server_with_config(runtime, config).await;

        let client = reqwest::Client::new();

        let disable = client
            .post(format!("http://{addr}/automations/rain_check/enabled"))
            .json(&json!({ "enabled": false }))
            .send()
            .await
            .expect("disable request succeeds");
        assert_eq!(disable.status(), StatusCode::OK);

        let reload = client
            .post(format!("http://{addr}/automations/reload"))
            .send()
            .await
            .expect("reload request succeeds");
        assert_eq!(reload.status(), StatusCode::OK);
        let reload_body = reload.json::<Value>().await.expect("reload json body");
        assert_eq!(reload_body["status"], "ok");

        let detail = client
            .get(format!("http://{addr}/automations/rain_check"))
            .send()
            .await
            .expect("detail request succeeds");
        assert_eq!(detail.status(), StatusCode::OK);
        let body = detail.json::<Value>().await.expect("detail json body");
        assert_eq!(body["status"], "disabled");

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_scene_endpoint_runs_scene_commands() {
        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("test device exists");

        let scene_dir = write_temp_scene_dir(&[(
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
        )]);
        let scenes = SceneRunner::new(
            SceneCatalog::load_from_directory(&scene_dir, None).expect("scenes load"),
        );
        let (addr, shutdown, handle) = spawn_test_server_with_scenes(runtime.clone(), scenes).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/scenes/set_brightness/execute"))
            .send()
            .await
            .expect("scene execute request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .json::<Value>()
            .await
            .expect("scene execute json body");
        assert_eq!(body["status"], "ok");
        assert_eq!(body["results"][0]["target"], "test:device");
        assert_eq!(body["results"][0]["status"], "ok");
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("updated device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(42))
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn devices_endpoint_returns_hydrated_persisted_devices() {
        let runtime = create_runtime_with_hydrated_store(sample_device(
            "test:hydrated",
            TEMPERATURE_OUTDOOR,
            measurement_value(19.0, "celsius"),
        ))
        .await;
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let response = reqwest::get(format!("http://{addr}/devices"))
            .await
            .expect("devices request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<Value>().await.expect("devices JSON body");
        assert_eq!(body.as_array().expect("devices array").len(), 1);
        assert_eq!(body[0]["id"], Value::String("test:hydrated".to_string()));

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn device_history_endpoint_returns_timeline_entries() {
        let database_url = temp_sqlite_url();
        let store = Arc::new(
            SqliteDeviceStore::new_with_history(
                &database_url,
                true,
                SqliteHistoryConfig {
                    enabled: true,
                    retention: None,
                    selection: HistorySelection::default(),
                },
            )
            .await
            .expect("sqlite store initializes"),
        );
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");
        let first_at = Utc::now() - chrono::Duration::minutes(2);
        let second_at = Utc::now() - chrono::Duration::minutes(1);
        let mut first = sample_device(
            "test:history",
            TEMPERATURE_OUTDOOR,
            measurement_value(20.0, "celsius"),
        );
        first.room_id = Some(RoomId("lab".to_string()));
        first.updated_at = first_at;
        first.last_seen = first_at;
        let mut second = first.clone();
        second.updated_at = second_at;
        second.last_seen = second_at;
        second.attributes.insert(
            TEMPERATURE_OUTDOOR.to_string(),
            measurement_value(21.5, "celsius"),
        );
        store
            .save_device(&first)
            .await
            .expect("first save succeeds");
        store
            .save_device(&second)
            .await
            .expect("second save succeeds");

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime.registry().restore_rooms(vec![Room {
            id: RoomId("lab".to_string()),
            name: "Lab".to_string(),
        }]);
        runtime
            .registry()
            .restore(vec![second.clone()])
            .expect("restore succeeds");

        let (addr, shutdown, handle) = spawn_test_server_with_store(runtime, store, true).await;

        let response = reqwest::get(format!(
            "http://{addr}/devices/test:history/history?limit=1"
        ))
        .await
        .expect("history request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<Value>().await.expect("history json body");
        assert_eq!(body["device_id"], "test:history");
        assert_eq!(body["entries"].as_array().expect("entries array").len(), 1);
        assert_eq!(
            body["entries"][0]["device"]["attributes"][TEMPERATURE_OUTDOOR]["value"],
            21.5
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn attribute_history_endpoint_filters_by_time_range() {
        let database_url = temp_sqlite_url();
        let store = Arc::new(
            SqliteDeviceStore::new_with_history(
                &database_url,
                true,
                SqliteHistoryConfig {
                    enabled: true,
                    retention: None,
                    selection: HistorySelection::default(),
                },
            )
            .await
            .expect("sqlite store initializes"),
        );
        let first_at = Utc::now() - chrono::Duration::minutes(3);
        let second_at = Utc::now() - chrono::Duration::minutes(1);
        let mut first = sample_device(
            "test:history",
            TEMPERATURE_OUTDOOR,
            measurement_value(20.0, "celsius"),
        );
        first.updated_at = first_at;
        first.last_seen = first_at;
        let mut second = first.clone();
        second.updated_at = second_at;
        second.last_seen = second_at;
        second.attributes.insert(
            TEMPERATURE_OUTDOOR.to_string(),
            measurement_value(22.0, "celsius"),
        );
        store
            .save_device(&first)
            .await
            .expect("first save succeeds");
        store
            .save_device(&second)
            .await
            .expect("second save succeeds");

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .restore(vec![second.clone()])
            .expect("restore succeeds");

        let (addr, shutdown, handle) = spawn_test_server_with_store(runtime, store, true).await;

        let response = reqwest::Client::new()
            .get(format!(
                "http://{addr}/devices/test:history/history/{TEMPERATURE_OUTDOOR}"
            ))
            .query(&[
                (
                    "start",
                    (Utc::now() - chrono::Duration::minutes(2)).to_rfc3339(),
                ),
                ("end", Utc::now().to_rfc3339()),
            ])
            .send()
            .await
            .expect("attribute history request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .json::<Value>()
            .await
            .expect("attribute history json body");
        assert_eq!(body["entries"].as_array().expect("entries array").len(), 1);
        assert_eq!(body["entries"][0]["value"]["value"], 22.0);

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn command_audit_endpoint_returns_recorded_commands() {
        let database_url = temp_sqlite_url();
        let store = Arc::new(
            SqliteDeviceStore::new_with_history(
                &database_url,
                true,
                SqliteHistoryConfig {
                    enabled: true,
                    retention: None,
                    selection: HistorySelection::default(),
                },
            )
            .await
            .expect("sqlite store initializes"),
        );
        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("test device exists");

        let (addr, shutdown, handle) =
            spawn_test_server_with_store(runtime.clone(), store.clone(), true).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/devices/test:device/command"))
            .json(&json!({
                "capability": "brightness",
                "action": "set",
                "value": 42
            }))
            .send()
            .await
            .expect("command request succeeds");
        assert_eq!(response.status(), StatusCode::OK);

        timeout(Duration::from_secs(2), async {
            loop {
                let entries = store
                    .load_command_audit(None, None, None, 10)
                    .await
                    .expect("command audit loads");
                if !entries.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("command audit persists in time");

        let audit = reqwest::get(format!("http://{addr}/audit/commands"))
            .await
            .expect("command audit request succeeds");
        assert_eq!(audit.status(), StatusCode::OK);
        let body = audit
            .json::<Value>()
            .await
            .expect("command audit json body");
        assert_eq!(body["entries"].as_array().expect("entries array").len(), 1);
        assert_eq!(body["entries"][0]["device_id"], "test:device");
        assert_eq!(body["entries"][0]["status"], "ok");

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scene_history_endpoint_returns_recorded_scene_runs() {
        let database_url = temp_sqlite_url();
        let store = Arc::new(
            SqliteDeviceStore::new_with_history(
                &database_url,
                true,
                SqliteHistoryConfig {
                    enabled: true,
                    retention: None,
                    selection: HistorySelection::default(),
                },
            )
            .await
            .expect("sqlite store initializes"),
        );
        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("test device exists");

        let scene_dir = write_temp_scene_dir(&[(
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
        )]);
        let scenes = SceneRunner::new(
            SceneCatalog::load_from_directory(&scene_dir, None).expect("scenes load"),
        );

        let config = test_config(serde_json::Map::new());
        let (runner_tx, _runner_rx) =
            watch::channel(AutomationRunner::new(AutomationCatalog::empty()));
        let app = app(
            AppState {
                runtime: runtime.clone(),
                scenes: Arc::new(RwLock::new(scenes)),
                automations: Arc::new(RwLock::new(Arc::new(AutomationCatalog::empty()))),
                automation_control: Arc::new(RwLock::new(Arc::new(
                    AutomationRunner::new(AutomationCatalog::empty()).controller(),
                ))),
                automation_runner_tx: runner_tx,
                automation_observer: None,
                trigger_context: TriggerContext::default(),
                health: test_health(&["open_meteo"]),
                store: Some(store.clone()),
                history: history_settings(true),
                scenes_enabled: true,
                automations_enabled: false,
                scenes_directory: scene_dir.to_string_lossy().to_string(),
                automations_directory: String::new(),
                scenes_watch: false,
                automations_watch: false,
                scripts_watch: false,
                scripts_enabled: false,
                scripts_directory: String::new(),
            },
            &config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("test server exits cleanly");
        });

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/scenes/set_brightness/execute"))
            .send()
            .await
            .expect("scene execute request succeeds");
        assert_eq!(response.status(), StatusCode::OK);

        timeout(Duration::from_secs(2), async {
            loop {
                let entries = store
                    .load_scene_history("set_brightness", None, None, 10)
                    .await
                    .expect("scene history loads");
                if !entries.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scene history persists in time");

        let history = reqwest::get(format!("http://{addr}/scenes/set_brightness/history"))
            .await
            .expect("scene history request succeeds");
        assert_eq!(history.status(), StatusCode::OK);
        let body = history
            .json::<Value>()
            .await
            .expect("scene history json body");
        assert_eq!(body["scene_id"], "set_brightness");
        assert_eq!(body["entries"].as_array().expect("entries array").len(), 1);
        assert_eq!(body["entries"][0]["status"], "ok");
        assert_eq!(body["entries"][0]["results"][0]["target"], "test:device");

        let _ = shutdown_tx.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn automation_history_endpoint_returns_recorded_automation_runs() {
        let database_url = temp_sqlite_url();
        let store = Arc::new(
            SqliteDeviceStore::new_with_history(
                &database_url,
                true,
                SqliteHistoryConfig {
                    enabled: true,
                    retention: None,
                    selection: HistorySelection::default(),
                },
            )
            .await
            .expect("sqlite store initializes"),
        );

        let automation_dir = write_temp_scene_dir(&[(
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
        )]);
        let automations = Arc::new(
            AutomationCatalog::load_from_directory(&automation_dir, None)
                .expect("automations load"),
        );

        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 32,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:rain",
                "custom.test.rain",
                AttributeValue::Bool(false),
            ))
            .await
            .expect("sensor exists");
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("target exists");

        let observer = Arc::new(StoreAutomationObserver {
            store: store.clone(),
        }) as Arc<dyn AutomationExecutionObserver>;
        let runner = AutomationRunner::new((*automations).clone()).with_observer(observer);
        let runtime_for_runner = runtime.clone();
        let automation_task = tokio::spawn(async move {
            runner.run(runtime_for_runner).await;
        });

        let (addr, shutdown, handle) =
            spawn_test_server_with_store(runtime.clone(), store.clone(), true).await;

        tokio::time::sleep(Duration::from_millis(25)).await;
        runtime
            .registry()
            .upsert(sample_device(
                "test:rain",
                "custom.test.rain",
                AttributeValue::Bool(true),
            ))
            .await
            .expect("sensor update succeeds");

        timeout(Duration::from_secs(2), async {
            loop {
                let entries = store
                    .load_automation_history("rain_check", None, None, 10)
                    .await
                    .expect("automation history loads");
                if !entries.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("automation history persists in time");

        let response = reqwest::get(format!("http://{addr}/automations/rain_check/history"))
            .await
            .expect("automation history request succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .json::<Value>()
            .await
            .expect("automation history json body");
        assert_eq!(body["automation_id"], "rain_check");
        assert_eq!(body["entries"].as_array().expect("entries array").len(), 1);
        assert_eq!(body["entries"][0]["status"], "ok");
        assert_eq!(body["entries"][0]["results"][0]["target"], "test:device");

        automation_task.abort();
        let _ = automation_task.await;
        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn history_endpoints_return_not_found_when_disabled() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(22.0, "celsius"),
            ))
            .await
            .expect("valid test device upsert succeeds");
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let response = reqwest::get(format!("http://{addr}/devices/test:device/history"))
            .await
            .expect("history request succeeds");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.json::<Value>().await.expect("error json body")["error"],
            "history is disabled"
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn missing_device_returns_404() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let response = reqwest::get(format!("http://{addr}/devices/nonexistent"))
            .await
            .expect("device request succeeds");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.json::<Value>().await.expect("error JSON body")["error"],
            "device 'nonexistent' not found"
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn command_endpoint_validates_and_dispatches_typed_commands() {
        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("test device exists");
        let (addr, shutdown, handle) = spawn_test_server(runtime.clone()).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/devices/test:device/command"))
            .json(&json!({
                "capability": "brightness",
                "action": "set",
                "value": 42
            }))
            .send()
            .await
            .expect("command request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("updated device exists")
                .attributes
                .get("brightness"),
            Some(&smart_home_core::model::AttributeValue::Integer(42))
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn command_endpoint_rejects_invalid_typed_command() {
        let runtime = Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("test device exists");
        let (addr, shutdown, handle) = spawn_test_server(runtime).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/devices/test:device/command"))
            .json(&json!({
                "capability": "brightness",
                "action": "set"
            }))
            .send()
            .await
            .expect("invalid command request succeeds");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<Value>().await.expect("error response json")["error"],
            "command action 'set' for capability 'brightness' requires a value"
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn websocket_streams_device_state_changed_event() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("valid test device upsert succeeds");
        let (addr, shutdown, handle) = spawn_test_server(runtime.clone()).await;

        let (mut socket, _) = connect_async(format!("ws://{addr}/events"))
            .await
            .expect("websocket connects");

        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(21.0, "celsius"),
            ))
            .await
            .expect("valid test device update succeeds");

        let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("websocket event arrives in time")
            .expect("websocket stream yields a message")
            .expect("websocket message is valid");

        let payload: Value = serde_json::from_str(message.to_text().expect("text websocket frame"))
            .expect("valid websocket JSON frame");

        assert_eq!(payload["type"], "device.state_changed");
        assert_eq!(payload["id"], "test:device");

        drop(socket);
        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn websocket_emits_scene_reload_lifecycle_events() {
        let scene_dir = write_temp_scene_dir(&[(
            "video.lua",
            r#"return {
                id = "video",
                name = "Video",
                execute = function(ctx)
                end
            }"#,
        )]);
        let mut config = test_config(serde_json::Map::new());
        config.scenes.enabled = true;
        config.scenes.directory = scene_dir.to_string_lossy().to_string();

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (addr, shutdown, handle) = spawn_test_server_with_config(runtime, config).await;

        let (mut socket, _) = connect_async(format!("ws://{addr}/events"))
            .await
            .expect("websocket connects");

        let reload = reqwest::Client::new()
            .post(format!("http://{addr}/scenes/reload"))
            .send()
            .await
            .expect("reload request succeeds");
        assert_eq!(reload.status(), StatusCode::OK);

        let first = timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("first reload websocket event arrives")
            .expect("websocket yields first message")
            .expect("first websocket message valid");
        let first_payload: Value = serde_json::from_str(first.to_text().expect("text frame"))
            .expect("valid first websocket JSON");
        assert_eq!(first_payload["type"], "scene.catalog_reload_started");

        let second = timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("second reload websocket event arrives")
            .expect("websocket yields second message")
            .expect("second websocket message valid");
        let second_payload: Value = serde_json::from_str(second.to_text().expect("text frame"))
            .expect("valid second websocket JSON");
        assert_eq!(second_payload["type"], "scene.catalog_reloaded");
        assert_eq!(second_payload["target"], "scenes");

        drop(socket);
        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn websocket_emits_scripts_reload_lifecycle_events() {
        let scripts_dir = std::env::temp_dir().join(format!(
            "smart-home-api-scripts-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&scripts_dir).expect("scripts dir created");
        std::fs::write(scripts_dir.join("helpers.lua"), "return { ok = true }")
            .expect("script file created");

        let mut config = test_config(serde_json::Map::new());
        config.scripts.enabled = true;
        config.scripts.directory = scripts_dir.to_string_lossy().to_string();

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (addr, shutdown, handle) = spawn_test_server_with_config(runtime, config).await;

        let (mut socket, _) = connect_async(format!("ws://{addr}/events"))
            .await
            .expect("websocket connects");

        let reload = reqwest::Client::new()
            .post(format!("http://{addr}/scripts/reload"))
            .send()
            .await
            .expect("scripts reload request succeeds");
        assert_eq!(reload.status(), StatusCode::OK);

        let first = timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("first scripts reload event arrives")
            .expect("websocket yields first message")
            .expect("first websocket message valid");
        let first_payload: Value = serde_json::from_str(first.to_text().expect("text frame"))
            .expect("valid first websocket JSON");
        assert_eq!(first_payload["type"], "scripts.reload_started");

        let second = timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("second scripts reload event arrives")
            .expect("websocket yields second message")
            .expect("second websocket message valid");
        let second_payload: Value = serde_json::from_str(second.to_text().expect("text frame"))
            .expect("valid second websocket JSON");
        assert_eq!(second_payload["type"], "scripts.reloaded");
        assert_eq!(second_payload["target"], "scripts");

        drop(socket);
        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn websocket_disconnect_does_not_crash_server() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (addr, shutdown, handle) = spawn_test_server(runtime.clone()).await;

        let (socket, _) = connect_async(format!("ws://{addr}/events"))
            .await
            .expect("websocket connects");
        drop(socket);

        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(22.0, "celsius"),
            ))
            .await
            .expect("valid test device upsert succeeds");

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished());

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
    }

    #[tokio::test]
    async fn end_to_end_runtime_http_and_websocket_flow() {
        let server = MockServer::start(vec![MockResponse {
            status_line: "HTTP/1.1 200 OK",
            body: "{\"current_weather\":{\"temperature\":18.25,\"windspeed\":11.5,\"winddirection\":225.0}}",
        }])
        .await;

        let config =
            Config::load_from_file("/home/andy/projects/rust_home/smart-home/config/default.toml")
                .expect("default config loads successfully");
        let mut adapter_config = config
            .adapters
            .get("open_meteo")
            .expect("open_meteo config exists")
            .as_object()
            .expect("open_meteo config is an object")
            .clone();
        adapter_config.insert("base_url".to_string(), Value::String(server.base_url()));
        adapter_config.insert("poll_interval_secs".to_string(), Value::from(60));
        adapter_config.insert("test_poll_interval_ms".to_string(), Value::from(25));
        let config = test_config(serde_json::Map::from_iter([(
            "open_meteo".to_string(),
            Value::Object(adapter_config),
        )]));
        let (mut adapters, _) =
            build_adapters(&config).expect("factory-based adapter build succeeds");
        let adapter = adapters.pop().expect("open_meteo adapter built");
        let runtime = Arc::new(Runtime::new(vec![adapter], config.runtime));
        let runtime_task = {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                runtime.run().await;
            })
        };
        let (addr, shutdown, handle) = spawn_test_server(runtime.clone()).await;

        timeout(Duration::from_secs(5), async {
            loop {
                if runtime.registry().list().len() >= 3 {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("devices appear in registry within timeout");

        let devices = reqwest::get(format!("http://{addr}/devices"))
            .await
            .expect("devices request succeeds")
            .json::<Value>()
            .await
            .expect("devices JSON body");

        let device_ids: Vec<&str> = devices
            .as_array()
            .expect("devices array")
            .iter()
            .filter_map(|device| device["id"].as_str())
            .collect();

        assert!(!device_ids.contains(&"open_meteo:temperature"));
        assert!(device_ids.contains(&"open_meteo:temperature_outdoor"));
        assert!(device_ids.contains(&"open_meteo:wind_speed"));
        assert!(device_ids.contains(&"open_meteo:wind_direction"));

        let (mut socket, _) = connect_async(format!("ws://{addr}/events"))
            .await
            .expect("websocket connects");

        runtime
            .registry()
            .upsert(sample_device(
                "test:device",
                TEMPERATURE_OUTDOOR,
                measurement_value(30.0, "celsius"),
            ))
            .await
            .expect("valid test device upsert succeeds");

        let payload = timeout(Duration::from_secs(10), async {
            loop {
                let message = socket
                    .next()
                    .await
                    .expect("websocket stream yields a message")
                    .expect("websocket message is valid");
                let payload: Value =
                    serde_json::from_str(message.to_text().expect("text websocket frame"))
                        .expect("valid websocket JSON frame");

                if payload["type"] == "device.state_changed" && payload["id"] == "test:device" {
                    break payload;
                }
            }
        })
        .await
        .expect("expected websocket device state event arrives in time");

        assert_eq!(payload["type"], "device.state_changed");

        drop(socket);
        let _ = shutdown.send(());
        handle.await.expect("server task completes");
        runtime_task.abort();
        let _ = runtime_task.await;
    }

    #[tokio::test]
    async fn persistence_worker_saves_and_restores_device_state_across_restart() {
        let database_url = temp_sqlite_url();
        let store = Arc::new(
            SqliteDeviceStore::new(&database_url, true)
                .await
                .expect("sqlite store initializes"),
        );

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let persistence_task = {
            let runtime = runtime.clone();
            let store: Arc<dyn DeviceStore> = store.clone();
            let health = HealthState::new(&[], true, false);
            tokio::spawn(async move {
                run_persistence_worker(runtime, store, health, shutdown_rx).await;
            })
        };
        tokio::task::yield_now().await;

        let device = sample_device(
            "test:restart",
            TEMPERATURE_OUTDOOR,
            measurement_value(24.0, "celsius"),
        );
        runtime
            .registry()
            .upsert(device.clone())
            .await
            .expect("device upsert succeeds");

        timeout(Duration::from_secs(2), async {
            loop {
                let devices = store.load_all_devices().await.expect("load succeeds");
                if devices == vec![device.clone()] {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("device persists within timeout");

        let restarted_runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let rooms = store
            .load_all_rooms()
            .await
            .expect("restored rooms load succeeds");
        let restored = store
            .load_all_devices()
            .await
            .expect("restored load succeeds");
        restarted_runtime.registry().restore_rooms(rooms);
        restarted_runtime
            .registry()
            .restore(restored)
            .expect("restore succeeds");

        assert_eq!(restarted_runtime.registry().list(), vec![device]);

        let _ = shutdown_tx.send(());
        let _ = persistence_task.await;
    }

    #[tokio::test]
    async fn persistence_worker_flushes_current_registry_state_on_shutdown() {
        let database_url = temp_sqlite_url();
        let store = Arc::new(
            SqliteDeviceStore::new(&database_url, true)
                .await
                .expect("sqlite store initializes"),
        );

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let persistence_task = {
            let runtime = runtime.clone();
            let store: Arc<dyn DeviceStore> = store.clone();
            let health = HealthState::new(&[], true, false);
            tokio::spawn(async move {
                run_persistence_worker(runtime, store, health, shutdown_rx).await;
            })
        };
        tokio::task::yield_now().await;

        let room = Room {
            id: RoomId("living_room".to_string()),
            name: "Living Room".to_string(),
        };
        runtime.registry().upsert_room(room.clone()).await;

        let mut device = sample_device(
            "test:shutdown",
            TEMPERATURE_OUTDOOR,
            measurement_value(19.5, "celsius"),
        );
        device.room_id = Some(RoomId("living_room".to_string()));
        runtime
            .registry()
            .upsert(device.clone())
            .await
            .expect("device upsert succeeds");

        let _ = shutdown_tx.send(());
        let _ = persistence_task.await;

        assert_eq!(
            store.load_all_rooms().await.expect("rooms load"),
            vec![room]
        );
        assert_eq!(
            store.load_all_devices().await.expect("devices load"),
            vec![device]
        );
    }

    #[tokio::test]
    async fn persistence_worker_recovers_from_lag_by_reconciling_latest_registry_state() {
        let store = Arc::new(LaggyMemoryStore::new());
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 1,
            },
        ));
        let health = HealthState::new(&[], true, false);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let persistence_task = {
            let runtime = runtime.clone();
            let store: Arc<dyn DeviceStore> = store.clone();
            let health = health.clone();
            tokio::spawn(async move {
                run_persistence_worker(runtime, store, health, shutdown_rx).await;
            })
        };
        tokio::task::yield_now().await;

        runtime
            .registry()
            .upsert(sample_device(
                "test:lagged",
                TEMPERATURE_OUTDOOR,
                measurement_value(20.0, "celsius"),
            ))
            .await
            .expect("initial device upsert succeeds");
        store.wait_for_first_save_started().await;

        runtime
            .registry()
            .upsert(sample_device(
                "test:lagged",
                TEMPERATURE_OUTDOOR,
                measurement_value(21.0, "celsius"),
            ))
            .await
            .expect("second device upsert succeeds");
        let latest = sample_device(
            "test:lagged",
            TEMPERATURE_OUTDOOR,
            measurement_value(22.0, "celsius"),
        );
        runtime
            .registry()
            .upsert(latest.clone())
            .await
            .expect("third device upsert succeeds");

        timeout(Duration::from_secs(2), async {
            loop {
                if store.load_all_devices().await.expect("load succeeds") == vec![latest.clone()] {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("lag recovery should reconcile latest registry state");

        assert_eq!(health.response().persistence.status, "ok");

        let _ = shutdown_tx.send(());
        let _ = persistence_task.await;
    }

    #[tokio::test]
    async fn reconcile_device_store_removes_stale_rows_and_saves_current_devices() {
        let database_url = temp_sqlite_url();
        let store = Arc::new(
            SqliteDeviceStore::new(&database_url, true)
                .await
                .expect("sqlite store initializes"),
        );
        let stale = sample_device(
            "test:stale",
            TEMPERATURE_OUTDOOR,
            measurement_value(10.0, "celsius"),
        );
        let current = sample_device(
            "test:current",
            TEMPERATURE_OUTDOOR,
            measurement_value(25.0, "celsius"),
        );
        store
            .save_device(&stale)
            .await
            .expect("seed stale device succeeds");

        let store_trait: Arc<dyn DeviceStore> = store.clone();
        reconcile_device_store(Vec::new(), Vec::new(), vec![current.clone()], store_trait)
            .await
            .expect("reconciliation succeeds");

        assert_eq!(
            store.load_all_devices().await.expect("load succeeds"),
            vec![current]
        );
    }

    #[test]
    fn create_device_store_rejects_unimplemented_postgres_backend() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime creates");
        let config = Config {
            runtime: RuntimeConfig {
                event_bus_capacity: 16,
            },
            api: smart_home_core::config::ApiConfig {
                bind_address: "127.0.0.1:3001".to_string(),
                cors: smart_home_core::config::ApiCorsConfig::default(),
            },
            locale: smart_home_core::config::LocaleConfig::default(),
            logging: smart_home_core::config::LoggingConfig {
                level: "info".to_string(),
            },
            persistence: smart_home_core::config::PersistenceConfig {
                enabled: true,
                backend: PersistenceBackend::Postgres,
                database_url: Some("postgres://localhost/smart-home".to_string()),
                auto_create: true,
                history: HistoryConfig::default(),
            },
            scenes: smart_home_core::config::ScenesConfig::default(),
            automations: smart_home_core::config::AutomationsConfig::default(),
            scripts: smart_home_core::config::ScriptsConfig::default(),
            telemetry: smart_home_core::config::TelemetryConfig::default(),
            adapters: HashMap::new(),
        };

        let error = runtime
            .block_on(create_device_store(&config))
            .err()
            .expect("postgres backend should not be implemented yet");

        assert_eq!(
            error.to_string(),
            "persistence backend 'postgres' is not implemented yet"
        );
    }

    #[test]
    fn temp_sqlite_url_points_to_temp_dir() {
        let url = temp_sqlite_url();
        assert!(url.starts_with("sqlite://"));

        if let Some(path) = url.strip_prefix("sqlite://") {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn build_adapters_uses_registered_factories() {
        let config = test_config(serde_json::Map::from_iter([(
            "open_meteo".to_string(),
            serde_json::json!({
                "enabled": true,
                "latitude": 51.5,
                "longitude": -0.1,
                "poll_interval_secs": 90
            }),
        )]));

        let (adapters, summaries) = build_adapters(&config).expect("adapter build succeeds");

        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].name(), "open_meteo");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0], "open_meteo");
    }

    #[test]
    fn build_adapters_rejects_unknown_adapter_config() {
        let config = test_config(serde_json::Map::from_iter([(
            "made_up_adapter".to_string(),
            serde_json::json!({
                "enabled": true,
                "base_url": "http://localhost:8080"
            }),
        )]));

        let error = build_adapters(&config)
            .err()
            .expect("unknown adapter should fail");

        assert_eq!(
            error.to_string(),
            "no adapter factory registered for 'made_up_adapter'"
        );
    }
}
