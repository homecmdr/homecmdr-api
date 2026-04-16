use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

use crate::adapter::Adapter;
use crate::bus::EventBus;
use crate::capability::{
    BRIGHTNESS, COLOR_HEX, COLOR_HS, COLOR_MODE, COLOR_MODE_VALUES, COLOR_RGB,
    COLOR_TEMPERATURE, COLOR_XY, CapabilitySchema, CLOUD_COVERAGE, EFFECT, ILLUMINANCE,
    LED_INDICATION, LIGHT_CAPABILITIES, LIGHT_EFFECT_VALUES, TEMPERATURE_OUTDOOR, TRANSITION,
    WEATHER_CAPABILITIES, WIND_DIRECTION, WIND_SPEED, accumulation_value, capability_definition,
    light_capability, measurement_value, weather_capability,
};
use crate::event::Event;
use crate::model::{AttributeValue, Device, DeviceId, DeviceKind, Metadata};
use crate::registry::DeviceRegistry;
use crate::config::Config;
use crate::runtime::{Runtime, RuntimeConfig};

struct MockAdapter;

struct PublishOnceAdapter;

struct FailingAdapter;

struct WaitingAdapter {
    started: Arc<Notify>,
}

#[async_trait]
impl Adapter for MockAdapter {
    fn name(&self) -> &str {
        "mock"
    }

    async fn run(&self, _registry: DeviceRegistry, _bus: EventBus) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Adapter for PublishOnceAdapter {
    fn name(&self) -> &str {
        "publish_once"
    }

    async fn run(&self, registry: DeviceRegistry, _bus: EventBus) -> Result<()> {
        registry
            .upsert(sample_device(
                "publish_once:temperature_outdoor",
                TEMPERATURE_OUTDOOR,
                measurement_value(19.5, "celsius"),
            ))
            .await
            .context("publish once adapter upsert failed")?;

        Ok(())
    }
}

#[async_trait]
impl Adapter for FailingAdapter {
    fn name(&self) -> &str {
        "failing"
    }

    async fn run(&self, _registry: DeviceRegistry, _bus: EventBus) -> Result<()> {
        anyhow::bail!("adapter failure")
    }
}

#[async_trait]
impl Adapter for WaitingAdapter {
    fn name(&self) -> &str {
        "waiting"
    }

    async fn run(&self, _registry: DeviceRegistry, _bus: EventBus) -> Result<()> {
        self.started.notify_one();
        std::future::pending::<()>().await;
        Ok(())
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
        updated_at: Utc::now(),
    }
}

fn write_temp_config(contents: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("smart-home-config-{unique}.toml"));

    fs::write(&path, contents).expect("write temp config file");
    path
}

#[test]
fn device_round_trips_through_json() {
    let mut attributes = HashMap::new();
    attributes.insert(
        TEMPERATURE_OUTDOOR.to_string(),
        measurement_value(21.5, "celsius"),
    );
    attributes.insert("online".to_string(), AttributeValue::Bool(true));
    attributes.insert("label".to_string(), AttributeValue::Text("patio".to_string()));
    attributes.insert(WIND_DIRECTION.to_string(), AttributeValue::Integer(225));
    attributes.insert("extra".to_string(), AttributeValue::Null);

    let mut vendor_specific = HashMap::new();
    vendor_specific.insert(
        "station_id".to_string(),
        serde_json::Value::String("abc123".to_string()),
    );

    let device = Device {
        id: DeviceId("open_meteo:temperature_outdoor".to_string()),
        kind: DeviceKind::Sensor,
        attributes,
        metadata: Metadata {
            source: "open_meteo".to_string(),
            location: Some("outdoor".to_string()),
            accuracy: Some(0.95),
            vendor_specific,
        },
        updated_at: Utc::now(),
    };

    let json = serde_json::to_string(&device).expect("serialize device");
    let decoded: Device = serde_json::from_str(&json).expect("deserialize device");

    assert_eq!(decoded, device);
}

#[tokio::test]
async fn event_bus_delivers_events_to_all_subscribers() {
    let bus = EventBus::new(16);
    let mut sub_a = bus.subscribe();
    let mut sub_b = bus.subscribe();
    let mut sub_c = bus.subscribe();

    for idx in 0..5 {
        bus.publish(Event::SystemError {
            message: format!("event-{idx}"),
        });
    }

    for idx in 0..5 {
        let expected = Event::SystemError {
            message: format!("event-{idx}"),
        };

        assert_eq!(sub_a.recv().await.expect("subscriber a receives event"), expected);
        assert_eq!(sub_b.recv().await.expect("subscriber b receives event"), expected);
        assert_eq!(sub_c.recv().await.expect("subscriber c receives event"), expected);
    }
}

#[tokio::test]
async fn slow_subscriber_lags_without_blocking_fast_subscriber() {
    let bus = EventBus::new(2);
    let mut slow = bus.subscribe();
    let mut fast = bus.subscribe();

    let fast_task: JoinHandle<Vec<Event>> = tokio::spawn(async move {
        let mut received = Vec::new();

        for _ in 0..5 {
            received.push(fast.recv().await.expect("fast subscriber receives event"));
        }

        received
    });

    let publisher = bus.clone();
    let publish_task = tokio::spawn(async move {
        for idx in 0..5 {
            publisher.publish(Event::SystemError {
                message: format!("event-{idx}"),
            });
            sleep(Duration::from_millis(1)).await;
        }
    });

    publish_task.await.expect("publisher task completes");

    assert!(matches!(slow.recv().await, Err(RecvError::Lagged(_))));

    let fast_events = fast_task.await.expect("fast subscriber task completes");
    assert_eq!(fast_events.len(), 5);

    for (idx, event) in fast_events.into_iter().enumerate() {
        assert_eq!(
            event,
            Event::SystemError {
                message: format!("event-{idx}"),
            }
        );
    }
}

#[tokio::test]
async fn upserting_new_device_publishes_device_added() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus.clone());
    let mut subscriber = bus.subscribe();
    let device = sample_device(
        "test:1",
        TEMPERATURE_OUTDOOR,
        measurement_value(20.0, "celsius"),
    );

    registry.upsert(device.clone()).await.expect("valid device upsert succeeds");

    assert_eq!(
        subscriber.recv().await.expect("device added event received"),
        Event::DeviceAdded { device }
    );
}

#[tokio::test]
async fn upserting_existing_device_publishes_state_changed() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus.clone());
    let mut subscriber = bus.subscribe();
    let original = sample_device(
        "test:1",
        TEMPERATURE_OUTDOOR,
        measurement_value(20.0, "celsius"),
    );
    let updated = sample_device(
        "test:1",
        TEMPERATURE_OUTDOOR,
        measurement_value(21.5, "celsius"),
    );

    registry.upsert(original).await.expect("valid device upsert succeeds");
    subscriber.recv().await.expect("device added event received");

    registry.upsert(updated.clone()).await.expect("valid device update succeeds");

    assert_eq!(
        subscriber.recv().await.expect("device updated event received"),
        Event::DeviceStateChanged {
            id: updated.id.clone(),
            attributes: updated.attributes.clone(),
        }
    );
}

#[tokio::test]
async fn removing_existing_device_publishes_device_removed() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus.clone());
    let mut subscriber = bus.subscribe();
    let device = sample_device(
        "test:1",
        TEMPERATURE_OUTDOOR,
        measurement_value(20.0, "celsius"),
    );

    registry.upsert(device.clone()).await.expect("valid device upsert succeeds");
    subscriber.recv().await.expect("device added event received");

    assert!(registry.remove(&device.id).await);
    assert_eq!(
        subscriber.recv().await.expect("device removed event received"),
        Event::DeviceRemoved {
            id: device.id.clone(),
        }
    );
}

#[tokio::test]
async fn removing_missing_device_returns_false_without_event() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus.clone());
    let mut subscriber = bus.subscribe();
    let missing = DeviceId("test:missing".to_string());

    assert!(!registry.remove(&missing).await);
    assert!(matches!(subscriber.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)));
}

#[tokio::test]
async fn concurrent_upserts_produce_expected_registry_size() {
    let bus = EventBus::new(32);
    let registry = DeviceRegistry::new(bus);
    let mut tasks = Vec::new();

    for idx in 0..10 {
        let registry = registry.clone();
        tasks.push(tokio::spawn(async move {
            let device = sample_device(
                &format!("test:{idx}"),
                TEMPERATURE_OUTDOOR,
                measurement_value(idx as f64, "celsius"),
            );
            registry.upsert(device).await.expect("valid concurrent upsert succeeds");
        }));
    }

    for task in tasks {
        task.await.expect("upsert task completes");
    }

    assert_eq!(registry.list().len(), 10);
}

#[tokio::test]
async fn restore_loads_devices_without_publishing_events() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus.clone());
    let mut subscriber = bus.subscribe();
    let restored = vec![sample_device(
        "test:restored",
        TEMPERATURE_OUTDOOR,
        measurement_value(23.0, "celsius"),
    )];

    registry.restore(restored.clone()).expect("restore succeeds");

    assert_eq!(registry.list(), restored);
    assert!(matches!(subscriber.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)));
}

#[tokio::test]
async fn restore_rejects_invalid_device_shape() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = sample_device(
        "test:restored-invalid",
        TEMPERATURE_OUTDOOR,
        AttributeValue::Text("bad".to_string()),
    );

    let error = registry.restore(vec![invalid]).expect_err("invalid restore should fail");

    assert!(error.to_string().contains("expected measurement object"));
}

#[test]
fn mock_adapter_implements_trait() {
    fn assert_adapter<T: Adapter>(_adapter: &T) {}

    let adapter = MockAdapter;
    assert_adapter(&adapter);
    assert_eq!(adapter.name(), "mock");
}

#[test]
fn mock_adapter_can_be_boxed_as_trait_object() {
    let adapter: Box<dyn Adapter> = Box::new(MockAdapter);

    assert_eq!(adapter.name(), "mock");
}

#[tokio::test]
async fn runtime_with_mock_adapter_populates_registry() {
    let runtime = Runtime::new(
        vec![Box::new(PublishOnceAdapter)],
        RuntimeConfig {
            event_bus_capacity: 16,
        },
    );

    runtime.run_until(sleep(Duration::from_millis(10))).await;

    assert!(runtime
        .registry()
        .get(&DeviceId("publish_once:temperature_outdoor".to_string()))
        .is_some());
}

#[test]
fn weather_capabilities_are_unique_and_typed() {
    let mut seen = std::collections::HashSet::new();

    for capability in WEATHER_CAPABILITIES {
        assert!(seen.insert(capability.key), "duplicate capability key: {}", capability.key);
        assert!(capability.read_only);
    }

    assert_eq!(
        weather_capability(TEMPERATURE_OUTDOOR).map(|capability| capability.schema),
        Some(CapabilitySchema::Measurement)
    );
    assert_eq!(
        weather_capability(WIND_SPEED).map(|capability| capability.schema),
        Some(CapabilitySchema::Measurement)
    );
    assert_eq!(
        weather_capability(WIND_DIRECTION).map(|capability| capability.schema),
        Some(CapabilitySchema::IntegerOrString)
    );
}

#[test]
fn light_capabilities_are_unique_and_typed() {
    let mut seen = std::collections::HashSet::new();

    for capability in LIGHT_CAPABILITIES {
        assert!(seen.insert(capability.key), "duplicate capability key: {}", capability.key);
    }

    assert_eq!(
        light_capability(BRIGHTNESS).map(|capability| capability.schema),
        Some(CapabilitySchema::Percentage)
    );
    assert_eq!(
        light_capability(COLOR_RGB).map(|capability| capability.schema),
        Some(CapabilitySchema::RgbColor)
    );
    assert_eq!(
        light_capability(COLOR_HEX).map(|capability| capability.schema),
        Some(CapabilitySchema::HexColor)
    );
    assert_eq!(
        capability_definition(LED_INDICATION).map(|capability| capability.schema),
        Some(CapabilitySchema::Boolean)
    );
    assert_eq!(
        capability_definition(COLOR_MODE).map(|capability| capability.schema),
        Some(CapabilitySchema::Enum(&COLOR_MODE_VALUES))
    );
    assert_eq!(
        capability_definition(EFFECT).map(|capability| capability.schema),
        Some(CapabilitySchema::Enum(&LIGHT_EFFECT_VALUES))
    );
}

#[test]
fn capability_helpers_build_expected_shapes() {
    assert_eq!(
        measurement_value(18.5, "celsius"),
        AttributeValue::Object(HashMap::from([
            ("value".to_string(), AttributeValue::Float(18.5)),
            ("unit".to_string(), AttributeValue::Text("celsius".to_string())),
        ]))
    );

    assert_eq!(
        accumulation_value(4.2, "mm", "1h"),
        AttributeValue::Object(HashMap::from([
            ("value".to_string(), AttributeValue::Float(4.2)),
            ("unit".to_string(), AttributeValue::Text("mm".to_string())),
            ("period".to_string(), AttributeValue::Text("1h".to_string())),
        ]))
    );
}

#[tokio::test]
async fn registry_accepts_valid_light_capabilities() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let device = Device {
        id: DeviceId("elgato:light:1".to_string()),
        kind: DeviceKind::Light,
        attributes: HashMap::from([
            (BRIGHTNESS.to_string(), AttributeValue::Integer(50)),
            (
                COLOR_RGB.to_string(),
                AttributeValue::Object(HashMap::from([
                    ("r".to_string(), AttributeValue::Integer(255)),
                    ("g".to_string(), AttributeValue::Integer(128)),
                    ("b".to_string(), AttributeValue::Integer(0)),
                ])),
            ),
            (COLOR_HEX.to_string(), AttributeValue::Text("#ff8000".to_string())),
            (
                COLOR_XY.to_string(),
                AttributeValue::Object(HashMap::from([
                    ("x".to_string(), AttributeValue::Float(0.5)),
                    ("y".to_string(), AttributeValue::Float(0.4)),
                ])),
            ),
            (
                COLOR_HS.to_string(),
                AttributeValue::Object(HashMap::from([
                    ("hue".to_string(), AttributeValue::Integer(120)),
                    ("saturation".to_string(), AttributeValue::Integer(80)),
                ])),
            ),
            (
                COLOR_TEMPERATURE.to_string(),
                AttributeValue::Object(HashMap::from([
                    ("value".to_string(), AttributeValue::Integer(3000)),
                    ("unit".to_string(), AttributeValue::Text("kelvin".to_string())),
                ])),
            ),
            (COLOR_MODE.to_string(), AttributeValue::Text("rgb".to_string())),
            (EFFECT.to_string(), AttributeValue::Text("none".to_string())),
            (TRANSITION.to_string(), AttributeValue::Float(1.5)),
            (ILLUMINANCE.to_string(), AttributeValue::Integer(320)),
            (LED_INDICATION.to_string(), AttributeValue::Bool(true)),
        ]),
        metadata: Metadata {
            source: "test".to_string(),
            location: Some("desk".to_string()),
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
    };

    registry.upsert(device.clone()).await.expect("valid light device should succeed");

    assert_eq!(registry.get(&device.id), Some(device));
}

#[tokio::test]
async fn registry_rejects_invalid_brightness_percentage() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = Device {
        id: DeviceId("elgato:light:bad-brightness".to_string()),
        kind: DeviceKind::Light,
        attributes: HashMap::from([(BRIGHTNESS.to_string(), AttributeValue::Integer(101))]),
        metadata: Metadata {
            source: "test".to_string(),
            location: None,
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
    };

    let error = registry.upsert(invalid).await.expect_err("invalid brightness should fail");

    assert!(error.to_string().contains("expected integer percentage between 0 and 100"));
}

#[tokio::test]
async fn registry_rejects_invalid_color_temperature_unit_or_range() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = Device {
        id: DeviceId("elgato:light:bad-temp".to_string()),
        kind: DeviceKind::Light,
        attributes: HashMap::from([(
            COLOR_TEMPERATURE.to_string(),
            AttributeValue::Object(HashMap::from([
                ("value".to_string(), AttributeValue::Integer(1000)),
                ("unit".to_string(), AttributeValue::Text("kelvin".to_string())),
            ])),
        )]),
        metadata: Metadata {
            source: "test".to_string(),
            location: None,
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
    };

    let error = registry
        .upsert(invalid)
        .await
        .expect_err("invalid color temperature should fail");

    assert!(error.to_string().contains("color temperature in kelvin must be between 2200 and 6500"));
}

#[tokio::test]
async fn registry_rejects_invalid_hex_color_shape() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = Device {
        id: DeviceId("elgato:light:bad-hex".to_string()),
        kind: DeviceKind::Light,
        attributes: HashMap::from([(COLOR_HEX.to_string(), AttributeValue::Text("orange".to_string()))]),
        metadata: Metadata {
            source: "test".to_string(),
            location: None,
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
    };

    let error = registry.upsert(invalid).await.expect_err("invalid hex color should fail");

    assert!(error.to_string().contains("expected hex color string like '#ff8800'"));
}

#[tokio::test]
async fn registry_rejects_invalid_measurement_shape() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = sample_device(
        "test:invalid-measurement",
        TEMPERATURE_OUTDOOR,
        AttributeValue::Float(20.0),
    );

    let error = registry.upsert(invalid).await.expect_err("invalid measurement should fail");

    assert!(error.to_string().contains("expected measurement object"));
}

#[tokio::test]
async fn registry_rejects_invalid_integer_shape() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = sample_device(
        "test:invalid-cloud",
        CLOUD_COVERAGE,
        measurement_value(20.0, "percent"),
    );

    let error = registry.upsert(invalid).await.expect_err("invalid integer should fail");

    assert!(error.to_string().contains("expected integer"));
}

#[tokio::test]
async fn runtime_publishes_system_error_when_adapter_fails() {
    let runtime = Runtime::new(
        vec![Box::new(FailingAdapter)],
        RuntimeConfig {
            event_bus_capacity: 16,
        },
    );
    let mut subscriber = runtime.bus().subscribe();

    runtime.run_until(sleep(Duration::from_millis(10))).await;

    assert_eq!(
        subscriber.recv().await.expect("system error event received"),
        Event::SystemError {
            message: "adapter 'failing' failed: adapter failure".to_string(),
        }
    );
}

#[tokio::test]
async fn runtime_shuts_down_cleanly_on_shutdown_signal() {
    let started = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new(
        vec![Box::new(WaitingAdapter {
            started: Arc::clone(&started),
        })],
        RuntimeConfig {
            event_bus_capacity: 16,
        },
    ));

    let runtime_task = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            runtime
                .run_until(async {
                    started.notified().await;
                    sleep(Duration::from_millis(10)).await;
                })
                .await;
        })
    };

    runtime_task.await.expect("runtime task exits cleanly");
}

#[test]
fn config_loads_default_toml() {
    let config = Config::load_from_file("/home/andy/projects/rust_home/smart-home/config/default.toml")
        .expect("default config loads successfully");

    assert_eq!(config.runtime.event_bus_capacity, 1024);
    assert_eq!(config.logging.level, "info");
    assert!(config.persistence.enabled);
    assert_eq!(config.persistence.backend, crate::config::PersistenceBackend::Sqlite);
    assert_eq!(
        config.persistence.database_url.as_deref(),
        Some("sqlite://data/smart-home.db")
    );
    assert!(config.persistence.auto_create);
    assert!(!config.telemetry.enabled);
    assert!(config.telemetry.selection.device_ids.is_empty());
    let open_meteo = config
        .adapters
        .get("open_meteo")
        .expect("open_meteo adapter config exists");
    assert_eq!(open_meteo["enabled"], serde_json::json!(true));
    assert_eq!(open_meteo["latitude"], serde_json::json!(51.5));
    assert_eq!(open_meteo["longitude"], serde_json::json!(-0.1));
    assert_eq!(open_meteo["poll_interval_secs"], serde_json::json!(90));
}

#[test]
fn config_missing_required_field_returns_clear_error() {
    let path = write_temp_config(
        r#"
[runtime]
event_bus_capacity = 1024

[logging]
level = "info"

[adapters.open_meteo]
enabled = true
latitude = 51.5
longitude = -0.1
"#,
    );

    let config = Config::load_from_file(&path).expect("generic adapter config should load");
    let _ = fs::remove_file(&path);

    let open_meteo = config
        .adapters
        .get("open_meteo")
        .expect("open_meteo adapter config exists");
    assert_eq!(open_meteo["enabled"], serde_json::json!(true));
    assert_eq!(open_meteo["latitude"], serde_json::json!(51.5));
    assert_eq!(open_meteo["longitude"], serde_json::json!(-0.1));
    assert!(open_meteo.get("poll_interval_secs").is_none());
}

#[test]
fn config_rejects_missing_database_url_when_persistence_enabled() {
    let path = write_temp_config(
        r#"
[runtime]
event_bus_capacity = 1024

[logging]
level = "info"

[persistence]
enabled = true
backend = "sqlite"
auto_create = true

[adapters.open_meteo]
enabled = true
latitude = 51.5
longitude = -0.1
poll_interval_secs = 90
"#,
    );

    let error = Config::load_from_file(&path).expect_err("missing database url should fail");
    let _ = fs::remove_file(&path);

    assert_eq!(
        error.to_string(),
        "persistence.database_url is required when persistence is enabled"
    );
}
