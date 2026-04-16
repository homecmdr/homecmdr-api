use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::Path;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::{Message, WebSocket};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Serialize;
use serde_json::json;
use smart_home_adapters as _;
use smart_home_core::adapter::{Adapter, registered_adapter_factories};
use smart_home_core::config::{Config, PersistenceBackend};
use smart_home_core::event::Event;
use smart_home_core::model::DeviceId;
use smart_home_core::runtime::Runtime;
use smart_home_core::store::DeviceStore;
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

    let (adapters, adapter_summaries) = build_adapters(&config).context("failed to build adapters")?;

    let runtime = Arc::new(Runtime::new(adapters, config.runtime));

    if let Some(store) = &device_store {
        let devices = store
            .load_all_devices()
            .await
            .context("failed to load persisted devices")?;
        runtime
            .registry()
            .restore(devices)
            .context("failed to restore persisted devices into registry")?;
    }

    let app = app(runtime.clone(), adapter_summaries);
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
            anyhow::bail!("duplicate adapter factory registration for '{}'", factory.name());
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
        PersistenceBackend::Postgres => anyhow::bail!("persistence backend 'postgres' is not implemented yet"),
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
            Ok(Event::AdapterStarted { .. } | Event::SystemError { .. }) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "persistence subscriber lagged; reconciling registry state");

                if let Err(error) = reconcile_device_store(runtime.registry().list(), store.clone()).await {
                    tracing::error!(error = %error, "failed to reconcile persisted registry state after lag");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn reconcile_device_store(devices: Vec<smart_home_core::model::Device>, store: Arc<dyn DeviceStore>) -> Result<()> {
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

    for stale_id in persisted_ids.difference(&current_ids) {
        store
            .delete_device(stale_id)
            .await
            .with_context(|| format!("failed to delete stale device '{}' during reconciliation", stale_id.0))?;
    }

    for device in devices {
        store
            .save_device(&device)
            .await
            .with_context(|| format!("failed to save device '{}' during reconciliation", device.id.0))?;
    }

    Ok(())
}

fn app(runtime: Arc<Runtime>, adapter_summaries: Vec<AdapterSummary>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/adapters", get(adapters))
        .route("/devices", get(list_devices))
        .route("/devices/{id}", get(get_device))
        .route("/devices/{id}/command", post(command_device))
        .route("/events", get(events))
        .layer(Extension(Arc::new(adapter_summaries)))
        .with_state(runtime)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn adapters(
    Extension(adapter_summaries): Extension<Arc<Vec<AdapterSummary>>>,
) -> Json<Vec<AdapterSummary>> {
    Json((*adapter_summaries).clone())
}

async fn list_devices(State(runtime): State<Arc<Runtime>>) -> Json<Vec<smart_home_core::model::Device>> {
    Json(runtime.registry().list())
}

async fn get_device(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<smart_home_core::model::Device>, ApiError> {
    runtime
        .registry()
        .get(&DeviceId(id.clone()))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("device '{id}' not found")))
}

async fn command_device(Path(id): Path<String>) -> Result<Json<serde_json::Value>, ApiError> {
    Err(ApiError::not_implemented(format!(
        "device commands are not implemented for '{id}'"
    )))
}

async fn events(ws: WebSocketUpgrade, State(runtime): State<Arc<Runtime>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_events_socket(socket, runtime))
}

async fn handle_events_socket(mut socket: WebSocket, runtime: Arc<Runtime>) {
    let mut receiver = runtime.bus().subscribe();

    loop {
        let frame = match receiver.recv().await {
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
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::time::Duration;

    use futures_util::StreamExt;
    use reqwest::StatusCode;
    use serde_json::Value;
    use smart_home_core::capability::{TEMPERATURE_OUTDOOR, measurement_value};
    use smart_home_core::config::{Config, PersistenceBackend};
    use smart_home_core::model::{AttributeValue, Device, DeviceId, DeviceKind, Metadata};
    use smart_home_core::runtime::RuntimeConfig;
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
            kind: DeviceKind::Sensor,
            attributes,
            metadata: Metadata {
                source: "test".to_string(),
                location: Some("lab".to_string()),
                accuracy: Some(1.0),
                vendor_specific: HashMap::new(),
            },
            updated_at: chrono::Utc::now(),
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
            telemetry: smart_home_core::config::TelemetryConfig::default(),
            adapters: adapters.into_iter().collect(),
        }
    }

    async fn create_runtime_with_hydrated_store(device: Device) -> Arc<Runtime> {
        let database_url = temp_sqlite_url();
        let store = SqliteDeviceStore::new(&database_url, true)
            .await
            .expect("sqlite store initializes");
        store.save_device(&device).await.expect("seed device persists");

        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let devices = store.load_all_devices().await.expect("load hydrated devices succeeds");
        runtime
            .registry()
            .restore(devices)
            .expect("registry restore succeeds");

        runtime
    }

    async fn spawn_test_server(runtime: Arc<Runtime>) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let app = app(
            runtime,
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
        assert_eq!(response.json::<Value>().await.expect("error JSON body")["error"], "device 'nonexistent' not found");

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

        let config = Config::load_from_file("/home/andy/projects/rust_home/smart-home/config/default.toml")
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
        let (mut adapters, _) = build_adapters(&config).expect("factory-based adapter build succeeds");
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
                let payload: Value = serde_json::from_str(message.to_text().expect("text websocket frame"))
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
        let restored = store.load_all_devices().await.expect("restored load succeeds");
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
        store.save_device(&stale).await.expect("seed stale device succeeds");

        let store_trait: Arc<dyn DeviceStore> = store.clone();
        reconcile_device_store(vec![current.clone()], store_trait)
            .await
            .expect("reconciliation succeeds");

        assert_eq!(store.load_all_devices().await.expect("load succeeds"), vec![current]);
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
