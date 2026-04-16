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
    CapabilitySchema, CLOUD_COVERAGE, TEMPERATURE_OUTDOOR, WEATHER_CAPABILITIES, WIND_DIRECTION,
    WIND_SPEED, accumulation_value, measurement_value, weather_capability,
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
    assert!(config.adapters.open_meteo.enabled);
    assert_eq!(config.adapters.open_meteo.latitude, 51.5);
    assert_eq!(config.adapters.open_meteo.longitude, -0.1);
    assert_eq!(config.adapters.open_meteo.poll_interval_secs, 300);
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

    let error = Config::load_from_file(&path).expect_err("missing field should fail");
    let _ = fs::remove_file(&path);

    assert!(error.to_string().contains("poll_interval_secs"));
}

#[test]
fn config_rejects_poll_interval_below_minimum() {
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
poll_interval_secs = 59
"#,
    );

    let error = Config::load_from_file(&path).expect_err("invalid poll interval should fail");
    let _ = fs::remove_file(&path);

    assert_eq!(
        error.to_string(),
        "adapters.open_meteo.poll_interval_secs must be >= 60"
    );
}
