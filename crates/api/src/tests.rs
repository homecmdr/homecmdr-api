use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use homecmdr_core::capability::{measurement_value, TEMPERATURE_OUTDOOR, WIND_SPEED};
use homecmdr_core::command::DeviceCommand;
use homecmdr_core::config::{Config, HistoryConfig, PersistenceBackend};
use homecmdr_core::model::{
    AttributeValue, Device, DeviceGroup, DeviceId, DeviceKind, GroupId, Metadata, Room, RoomId,
};
use homecmdr_core::runtime::RuntimeConfig;
use homecmdr_core::store::AutomationRuntimeState;
use homecmdr_scenes::{SceneCatalog, SceneRunner};
use reqwest::StatusCode;
use serde_json::Value;
use store_sql::{SqliteDeviceStore, SqliteHistoryConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Notify};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;

use homecmdr_core::config::LocaleConfig;
use homecmdr_core::person_registry::PersonRegistry;
use homecmdr_core::store::PersonStore;

use super::*;

const TEST_MASTER_KEY: &str = "homecmdr-test-key-for-tests-only";

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {TEST_MASTER_KEY}"))
                    .expect("valid header value"),
            );
            headers
        })
        .build()
        .expect("test client builds")
}

/// Build a WebSocket `connect_async` request that includes the test master-key
/// bearer token, so it passes the auth middleware on the `/events` route.
fn authed_ws_request(url: &str) -> tokio_tungstenite::tungstenite::http::Request<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().expect("valid ws url");
    req.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {TEST_MASTER_KEY}")
            .parse()
            .expect("valid auth header value"),
    );
    req
}

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

    async fn save_scene_execution(&self, entry: &SceneExecutionHistoryEntry) -> anyhow::Result<()> {
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

    async fn prune_history(&self) -> anyhow::Result<()> {
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
        _registry: homecmdr_core::registry::DeviceRegistry,
        _bus: homecmdr_core::bus::EventBus,
    ) -> Result<()> {
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn command(
        &self,
        device_id: &DeviceId,
        command: DeviceCommand,
        registry: homecmdr_core::registry::DeviceRegistry,
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
    let path = std::env::temp_dir().join(format!("homecmdr-api-{unique}.db"));
    format!("sqlite://{}", path.display())
}

fn test_config(adapters: serde_json::Map<String, serde_json::Value>) -> Config {
    Config {
        runtime: RuntimeConfig {
            event_bus_capacity: 16,
        },
        api: homecmdr_core::config::ApiConfig {
            bind_address: "127.0.0.1:3001".to_string(),
            cors: homecmdr_core::config::ApiCorsConfig::default(),
            rate_limit: homecmdr_core::config::RateLimitConfig::default(),
        },
        locale: homecmdr_core::config::LocaleConfig::default(),
        logging: homecmdr_core::config::LoggingConfig {
            level: "info".to_string(),
        },
        persistence: homecmdr_core::config::PersistenceConfig {
            enabled: false,
            backend: PersistenceBackend::Sqlite,
            database_url: Some(temp_sqlite_url()),
            auto_create: true,
            history: HistoryConfig::default(),
        },
        scenes: homecmdr_core::config::ScenesConfig::default(),
        automations: homecmdr_core::config::AutomationsConfig::default(),
        scripts: homecmdr_core::config::ScriptsConfig::default(),
        telemetry: homecmdr_core::config::TelemetryConfig::default(),
        adapters: adapters.into_iter().collect(),
        auth: homecmdr_core::config::AuthConfig {
            master_key: TEST_MASTER_KEY.to_string(),
        },
        plugins: homecmdr_core::config::PluginsConfig::default(),
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
    let scenes = if config.scenes.enabled && std::path::Path::new(&config.scenes.directory).exists()
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
            None,
            sha256_hex(TEST_MASTER_KEY),
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
            None,
            sha256_hex(TEST_MASTER_KEY),
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
            None,
            sha256_hex(TEST_MASTER_KEY),
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
            None,
            sha256_hex(TEST_MASTER_KEY),
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

fn write_temp_scene_dir(files: &[(&str, &str)]) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("homecmdr-api-scenes-{unique}"));
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

    let response = test_client()
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("health request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<Value>().await.expect("health JSON body");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["ready"], true);
    assert_eq!(body["runtime"]["status"], "ok");

    let ready = test_client()
        .get(format!("http://{addr}/ready"))
        .send()
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

    let response = test_client()
        .get(format!("http://{addr}/devices"))
        .send()
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

    let response = test_client()
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

    let denied = test_client()
        .get(format!("http://{addr}/devices"))
        .header("Origin", "http://evil.example")
        .send()
        .await
        .expect("disallowed origin request succeeds");
    assert!(denied
        .headers()
        .get("access-control-allow-origin")
        .is_none());

    let preflight = test_client()
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

    let response = test_client()
        .get(format!(
            "http://{addr}/devices?ids=test:second&ids=test:first"
        ))
        .send()
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

    let response = test_client()
        .get(format!(
            "http://{addr}/devices?ids=missing&ids=test:device&ids=test:device"
        ))
        .send()
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

    let client = test_client();
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

    let rooms = test_client()
        .get(format!("http://{addr}/rooms"))
        .send()
        .await
        .expect("rooms request succeeds")
        .json::<Value>()
        .await
        .expect("rooms json body");
    assert_eq!(rooms.as_array().expect("rooms array").len(), 1);

    let room_devices = test_client()
        .get(format!("http://{addr}/rooms/outside/devices"))
        .send()
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

    let response = test_client()
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

    let response = test_client()
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
    let client = test_client();

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

    let groups = test_client()
        .get(format!("http://{addr}/groups"))
        .send()
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
    let client = test_client();

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
    let scenes =
        SceneRunner::new(SceneCatalog::load_from_directory(&scene_dir, None).expect("scenes load"));
    let (addr, shutdown, handle) = spawn_test_server_with_scenes(runtime, scenes).await;

    let response = test_client()
        .get(format!("http://{addr}/scenes"))
        .send()
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
        AutomationCatalog::load_from_directory(&automation_dir, None).expect("automations load"),
    );
    let automation_control = Arc::new(AutomationRunner::new((*automations).clone()).controller());
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
            auth_key_store: None,
            master_key_hash: sha256_hex(TEST_MASTER_KEY),
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
            backstop_timeout: Duration::from_secs(3600),
            plugins_enabled: false,
            plugins_directory: String::new(),
            pictures_directory: String::new(),
            person_store: None,
            person_registry: None,
            plugin_catalog: Arc::new(RwLock::new(Vec::new())),
            ipc_adapter_names: Arc::new(HashSet::new()),
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

    let response = test_client()
        .get(format!("http://{addr}/automations"))
        .send()
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

    let detail = test_client()
        .get(format!("http://{addr}/automations/rain_check"))
        .send()
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
        AutomationCatalog::load_from_directory(&automation_dir, None).expect("automations load"),
    );
    let observer =
        Arc::new(RecordingAutomationObserver::default()) as Arc<dyn AutomationExecutionObserver>;
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
            auth_key_store: None,
            person_store: None,
            person_registry: None,
            master_key_hash: sha256_hex(TEST_MASTER_KEY),
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
            backstop_timeout: Duration::from_secs(3600),
            plugins_enabled: false,
            plugins_directory: String::new(),
            pictures_directory: String::new(),
            plugin_catalog: Arc::new(RwLock::new(Vec::new())),
            ipc_adapter_names: Arc::new(HashSet::new()),
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

    let client = test_client();

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

    let client = test_client();

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

    let capabilities = test_client()
        .get(format!("http://{addr}/capabilities"))
        .send()
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

    let diagnostics = test_client()
        .get(format!("http://{addr}/diagnostics"))
        .send()
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
    let (runner_tx, _runner_rx) = watch::channel(AutomationRunner::new(AutomationCatalog::empty()));
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
            auth_key_store: None,
            person_store: None,
            person_registry: None,
            master_key_hash: sha256_hex(TEST_MASTER_KEY),
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
            backstop_timeout: Duration::from_secs(3600),
            plugins_enabled: false,
            plugins_directory: String::new(),
            pictures_directory: String::new(),
            plugin_catalog: Arc::new(RwLock::new(Vec::new())),
            ipc_adapter_names: Arc::new(HashSet::new()),
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

    let response = test_client()
        .get(format!("http://{addr}/adapters/open_meteo"))
        .send()
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

    let client = test_client();

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
    let scenes =
        SceneRunner::new(SceneCatalog::load_from_directory(&scene_dir, None).expect("scenes load"));
    let (addr, shutdown, handle) = spawn_test_server_with_scenes(runtime.clone(), scenes).await;

    let response = test_client()
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

    let response = test_client()
        .get(format!("http://{addr}/devices"))
        .send()
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

    let response = test_client()
        .get(format!(
            "http://{addr}/devices/test:history/history?limit=1"
        ))
        .send()
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

    let response = test_client()
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

    let response = test_client()
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

    let audit = test_client()
        .get(format!("http://{addr}/audit/commands"))
        .send()
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
    let scenes =
        SceneRunner::new(SceneCatalog::load_from_directory(&scene_dir, None).expect("scenes load"));

    let config = test_config(serde_json::Map::new());
    let (runner_tx, _runner_rx) = watch::channel(AutomationRunner::new(AutomationCatalog::empty()));
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
            auth_key_store: None,
            person_store: None,
            person_registry: None,
            master_key_hash: sha256_hex(TEST_MASTER_KEY),
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
            backstop_timeout: Duration::from_secs(3600),
            plugins_enabled: false,
            plugins_directory: String::new(),
            pictures_directory: String::new(),
            plugin_catalog: Arc::new(RwLock::new(Vec::new())),
            ipc_adapter_names: Arc::new(HashSet::new()),
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

    let response = test_client()
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

    let history = test_client()
        .get(format!("http://{addr}/scenes/set_brightness/history"))
        .send()
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

    let response = test_client()
        .get(format!("http://{addr}/automations/rain_check/history"))
        .send()
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

    let response = test_client()
        .get(format!("http://{addr}/devices/test:device/history"))
        .send()
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

    let response = test_client()
        .get(format!("http://{addr}/devices/nonexistent"))
        .send()
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

    let response = test_client()
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
        Some(&homecmdr_core::model::AttributeValue::Integer(42))
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

    let response = test_client()
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

    let (mut socket, _) = connect_async(authed_ws_request(&format!("ws://{addr}/events")))
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

    let (mut socket, _) = connect_async(authed_ws_request(&format!("ws://{addr}/events")))
        .await
        .expect("websocket connects");

    let reload = test_client()
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
        "homecmdr-api-scripts-{}",
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

    let (mut socket, _) = connect_async(authed_ws_request(&format!("ws://{addr}/events")))
        .await
        .expect("websocket connects");

    let reload = test_client()
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

    let (socket, _) = connect_async(authed_ws_request(&format!("ws://{addr}/events")))
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
    // Locate the WASM binary (built by `cargo build --release` in plugins/open-meteo/).
    let plugins_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/plugins");
    if !plugins_dir.join("open_meteo.wasm").exists() {
        eprintln!(
            "SKIP end_to_end_runtime_http_and_websocket_flow: \
                 config/plugins/open_meteo.wasm not found."
        );
        return;
    }

    let server = MockServer::start(vec![MockResponse {
        status_line: "HTTP/1.1 200 OK",
        body: "{\"current\":{\"temperature_2m\":18.25,\"apparent_temperature\":16.0,\
                   \"relative_humidity_2m\":62.0,\"precipitation\":0.0,\"cloud_cover\":25,\
                   \"uv_index\":3.5,\"surface_pressure\":1012.0,\"wind_speed_10m\":11.5,\
                   \"wind_gusts_10m\":18.0,\"wind_direction_10m\":225.0,\
                   \"weather_code\":2,\"is_day\":1}}",
    }])
    .await;

    // Build a config that enables the WASM plugin for open_meteo.
    // poll_interval_secs = 1 so the adapter polls immediately in the test.
    let mut config = test_config(serde_json::Map::from_iter([(
        "open_meteo".to_string(),
        serde_json::json!({
            "enabled": true,
            "latitude": 51.5,
            "longitude": -0.1,
            "poll_interval_secs": 1,
            "base_url": server.base_url(),
        }),
    )]));
    config.plugins = homecmdr_core::config::PluginsConfig {
        enabled: true,
        directory: plugins_dir.to_string_lossy().into_owned(),
    };

    let (mut adapters, _, _) = build_adapters(&config).expect("WASM plugin adapter build succeeds");
    let adapter = adapters.pop().expect("open_meteo WASM adapter built");
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

    let devices = test_client()
        .get(format!("http://{addr}/devices"))
        .send()
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

    let (mut socket, _) = connect_async(authed_ws_request(&format!("ws://{addr}/events")))
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
fn temp_sqlite_url_points_to_temp_dir() {
    let url = temp_sqlite_url();
    assert!(url.starts_with("sqlite://"));

    if let Some(path) = url.strip_prefix("sqlite://") {
        let _ = fs::remove_file(path);
    }
}

#[test]
fn build_adapters_loads_wasm_plugins() {
    // Locate the compiled WASM binary relative to this crate's manifest directory.
    // config/plugins/ is two levels up from crates/api/.
    let plugins_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/plugins");

    if !plugins_dir.join("open_meteo.wasm").exists() {
        eprintln!(
            "SKIP build_adapters_loads_wasm_plugins: \
                 config/plugins/open_meteo.wasm not found — \
                 build the WASM guest first."
        );
        return;
    }

    let mut config = test_config(serde_json::Map::from_iter([(
        "open_meteo".to_string(),
        serde_json::json!({
            "enabled": true,
            "latitude": 51.5,
            "longitude": -0.1,
            "poll_interval_secs": 90,
            // Use a non-existent URL so init succeeds but we never actually poll.
            "base_url": "http://127.0.0.1:1"
        }),
    )]));
    config.plugins = homecmdr_core::config::PluginsConfig {
        enabled: true,
        directory: plugins_dir.to_string_lossy().into_owned(),
    };

    let (adapters, summaries, _ipc_names) =
        build_adapters(&config).expect("adapter build succeeds");

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

// ── File API tests ────────────────────────────────────────────────────────

/// Spawn a test server with three writable temp dirs (scenes, automations,
/// scripts) that are pre-populated with the given files.
async fn spawn_test_server_for_files(
    scenes_files: &[(&str, &str)],
    automations_files: &[(&str, &str)],
    scripts_files: &[(&str, &str)],
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let scenes_dir = write_temp_scene_dir(scenes_files);
    let automations_dir = write_temp_automation_dir(automations_files);
    let scripts_dir = write_temp_scene_dir(scripts_files);

    let runtime = Arc::new(Runtime::new(
        Vec::new(),
        RuntimeConfig {
            event_bus_capacity: 16,
        },
    ));
    let mut config = test_config(serde_json::Map::new());
    // The file API reads/writes files directly via the directory paths
    // stored in AppState and works correctly without the catalogs being
    // loaded.  Disable scene and automation loading so the server does not
    // try to evaluate the placeholder Lua files (which are not valid scene
    // or automation modules) and panic.
    config.scenes.enabled = false;
    config.scenes.directory = scenes_dir.to_string_lossy().to_string();
    config.automations.enabled = false;
    config.automations.directory = automations_dir.to_string_lossy().to_string();
    // scripts.enabled = true so scripts_root is set and the scripts
    // directory is propagated into AppState (no catalog loading happens
    // for scripts, only a path is stored).
    config.scripts.enabled = true;
    config.scripts.directory = scripts_dir.to_string_lossy().to_string();

    let (addr, shutdown, handle) = spawn_test_server_with_config(runtime, config).await;
    (
        addr,
        shutdown,
        handle,
        scenes_dir,
        automations_dir,
        scripts_dir,
    )
}

#[tokio::test]
async fn file_api_lists_lua_files() {
    let (addr, shutdown, handle, _, _, _) = spawn_test_server_for_files(
        &[("wakeup.lua", "-- scene")],
        &[("morning.lua", "-- automation")],
        &[("helpers.lua", "-- helpers")],
    )
    .await;

    let response = test_client()
        .get(format!("http://{addr}/files"))
        .send()
        .await
        .expect("list files request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .json::<Value>()
        .await
        .expect("list files json body");
    let entries = body.as_array().expect("files array");

    let paths: Vec<&str> = entries.iter().filter_map(|e| e["path"].as_str()).collect();
    assert!(
        paths.contains(&"scenes/wakeup.lua"),
        "expected scenes/wakeup.lua in {paths:?}"
    );
    assert!(
        paths.contains(&"automations/morning.lua"),
        "expected automations/morning.lua in {paths:?}"
    );
    assert!(
        paths.contains(&"scripts/helpers.lua"),
        "expected scripts/helpers.lua in {paths:?}"
    );

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn file_api_reads_file_content() {
    let content = "-- this is the wakeup scene\nreturn {}";
    let (addr, shutdown, handle, _, _, _) =
        spawn_test_server_for_files(&[("wakeup.lua", content)], &[], &[]).await;

    let response = test_client()
        .get(format!("http://{addr}/files/scenes/wakeup.lua"))
        .send()
        .await
        .expect("get file request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("get file text body");
    assert_eq!(body, content);

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn file_api_writes_and_reads_back() {
    let (addr, shutdown, handle, _, _, _) = spawn_test_server_for_files(&[], &[], &[]).await;

    let new_content = "return { id = 'test', name = 'Test' }";

    let put = test_client()
        .put(format!("http://{addr}/files/scenes/new.lua"))
        .json(&json!({ "content": new_content }))
        .send()
        .await
        .expect("put file request succeeds");
    assert_eq!(put.status(), StatusCode::NO_CONTENT);

    let get = test_client()
        .get(format!("http://{addr}/files/scenes/new.lua"))
        .send()
        .await
        .expect("get file request succeeds");
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.text().await.expect("get file text"), new_content);

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn file_api_rejects_path_traversal() {
    let (addr, shutdown, handle, _, _, _) = spawn_test_server_for_files(&[], &[], &[]).await;

    // Use percent-encoded slashes (%2F) so that reqwest's URL parser does
    // not normalise the ".." away before the request reaches the server.
    // Axum's Path extractor decodes the segment back to "scenes/../etc/passwd.lua"
    // and resolve_lua_path catches the ".." component → 400.
    let response = test_client()
        .get(format!(
            "http://{addr}/files/scenes%2F..%2F..%2F..%2Fetc%2Fpasswd.lua"
        ))
        .send()
        .await
        .expect("traversal request completes");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn file_api_rejects_non_lua_extension() {
    let (addr, shutdown, handle, _, _, _) = spawn_test_server_for_files(&[], &[], &[]).await;

    let response = test_client()
        .get(format!("http://{addr}/files/scenes/secret.txt"))
        .send()
        .await
        .expect("non-lua request completes");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

// ── Person picture endpoint tests ─────────────────────────────────────────────

/// Builds a server that has a live `PersonRegistry` and a temporary pictures
/// directory wired into `AppState`.  The returned `PathBuf` is the pictures dir.
async fn spawn_test_server_with_person_registry(
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf, // pictures directory
) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();

    let database_url = temp_sqlite_url();
    let store = Arc::new(
        SqliteDeviceStore::new(&database_url, true)
            .await
            .expect("sqlite store for person tests"),
    );
    let person_store: Arc<dyn PersonStore> = store.clone() as Arc<dyn PersonStore>;

    let runtime = Arc::new(Runtime::new(
        Vec::new(),
        RuntimeConfig {
            event_bus_capacity: 16,
        },
    ));

    let locale = LocaleConfig::default();
    let registry = PersonRegistry::new(person_store, runtime.bus().clone(), &locale)
        .await
        .expect("person registry initializes");

    let pictures_dir =
        std::env::temp_dir().join(format!("homecmdr-pictures-test-{unique}"));
    std::fs::create_dir_all(&pictures_dir).expect("create temp pictures dir");

    let config = test_config(serde_json::Map::new());
    let mut state = make_state(
        runtime,
        empty_scenes(),
        Arc::new(AutomationCatalog::empty()),
        TriggerContext::default(),
        test_health(&[]),
        Some(store.clone() as Arc<dyn DeviceStore>),
        None,
        sha256_hex(TEST_MASTER_KEY),
        history_settings(false),
        config.scenes.enabled,
        config.automations.enabled,
        config.scenes.directory.clone(),
        config.automations.directory.clone(),
        None,
    );
    state.person_registry = Some(Arc::new(registry));
    state.pictures_directory = pictures_dir.to_string_lossy().to_string();

    let app = app(state, &config);
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

    (addr, shutdown_tx, handle, pictures_dir)
}

#[tokio::test]
async fn person_picture_upload_stores_file_and_updates_picture_field() {
    let (addr, shutdown, handle, _pics_dir) =
        spawn_test_server_with_person_registry().await;

    // Create a person first.
    let create_resp = test_client()
        .post(format!("http://{addr}/persons"))
        .json(&serde_json::json!({ "id": "alice", "name": "Alice" }))
        .send()
        .await
        .expect("create person request succeeds");
    assert_eq!(create_resp.status(), StatusCode::OK);

    // Upload a small 1×1 white PNG.
    let png_bytes: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG signature
        0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00,
        0x90, 0x77, 0x53, 0xde, // IHDR CRC
        0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
        0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01,
        0xe2, 0x21, 0xbc, 0x33, // IDAT CRC
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82, // IEND
    ];
    let part = reqwest::multipart::Part::bytes(png_bytes.to_vec())
        .mime_str("image/png")
        .expect("valid mime");
    let form = reqwest::multipart::Form::new().part("picture", part);

    let upload_resp = test_client()
        .post(format!("http://{addr}/persons/alice/picture"))
        .multipart(form)
        .send()
        .await
        .expect("upload request succeeds");

    assert_eq!(upload_resp.status(), StatusCode::OK);
    let body = upload_resp.json::<Value>().await.expect("upload json body");
    assert_eq!(body["picture"], "/persons/alice/picture");

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn person_picture_get_returns_uploaded_bytes() {
    let (addr, shutdown, handle, _pics_dir) =
        spawn_test_server_with_person_registry().await;

    test_client()
        .post(format!("http://{addr}/persons"))
        .json(&serde_json::json!({ "id": "bob", "name": "Bob" }))
        .send()
        .await
        .expect("create person");

    // A minimal JPEG header (enough bytes to test content-type round-trip).
    let jpeg_bytes: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
        0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    let part = reqwest::multipart::Part::bytes(jpeg_bytes.to_vec())
        .mime_str("image/jpeg")
        .expect("valid mime");
    let form = reqwest::multipart::Form::new().part("picture", part);
    test_client()
        .post(format!("http://{addr}/persons/bob/picture"))
        .multipart(form)
        .send()
        .await
        .expect("upload");

    let get_resp = test_client()
        .get(format!("http://{addr}/persons/bob/picture"))
        .send()
        .await
        .expect("get picture request succeeds");

    assert_eq!(get_resp.status(), StatusCode::OK);
    let ct = get_resp
        .headers()
        .get("content-type")
        .expect("content-type header")
        .to_str()
        .expect("content-type is utf-8");
    assert!(ct.starts_with("image/jpeg"), "unexpected content-type: {ct}");
    let bytes = get_resp.bytes().await.expect("picture bytes");
    assert_eq!(bytes.as_ref(), jpeg_bytes);

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn person_picture_delete_removes_file_and_clears_field() {
    let (addr, shutdown, handle, pics_dir) =
        spawn_test_server_with_person_registry().await;

    test_client()
        .post(format!("http://{addr}/persons"))
        .json(&serde_json::json!({ "id": "carol", "name": "Carol" }))
        .send()
        .await
        .expect("create person");

    let png: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    let part = reqwest::multipart::Part::bytes(png.to_vec())
        .mime_str("image/png")
        .expect("valid mime");
    test_client()
        .post(format!("http://{addr}/persons/carol/picture"))
        .multipart(reqwest::multipart::Form::new().part("picture", part))
        .send()
        .await
        .expect("upload");

    // Verify the file is on disk.
    assert!(
        pics_dir.join("carol.png").exists(),
        "picture file should exist after upload"
    );

    let del_resp = test_client()
        .delete(format!("http://{addr}/persons/carol/picture"))
        .send()
        .await
        .expect("delete picture request succeeds");

    assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);

    // File should be gone.
    assert!(
        !pics_dir.join("carol.png").exists(),
        "picture file should be removed after delete"
    );

    // Person's picture field should be null.
    let person_resp = test_client()
        .get(format!("http://{addr}/persons/carol"))
        .send()
        .await
        .expect("get person after delete");
    let person = person_resp.json::<Value>().await.expect("person json");
    assert!(
        person["picture"].is_null(),
        "picture field should be null after delete"
    );

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn person_picture_get_returns_404_when_no_picture_uploaded() {
    let (addr, shutdown, handle, _pics_dir) =
        spawn_test_server_with_person_registry().await;

    test_client()
        .post(format!("http://{addr}/persons"))
        .json(&serde_json::json!({ "id": "dave", "name": "Dave" }))
        .send()
        .await
        .expect("create person");

    let resp = test_client()
        .get(format!("http://{addr}/persons/dave/picture"))
        .send()
        .await
        .expect("get picture request completes");

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn person_picture_upload_returns_404_for_unknown_person() {
    let (addr, shutdown, handle, _pics_dir) =
        spawn_test_server_with_person_registry().await;

    let part = reqwest::multipart::Part::bytes(vec![0u8; 4])
        .mime_str("image/png")
        .expect("valid mime");
    let resp = test_client()
        .post(format!("http://{addr}/persons/nobody/picture"))
        .multipart(reqwest::multipart::Form::new().part("picture", part))
        .send()
        .await
        .expect("upload request completes");

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}

#[tokio::test]
async fn person_picture_upload_rejects_non_image_content_type() {
    let (addr, shutdown, handle, _pics_dir) =
        spawn_test_server_with_person_registry().await;

    test_client()
        .post(format!("http://{addr}/persons"))
        .json(&serde_json::json!({ "id": "eve", "name": "Eve" }))
        .send()
        .await
        .expect("create person");

    let part = reqwest::multipart::Part::bytes(b"not an image".to_vec())
        .mime_str("text/plain")
        .expect("valid mime");
    let resp = test_client()
        .post(format!("http://{addr}/persons/eve/picture"))
        .multipart(reqwest::multipart::Form::new().part("picture", part))
        .send()
        .await
        .expect("upload request completes");

    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let _ = shutdown.send(());
    handle.await.expect("server task completes");
}
