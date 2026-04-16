use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::Path;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use smart_home_adapters as _;
use smart_home_core::adapter::{registered_adapter_factories, Adapter};
use smart_home_core::command::DeviceCommand;
use smart_home_core::config::{Config, PersistenceBackend};
use smart_home_core::event::Event;
use smart_home_core::model::{DeviceId, Room, RoomId};
use smart_home_core::runtime::Runtime;
use smart_home_core::store::DeviceStore;
use smart_home_scenes::{SceneCatalog, SceneExecutionResult, SceneSummary};
use store_sql::SqliteDeviceStore;
use tracing::Level;

#[derive(Clone, Serialize)]
struct AdapterSummary {
    name: String,
    status: &'static str,
}

#[derive(Serialize)]
struct HealthResponse {
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
}

#[derive(Debug, Serialize)]
struct SceneExecuteResponse {
    status: &'static str,
    results: Vec<SceneExecutionResult>,
}

type BuiltAdapters = (Vec<Box<dyn Adapter>>, Vec<AdapterSummary>);

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
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

    let runtime = Arc::new(Runtime::new(adapters, config.runtime));
    let scenes = if config.scenes.enabled {
        Arc::new(
            SceneCatalog::load_from_directory(&config.scenes.directory).with_context(|| {
                format!("failed to load scenes from {}", config.scenes.directory)
            })?,
        )
    } else {
        Arc::new(SceneCatalog::empty())
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
        },
        adapter_summaries,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .context("failed to bind API listener")?;

    let persistence_task = device_store.map(|store| {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            run_persistence_worker(runtime, store).await;
        })
    });

    let runtime_task = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime.run().await;
        })
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("API server failed")?;

    runtime_task.abort();
    let _ = runtime_task.await;

    if let Some(task) = persistence_task {
        task.abort();
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
            summaries.push(AdapterSummary {
                name: name.clone(),
                status: "running",
            });
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
            SqliteDeviceStore::new(database_url, config.persistence.auto_create)
                .await
                .with_context(|| format!("failed to initialize SQLite store '{database_url}'"))?,
        ),
        PersistenceBackend::Postgres => {
            anyhow::bail!("persistence backend 'postgres' is not implemented yet")
        }
    };

    Ok(Some(store))
}

async fn run_persistence_worker(runtime: Arc<Runtime>, store: Arc<dyn DeviceStore>) {
    let mut receiver = runtime.bus().subscribe();

    loop {
        match receiver.recv().await {
            Ok(Event::DeviceAdded { device }) => {
                if let Err(error) = store.save_device(&device).await {
                    tracing::error!(device_id = %device.id.0, error = %error, "failed to persist added device");
                }
            }
            Ok(Event::DeviceRoomChanged { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device room change");
                    }
                }
            }
            Ok(Event::DeviceStateChanged { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device state change");
                    }
                } else {
                    tracing::warn!(device_id = %id.0, "device state changed event received after device disappeared from registry");
                }
            }
            Ok(Event::DeviceRemoved { id }) => {
                if let Err(error) = store.delete_device(&id).await {
                    tracing::error!(device_id = %id.0, error = %error, "failed to delete persisted device");
                }
            }
            Ok(Event::DeviceSeen { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device last_seen update");
                    }
                }
            }
            Ok(Event::RoomAdded { room } | Event::RoomUpdated { room }) => {
                if let Err(error) = store.save_room(&room).await {
                    tracing::error!(room_id = %room.id.0, error = %error, "failed to persist room");
                }
            }
            Ok(Event::RoomRemoved { id }) => {
                if let Err(error) = store.delete_room(&id).await {
                    tracing::error!(room_id = %id.0, error = %error, "failed to delete persisted room");
                }

                for device in runtime.registry().list() {
                    if device.room_id.is_none() {
                        if let Err(error) = store.save_device(&device).await {
                            tracing::error!(device_id = %device.id.0, error = %error, "failed to persist room removal device update");
                        }
                    }
                }
            }
            Ok(Event::AdapterStarted { .. } | Event::SystemError { .. }) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
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
                    tracing::error!(error = %error, "failed to reconcile persisted registry state after lag");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
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

fn app(state: AppState, adapter_summaries: Vec<AdapterSummary>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/adapters", get(adapters))
        .route("/scenes", get(list_scenes))
        .route("/scenes/{id}/execute", post(execute_scene))
        .route("/rooms", get(list_rooms).post(create_room))
        .route("/rooms/{id}", get(get_room))
        .route("/rooms/{id}/devices", get(list_room_devices))
        .route("/rooms/{id}/command", post(command_room_devices))
        .route("/devices", get(list_devices))
        .route("/devices/{id}", get(get_device))
        .route("/devices/{id}/room", post(assign_device_room))
        .route("/devices/{id}/command", post(command_device))
        .route("/events", get(events))
        .layer(Extension(Arc::new(adapter_summaries)))
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn adapters(
    Extension(adapter_summaries): Extension<Arc<Vec<AdapterSummary>>>,
) -> Json<Vec<AdapterSummary>> {
    Json((*adapter_summaries).clone())
}

async fn list_scenes(State(state): State<AppState>) -> Json<Vec<SceneSummary>> {
    Json(state.scenes.summaries())
}

async fn execute_scene(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SceneExecuteResponse>, ApiError> {
    let Some(results) = state
        .scenes
        .execute(&id, state.runtime.clone())
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?
    else {
        return Err(ApiError::not_found(format!("scene '{id}' not found")));
    };

    Ok(Json(SceneExecuteResponse {
        status: "ok",
        results,
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

    match state.runtime.command_device(&device_id, command).await {
        Ok(true) => Ok(Json(json!({ "status": "ok" }))),
        Ok(false) => Err(ApiError::not_implemented(format!(
            "device commands are not implemented for '{id}'"
        ))),
        Err(error) => Err(ApiError::new(StatusCode::BAD_REQUEST, error.to_string())),
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
        match state
            .runtime
            .command_device(&device_id, command.clone())
            .await
        {
            Ok(true) => results.push(RoomCommandResult {
                device_id: device_id.0,
                status: "ok",
                message: None,
            }),
            Ok(false) => results.push(RoomCommandResult {
                device_id: device_id.0,
                status: "unsupported",
                message: Some("device commands are not implemented".to_string()),
            }),
            Err(error) => results.push(RoomCommandResult {
                device_id: device_id.0,
                status: "error",
                message: Some(error.to_string()),
            }),
        }
    }

    Ok(Json(results))
}

async fn events(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_events_socket(socket, state.runtime))
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
    use smart_home_core::config::{Config, PersistenceBackend};
    use smart_home_core::model::{
        AttributeValue, Device, DeviceId, DeviceKind, Metadata, Room, RoomId,
    };
    use smart_home_core::runtime::RuntimeConfig;
    use smart_home_scenes::SceneCatalog;
    use store_sql::SqliteDeviceStore;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;

    use super::*;

    struct MockResponse {
        status_line: &'static str,
        body: &'static str,
    }

    struct CommandAdapter;

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
            },
            scenes: smart_home_core::config::ScenesConfig::default(),
            telemetry: smart_home_core::config::TelemetryConfig::default(),
            adapters: adapters.into_iter().collect(),
        }
    }

    fn empty_scenes() -> Arc<SceneCatalog> {
        Arc::new(SceneCatalog::empty())
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
        let app = app(
            AppState {
                runtime,
                scenes: empty_scenes(),
            },
            vec![AdapterSummary {
                name: "open_meteo".to_string(),
                status: "running",
            }],
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
        scenes: Arc<SceneCatalog>,
    ) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let app = app(
            AppState { runtime, scenes },
            vec![AdapterSummary {
                name: "open_meteo".to_string(),
                status: "running",
            }],
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
        assert_eq!(
            response.json::<Value>().await.expect("health JSON body"),
            json!({ "status": "ok" })
        );

        let _ = shutdown.send(());
        handle.await.expect("server task completes");
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
        let scenes = Arc::new(SceneCatalog::load_from_directory(&scene_dir).expect("scenes load"));
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
        let scenes = Arc::new(SceneCatalog::load_from_directory(&scene_dir).expect("scenes load"));
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
        let persistence_task = {
            let runtime = runtime.clone();
            let store: Arc<dyn DeviceStore> = store.clone();
            tokio::spawn(async move {
                run_persistence_worker(runtime, store).await;
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

        persistence_task.abort();
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
            },
            scenes: smart_home_core::config::ScenesConfig::default(),
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
        assert_eq!(summaries[0].name, "open_meteo");
        assert_eq!(summaries[0].status, "running");
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
