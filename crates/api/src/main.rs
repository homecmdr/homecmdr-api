use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::Query;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::Path;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use smart_home_adapters as _;
use smart_home_automations::{
    AutomationCatalog, AutomationExecutionObserver, AutomationRunner,
};
use smart_home_core::adapter::{registered_adapter_factories, Adapter};
use smart_home_core::command::DeviceCommand;
use smart_home_core::config::{Config, PersistenceBackend};
use smart_home_core::event::Event;
use smart_home_core::model::{DeviceId, Room, RoomId};
use smart_home_core::runtime::Runtime;
use smart_home_core::store::{
    AttributeHistoryEntry, AutomationExecutionHistoryEntry, CommandAuditEntry,
    DeviceHistoryEntry, DeviceStore, SceneExecutionHistoryEntry, SceneStepResult,
};
use smart_home_scenes::{SceneCatalog, SceneExecutionResult, SceneSummary};
use store_sql::{SqliteDeviceStore, SqliteHistoryConfig};
use tracing::Level;

#[derive(Clone, Serialize)]
struct AdapterSummary {
    name: String,
    status: String,
    message: Option<String>,
    last_updated: Option<DateTime<Utc>>,
}

#[derive(Clone, Serialize)]
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

#[derive(Debug, Serialize)]
struct RoomCommandResult {
    device_id: String,
    status: &'static str,
    message: Option<String>,
}

#[derive(Clone)]
struct AppState {
    runtime: Arc<Runtime>,
    scenes: Arc<SceneCatalog>,
    health: HealthState,
    store: Option<Arc<dyn DeviceStore>>,
    history: HistorySettings,
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
    adapters: HashMap<String, ComponentStatus>,
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

impl HealthState {
    fn new(adapter_names: &[String], persistence_enabled: bool, automations_enabled: bool) -> Self {
        let adapters = adapter_names
            .iter()
            .cloned()
            .map(|name| (name, ComponentStatus::starting("waiting for adapter start")))
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
        snapshot
            .adapters
            .entry(adapter.to_string())
            .or_insert_with(ComponentStatus::ok)
            .set_ok();
    }

    fn adapter_error(&self, adapter: &str, message: impl Into<String>) {
        let mut snapshot = self.write();
        snapshot
            .adapters
            .entry(adapter.to_string())
            .or_insert_with(|| ComponentStatus::starting("adapter not started"))
            .set_error(message);
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
        let adapters = snapshot
            .adapters
            .iter()
            .map(|(name, status)| AdapterSummary {
                name: name.clone(),
                status: status.status.clone(),
                message: status.message.clone(),
                last_updated: status.last_updated,
            })
            .collect::<Vec<_>>();

        let ready = snapshot.startup_complete
            && snapshot.runtime.status == "ok"
            && snapshot.persistence.status == "ok"
            && snapshot.automations.status == "ok"
            && snapshot.adapters.values().all(|status| status.status == "ok");
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
                format!(
                    "history query limit must be <= {}",
                    self.max_limit
                ),
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
        Arc::new(
            SceneCatalog::load_from_directory(&config.scenes.directory, scripts_root.clone())
                .with_context(|| {
                    format!("failed to load scenes from {}", config.scenes.directory)
                })?,
        )
    } else {
        Arc::new(SceneCatalog::empty())
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
        let devices = store
            .load_all_devices()
            .await
            .context("failed to load persisted devices")?;
        runtime.registry().restore_rooms(rooms);
        runtime
            .registry()
            .restore(devices)
            .context("failed to restore persisted devices into registry")?;
    }

    let app = app(
        AppState {
            runtime: runtime.clone(),
            scenes,
            health: health.clone(),
            store: device_store.clone(),
            history: HistorySettings::from_config(&config),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .context("failed to bind API listener")?;

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
        let automations = automations.clone();
        let health = health.clone();
        let observer = device_store.clone().and_then(|store| {
            if config.persistence.history.enabled {
                Some(Arc::new(StoreAutomationObserver { store }) as Arc<dyn AutomationExecutionObserver>)
            } else {
                None
            }
        });
        tokio::spawn(async move {
            health.automations_ok();
            let runner = if let Some(observer) = observer {
                AutomationRunner::new((*automations).clone()).with_observer(observer)
            } else {
                AutomationRunner::new((*automations).clone())
            };
            runner.run(runtime).await;
            health.automations_error("automation runner exited unexpectedly");
        })
    };

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
                    health.persistence_error(format!("failed to persist added device '{}': {error}", device.id.0));
                    tracing::error!(device_id = %device.id.0, error = %error, "failed to persist added device");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::DeviceRoomChanged { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!("failed to persist room change for '{}': {error}", id.0));
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device room change");
                    } else {
                        health.persistence_ok();
                    }
                }
            }
            Ok(Event::DeviceStateChanged { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!("failed to persist state change for '{}': {error}", id.0));
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
                    health.persistence_error(format!("failed to delete persisted device '{}': {error}", id.0));
                    tracing::error!(device_id = %id.0, error = %error, "failed to delete persisted device");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::DeviceSeen { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!("failed to persist last_seen for '{}': {error}", id.0));
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device last_seen update");
                    } else {
                        health.persistence_ok();
                    }
                }
            }
            Ok(Event::RoomAdded { room } | Event::RoomUpdated { room }) => {
                if let Err(error) = store.save_room(&room).await {
                    health.persistence_error(format!("failed to persist room '{}': {error}", room.id.0));
                    tracing::error!(room_id = %room.id.0, error = %error, "failed to persist room");
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
            Ok(Event::AdapterStarted { .. } | Event::SystemError { .. }) => {}
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

fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/adapters", get(adapters))
        .route("/scenes", get(list_scenes))
        .route("/scenes/{id}/history", get(get_scene_history))
        .route("/scenes/{id}/execute", post(execute_scene))
        .route("/automations/{id}/history", get(get_automation_history))
        .route("/rooms", get(list_rooms).post(create_room))
        .route("/rooms/{id}", get(get_room))
        .route("/rooms/{id}/devices", get(list_room_devices))
        .route("/rooms/{id}/command", post(command_room_devices))
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
        .with_state(state)
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

async fn list_scenes(State(state): State<AppState>) -> Json<Vec<SceneSummary>> {
    Json(state.scenes.summaries())
}

async fn execute_scene(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SceneExecuteResponse>, ApiError> {
    let executed_at = Utc::now();
    let execution = state
        .scenes
        .execute(&id, state.runtime.clone())
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
    let Some(results) = execution else {
        return Err(ApiError::not_found(format!("scene '{id}' not found")));
    };

    persist_scene_history(
        &state,
        SceneExecutionHistoryEntry {
            executed_at,
            scene_id: id.clone(),
            status: "ok".to_string(),
            error: None,
            results: results
                .iter()
                .map(|result| SceneStepResult {
                    target: result.target.clone(),
                    status: result.status.to_string(),
                    message: result.message.clone(),
                })
                .collect(),
        },
    );

    Ok(Json(SceneExecuteResponse {
        status: "ok",
        results,
    }))
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

async fn list_devices(State(state): State<AppState>) -> Json<Vec<smart_home_core::model::Device>> {
    Json(state.runtime.registry().list())
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
    match state.runtime.command_device(&device_id, command_to_run).await {
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
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "history is disabled",
        ));
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

fn ensure_device_exists(state: &AppState, device_id: &DeviceId, raw_id: &str) -> Result<(), ApiError> {
    if state.runtime.registry().get(device_id).is_none() {
        return Err(ApiError::not_found(format!("device '{raw_id}' not found")));
    }

    Ok(())
}

fn internal_api_error(error: anyhow::Error) -> ApiError {
    tracing::error!(error = %error, "internal API error");
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
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
        Event::DeviceStateChanged { id, attributes } => {
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
        Event::DeviceRoomChanged { id, room_id } => {
            json!({ "type": "device.room_changed", "id": id.0, "room_id": room_id.map(|room| room.0) })
        }
        Event::AdapterStarted { adapter } => {
            json!({ "type": "adapter.started", "adapter": adapter })
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
    use smart_home_core::capability::{measurement_value, TEMPERATURE_OUTDOOR};
    use smart_home_core::command::DeviceCommand;
    use smart_home_core::config::{Config, HistoryConfig, PersistenceBackend};
    use smart_home_core::model::{
        AttributeValue, Device, DeviceId, DeviceKind, Metadata, Room, RoomId,
    };
    use smart_home_core::runtime::RuntimeConfig;
    use smart_home_scenes::SceneCatalog;
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
        device_history: Mutex<HashMap<DeviceId, Vec<DeviceHistoryEntry>>>,
        attribute_history: Mutex<HashMap<(DeviceId, String), Vec<AttributeHistoryEntry>>>,
        command_audit: Mutex<Vec<CommandAuditEntry>>,
        scene_history: Mutex<HashMap<String, Vec<SceneExecutionHistoryEntry>>>,
        automation_history: Mutex<HashMap<String, Vec<AutomationExecutionHistoryEntry>>>,
        first_save_started: Notify,
        first_save_seen: Mutex<bool>,
        delay_first_save: Mutex<bool>,
    }

    impl LaggyMemoryStore {
        fn new() -> Self {
            Self {
                devices: Mutex::new(HashMap::new()),
                rooms: Mutex::new(HashMap::new()),
                device_history: Mutex::new(HashMap::new()),
                attribute_history: Mutex::new(HashMap::new()),
                command_audit: Mutex::new(Vec::new()),
                scene_history: Mutex::new(HashMap::new()),
                automation_history: Mutex::new(HashMap::new()),
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

        async fn delete_device(&self, id: &DeviceId) -> anyhow::Result<()> {
            self.devices.lock().expect("devices lock").remove(id);
            Ok(())
        }

        async fn delete_room(&self, id: &RoomId) -> anyhow::Result<()> {
            self.rooms.lock().expect("rooms lock").remove(id);
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
                .filter(|entry| start.map(|value| entry.observed_at >= value).unwrap_or(true))
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
                .filter(|entry| start.map(|value| entry.observed_at >= value).unwrap_or(true))
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
                .filter(|entry| start.map(|value| entry.recorded_at >= value).unwrap_or(true))
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
                .filter(|entry| start.map(|value| entry.executed_at >= value).unwrap_or(true))
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
                .filter(|entry| start.map(|value| entry.executed_at >= value).unwrap_or(true))
                .filter(|entry| end.map(|value| entry.executed_at <= value).unwrap_or(true))
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| b.executed_at.cmp(&a.executed_at));
            entries.truncate(limit);
            Ok(entries)
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

    fn empty_scenes() -> Arc<SceneCatalog> {
        Arc::new(SceneCatalog::empty())
    }

    fn history_settings(enabled: bool) -> HistorySettings {
        HistorySettings {
            enabled,
            default_limit: 200,
            max_limit: 1000,
        }
    }

    fn test_health(adapter_names: &[&str]) -> HealthState {
        let health = HealthState::new(
            &adapter_names.iter().map(|name| (*name).to_string()).collect::<Vec<_>>(),
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
        let app = app(AppState {
            runtime,
            scenes: empty_scenes(),
            health: test_health(&["open_meteo"]),
            store: None,
            history: history_settings(false),
        });
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
        scenes: Arc<SceneCatalog>,
    ) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let app = app(AppState {
            runtime,
            scenes,
            health: test_health(&["open_meteo"]),
            store: None,
            history: history_settings(false),
        });
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
        let app = app(AppState {
            runtime,
            scenes: empty_scenes(),
            health: test_health(&["open_meteo"]),
            store: Some(store),
            history: history_settings(history_enabled),
        });
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
        let scenes = Arc::new(
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
        let scenes = Arc::new(
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
        store.save_device(&first).await.expect("first save succeeds");
        store.save_device(&second).await.expect("second save succeeds");

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

        let (addr, shutdown, handle) =
            spawn_test_server_with_store(runtime, store, true).await;

        let response = reqwest::get(format!(
            "http://{addr}/devices/test:history/history?limit=1"
        ))
        .await
        .expect("history request succeeds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<Value>().await.expect("history json body");
        assert_eq!(body["device_id"], "test:history");
        assert_eq!(body["entries"].as_array().expect("entries array").len(), 1);
        assert_eq!(body["entries"][0]["device"]["attributes"][TEMPERATURE_OUTDOOR]["value"], 21.5);

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
        store.save_device(&first).await.expect("first save succeeds");
        store.save_device(&second).await.expect("second save succeeds");

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

        let (addr, shutdown, handle) =
            spawn_test_server_with_store(runtime, store, true).await;

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
        let body = audit.json::<Value>().await.expect("command audit json body");
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
        let scenes = Arc::new(
            SceneCatalog::load_from_directory(&scene_dir, None).expect("scenes load"),
        );

        let app = app(AppState {
            runtime: runtime.clone(),
            scenes,
            health: test_health(&["open_meteo"]),
            store: Some(store.clone()),
            history: history_settings(true),
        });
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
        let body = history.json::<Value>().await.expect("scene history json body");
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
            AutomationCatalog::load_from_directory(&automation_dir, None).expect("automations load"),
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

        let observer = Arc::new(StoreAutomationObserver { store: store.clone() })
            as Arc<dyn AutomationExecutionObserver>;
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

        assert_eq!(store.load_all_rooms().await.expect("rooms load"), vec![room]);
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
                if store
                    .load_all_devices()
                    .await
                    .expect("load succeeds")
                    == vec![latest.clone()]
                {
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
        reconcile_device_store(Vec::new(), vec![current.clone()], store_trait)
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
            "zigbee2mqtt".to_string(),
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
            "no adapter factory registered for 'zigbee2mqtt'"
        );
    }
}
