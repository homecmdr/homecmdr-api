use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

use crate::adapter::Adapter;
use crate::bus::EventBus;
use crate::capability::{
    accumulation_value, capability_definition, is_custom_attribute_key, light_capability,
    measurement_value, weather_capability, CapabilitySchema, AIR_QUALITY, AIR_QUALITY_VALUES,
    AVAILABILITY_VALUES, BATTERY, BRIGHTNESS, CAPABILITY_OWNERSHIP, CLOUD_COVERAGE, CO2, COLOR_HEX,
    COLOR_HS, COLOR_MODE, COLOR_MODE_VALUES, COLOR_RGB, COLOR_TEMPERATURE, COLOR_XY, CONTACT,
    CONTACT_VALUES, COVER_POSITION, COVER_TILT, CURRENT, DOOR, EFFECT, ENERGY_MONTH, ENERGY_TODAY,
    ENERGY_TOTAL, ENERGY_YESTERDAY, ENTRY_STATE_VALUES, FAN_MODE, GARAGE_DOOR, HUMIDITY, HVAC_MODE,
    HVAC_MODE_VALUES, HVAC_STATE, HVAC_STATE_VALUES, ILLUMINANCE, LED_INDICATION,
    LIGHT_CAPABILITIES, LIGHT_EFFECT_VALUES, LOCK, LOCK_VALUES, MEDIA_APP, MEDIA_PLAYBACK,
    MEDIA_PLAYBACK_VALUES, MEDIA_SOURCE, MEDIA_TITLE, MOTION, MOTION_VALUES, MUTED, OCCUPANCY,
    OCCUPANCY_VALUES, POWER, POWER_CONSUMPTION, POWER_VALUES, PRESET_MODE, PRESSURE,
    SENSOR_CAPABILITIES, SMOKE, STATE, SWING_MODE, TARGET_TEMPERATURE, TEMPERATURE,
    TEMPERATURE_OUTDOOR, TRANSITION, VOLTAGE, VOLUME, WATER_LEAK, WEATHER_CAPABILITIES,
    WIND_DIRECTION, WIND_SPEED,
};
use crate::command::DeviceCommand;
use crate::config::Config;
use crate::event::Event;
use crate::model::{
    AttributeValue, Device, DeviceGroup, DeviceId, DeviceKind, GroupId, Metadata, Room, RoomId,
};
use crate::registry::DeviceRegistry;
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
        room_id: None,
        kind: DeviceKind::Sensor,
        attributes,
        metadata: Metadata {
            source: "test".to_string(),
            accuracy: Some(1.0),
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
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
    attributes.insert(
        STATE.to_string(),
        AttributeValue::Text("online".to_string()),
    );
    attributes.insert(
        "custom.test_adapter.label".to_string(),
        AttributeValue::Text("patio".to_string()),
    );
    attributes.insert(WIND_DIRECTION.to_string(), AttributeValue::Integer(225));
    attributes.insert(
        "custom.test_adapter.extra".to_string(),
        AttributeValue::Null,
    );

    let mut vendor_specific = HashMap::new();
    vendor_specific.insert(
        "station_id".to_string(),
        serde_json::Value::String("abc123".to_string()),
    );

    let device = Device {
        id: DeviceId("test_adapter:sensor:outdoor".to_string()),
        room_id: None,
        kind: DeviceKind::Sensor,
        attributes,
        metadata: Metadata {
            source: "test_adapter".to_string(),
            accuracy: Some(0.95),
            vendor_specific,
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
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

        assert_eq!(
            sub_a.recv().await.expect("subscriber a receives event"),
            expected
        );
        assert_eq!(
            sub_b.recv().await.expect("subscriber b receives event"),
            expected
        );
        assert_eq!(
            sub_c.recv().await.expect("subscriber c receives event"),
            expected
        );
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

    registry
        .upsert(device.clone())
        .await
        .expect("valid device upsert succeeds");

    assert_eq!(
        subscriber
            .recv()
            .await
            .expect("device added event received"),
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

    registry
        .upsert(original)
        .await
        .expect("valid device upsert succeeds");
    subscriber
        .recv()
        .await
        .expect("device added event received");

    registry
        .upsert(updated.clone())
        .await
        .expect("valid device update succeeds");

    assert_eq!(
        subscriber
            .recv()
            .await
            .expect("device updated event received"),
        Event::DeviceStateChanged {
            id: updated.id.clone(),
            attributes: updated.attributes.clone(),
            previous_attributes: HashMap::from([(
                TEMPERATURE_OUTDOOR.to_string(),
                measurement_value(20.0, "celsius"),
            )]),
        }
    );
}

#[tokio::test]
async fn upserting_identical_state_only_updates_last_seen_without_state_changed_event() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus.clone());
    let mut subscriber = bus.subscribe();
    let original = sample_device(
        "test:1",
        TEMPERATURE_OUTDOOR,
        measurement_value(20.0, "celsius"),
    );

    registry
        .upsert(original.clone())
        .await
        .expect("initial device insert succeeds");
    subscriber
        .recv()
        .await
        .expect("device added event received");

    let mut seen_again = original.clone();
    seen_again.last_seen = seen_again.last_seen + chrono::TimeDelta::seconds(30);
    registry
        .upsert(seen_again.clone())
        .await
        .expect("seen-again device upsert succeeds");

    assert_eq!(
        subscriber.recv().await.expect("device seen event received"),
        Event::DeviceSeen {
            id: seen_again.id.clone(),
            last_seen: seen_again.last_seen,
        }
    );
    assert_eq!(
        registry
            .get(&seen_again.id)
            .expect("device still exists")
            .updated_at,
        original.updated_at
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

    registry
        .upsert(device.clone())
        .await
        .expect("valid device upsert succeeds");
    subscriber
        .recv()
        .await
        .expect("device added event received");

    assert!(registry.remove(&device.id).await);
    assert_eq!(
        subscriber
            .recv()
            .await
            .expect("device removed event received"),
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
    assert!(matches!(
        subscriber.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
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
            registry
                .upsert(device)
                .await
                .expect("valid concurrent upsert succeeds");
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

    registry
        .restore(restored.clone())
        .expect("restore succeeds");

    assert_eq!(registry.list(), restored);
    assert!(matches!(
        subscriber.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn assigning_device_to_room_updates_registry() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let room = Room {
        id: RoomId("outside".to_string()),
        name: "Outside".to_string(),
    };
    let device = sample_device(
        "test:roomed",
        TEMPERATURE_OUTDOOR,
        measurement_value(23.0, "celsius"),
    );

    registry.upsert_room(room.clone()).await;
    registry
        .upsert(device.clone())
        .await
        .expect("device insert succeeds");
    registry
        .assign_device_to_room(&device.id, Some(room.id.clone()))
        .await
        .expect("assignment succeeds");

    assert_eq!(registry.list_devices_in_room(&room.id).len(), 1);
    assert_eq!(
        registry.get(&device.id).expect("device exists").room_id,
        Some(room.id)
    );
}

#[tokio::test]
async fn creating_group_with_existing_members_succeeds() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let device = sample_device(
        "test:grouped",
        TEMPERATURE_OUTDOOR,
        measurement_value(22.0, "celsius"),
    );
    registry
        .upsert(device.clone())
        .await
        .expect("device insert succeeds");

    let group = DeviceGroup {
        id: GroupId("bedroom_lamps".to_string()),
        name: "Bedroom Lamps".to_string(),
        members: vec![device.id.clone()],
    };

    registry
        .upsert_group(group.clone())
        .await
        .expect("group upsert succeeds");

    assert_eq!(registry.get_group(&group.id), Some(group));
}

#[tokio::test]
async fn creating_group_with_missing_member_is_rejected() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);

    let error = registry
        .upsert_group(DeviceGroup {
            id: GroupId("bedroom_lamps".to_string()),
            name: "Bedroom Lamps".to_string(),
            members: vec![DeviceId("test:missing".to_string())],
        })
        .await
        .expect_err("group with unknown member should fail");

    assert!(error
        .to_string()
        .contains("references unknown device 'test:missing'"));
}

#[tokio::test]
async fn removing_device_prunes_group_membership() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let device = sample_device(
        "test:grouped",
        TEMPERATURE_OUTDOOR,
        measurement_value(22.0, "celsius"),
    );
    registry
        .upsert(device.clone())
        .await
        .expect("device insert succeeds");
    registry
        .upsert_group(DeviceGroup {
            id: GroupId("bedroom_lamps".to_string()),
            name: "Bedroom Lamps".to_string(),
            members: vec![device.id.clone()],
        })
        .await
        .expect("group upsert succeeds");

    assert!(registry.remove(&device.id).await);

    let group = registry
        .get_group(&GroupId("bedroom_lamps".to_string()))
        .expect("group remains present");
    assert!(group.members.is_empty());
}

#[tokio::test]
async fn restore_groups_prunes_members_whose_devices_no_longer_exist() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);

    // Restore one live device.
    let live = sample_device(
        "test:live",
        TEMPERATURE_OUTDOOR,
        measurement_value(22.0, "celsius"),
    );
    registry
        .restore(vec![live.clone()])
        .expect("device restore succeeds");

    // Restore a group whose membership includes the live device and a
    // stale device that no longer exists in the store.
    registry.restore_groups(vec![DeviceGroup {
        id: GroupId("mixed_group".to_string()),
        name: "Mixed Group".to_string(),
        members: vec![live.id.clone(), DeviceId("test:stale".to_string())],
    }]);

    let group = registry
        .get_group(&GroupId("mixed_group".to_string()))
        .expect("group is present after restore");
    assert_eq!(
        group.members,
        vec![live.id],
        "stale member should be pruned; live member should remain"
    );
}

#[tokio::test]
async fn upserting_device_with_unknown_room_is_rejected() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let mut device = sample_device(
        "test:unknown-room",
        TEMPERATURE_OUTDOOR,
        measurement_value(20.0, "celsius"),
    );
    device.room_id = Some(RoomId("missing".to_string()));

    let error = registry
        .upsert(device)
        .await
        .expect_err("unknown room should fail");

    assert!(error
        .to_string()
        .contains("references unknown room 'missing'"));
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

    let error = registry
        .restore(vec![invalid])
        .expect_err("invalid restore should fail");

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
        assert!(
            seen.insert(capability.key),
            "duplicate capability key: {}",
            capability.key
        );
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
        assert!(
            seen.insert(capability.key),
            "duplicate capability key: {}",
            capability.key
        );
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
    assert_eq!(
        capability_definition(POWER).map(|capability| capability.schema),
        Some(CapabilitySchema::Enum(&POWER_VALUES))
    );
    assert_eq!(
        capability_definition(STATE).map(|capability| capability.schema),
        Some(CapabilitySchema::Enum(&AVAILABILITY_VALUES))
    );
}

#[test]
fn expanded_capabilities_are_discoverable_and_typed() {
    let expected = [
        (BATTERY, CapabilitySchema::Percentage),
        (TEMPERATURE, CapabilitySchema::Measurement),
        (HUMIDITY, CapabilitySchema::Measurement),
        (PRESSURE, CapabilitySchema::Measurement),
        (CO2, CapabilitySchema::Measurement),
        (AIR_QUALITY, CapabilitySchema::Enum(&AIR_QUALITY_VALUES)),
        (MOTION, CapabilitySchema::Enum(&MOTION_VALUES)),
        (CONTACT, CapabilitySchema::Enum(&CONTACT_VALUES)),
        (OCCUPANCY, CapabilitySchema::Enum(&OCCUPANCY_VALUES)),
        (SMOKE, CapabilitySchema::Boolean),
        (WATER_LEAK, CapabilitySchema::Boolean),
        (TARGET_TEMPERATURE, CapabilitySchema::Measurement),
        (HVAC_MODE, CapabilitySchema::Enum(&HVAC_MODE_VALUES)),
        (HVAC_STATE, CapabilitySchema::Enum(&HVAC_STATE_VALUES)),
        (FAN_MODE, CapabilitySchema::String),
        (SWING_MODE, CapabilitySchema::String),
        (PRESET_MODE, CapabilitySchema::String),
        (POWER_CONSUMPTION, CapabilitySchema::Measurement),
        (ENERGY_TOTAL, CapabilitySchema::Accumulation),
        (ENERGY_TODAY, CapabilitySchema::Accumulation),
        (ENERGY_YESTERDAY, CapabilitySchema::Accumulation),
        (ENERGY_MONTH, CapabilitySchema::Accumulation),
        (VOLTAGE, CapabilitySchema::Measurement),
        (CURRENT, CapabilitySchema::Measurement),
        (VOLUME, CapabilitySchema::Percentage),
        (MEDIA_SOURCE, CapabilitySchema::String),
        (MEDIA_TITLE, CapabilitySchema::String),
        (MEDIA_APP, CapabilitySchema::String),
        (
            MEDIA_PLAYBACK,
            CapabilitySchema::Enum(&MEDIA_PLAYBACK_VALUES),
        ),
        (LOCK, CapabilitySchema::Enum(&LOCK_VALUES)),
        (DOOR, CapabilitySchema::Enum(&ENTRY_STATE_VALUES)),
        (COVER_POSITION, CapabilitySchema::Percentage),
        (COVER_TILT, CapabilitySchema::Percentage),
    ];

    for (key, schema) in expected {
        assert_eq!(
            capability_definition(key).map(|capability| capability.schema),
            Some(schema),
            "capability '{key}' should be discoverable with the expected schema"
        );
    }
}

#[test]
fn capability_domains_and_ownership_policy_are_consistent() {
    let mut seen = std::collections::HashSet::new();

    for capability in crate::capability::ALL_CAPABILITIES
        .iter()
        .flat_map(|group| group.iter())
    {
        assert!(
            seen.insert(capability.key),
            "duplicate capability key: {}",
            capability.key
        );
        assert!(!capability.domain.is_empty());
    }

    assert!(
        SENSOR_CAPABILITIES
            .iter()
            .all(|capability| capability.read_only),
        "sensor capabilities should be read-only"
    );
    assert_eq!(
        CAPABILITY_OWNERSHIP.canonical_attribute_location,
        "device.attributes.<capability_key>"
    );
    assert_eq!(CAPABILITY_OWNERSHIP.custom_attribute_prefix, "custom.");
    assert_eq!(
        CAPABILITY_OWNERSHIP.vendor_metadata_field,
        "metadata.vendor_specific"
    );
    assert_eq!(CAPABILITY_OWNERSHIP.rules.len(), 4);
}

#[test]
fn capability_helpers_build_expected_shapes() {
    assert_eq!(
        measurement_value(18.5, "celsius"),
        AttributeValue::Object(HashMap::from([
            ("value".to_string(), AttributeValue::Float(18.5)),
            (
                "unit".to_string(),
                AttributeValue::Text("celsius".to_string())
            ),
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
async fn registry_accepts_expanded_canonical_capabilities() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let device = Device {
        id: DeviceId("test:capabilities:expanded".to_string()),
        room_id: None,
        kind: DeviceKind::Sensor,
        attributes: HashMap::from([
            (BATTERY.to_string(), AttributeValue::Integer(82)),
            (TEMPERATURE.to_string(), measurement_value(21.5, "celsius")),
            (HUMIDITY.to_string(), measurement_value(48.0, "percent")),
            (PRESSURE.to_string(), measurement_value(1013.2, "hpa")),
            (CO2.to_string(), measurement_value(640.0, "ppm")),
            (
                AIR_QUALITY.to_string(),
                AttributeValue::Text("good".to_string()),
            ),
            (
                MOTION.to_string(),
                AttributeValue::Text("clear".to_string()),
            ),
            (
                CONTACT.to_string(),
                AttributeValue::Text("closed".to_string()),
            ),
            (
                OCCUPANCY.to_string(),
                AttributeValue::Text("occupied".to_string()),
            ),
            (SMOKE.to_string(), AttributeValue::Bool(false)),
            (WATER_LEAK.to_string(), AttributeValue::Bool(false)),
            (
                TARGET_TEMPERATURE.to_string(),
                measurement_value(22.0, "celsius"),
            ),
            (
                HVAC_MODE.to_string(),
                AttributeValue::Text("auto".to_string()),
            ),
            (
                HVAC_STATE.to_string(),
                AttributeValue::Text("heating".to_string()),
            ),
            (
                FAN_MODE.to_string(),
                AttributeValue::Text("medium".to_string()),
            ),
            (
                SWING_MODE.to_string(),
                AttributeValue::Text("vertical".to_string()),
            ),
            (
                PRESET_MODE.to_string(),
                AttributeValue::Text("home".to_string()),
            ),
            (
                POWER_CONSUMPTION.to_string(),
                measurement_value(87.5, "watt"),
            ),
            (
                ENERGY_TOTAL.to_string(),
                accumulation_value(328.4, "kwh", "lifetime"),
            ),
            (
                ENERGY_TODAY.to_string(),
                accumulation_value(4.2, "kwh", "day"),
            ),
            (
                ENERGY_YESTERDAY.to_string(),
                accumulation_value(5.1, "kwh", "day"),
            ),
            (
                ENERGY_MONTH.to_string(),
                accumulation_value(112.6, "kwh", "month"),
            ),
            (VOLTAGE.to_string(), measurement_value(120.3, "volt")),
            (CURRENT.to_string(), measurement_value(0.72, "ampere")),
            (VOLUME.to_string(), AttributeValue::Integer(35)),
            (MUTED.to_string(), AttributeValue::Bool(false)),
            (
                MEDIA_SOURCE.to_string(),
                AttributeValue::Text("hdmi1".to_string()),
            ),
            (
                MEDIA_TITLE.to_string(),
                AttributeValue::Text("Nature Documentary".to_string()),
            ),
            (
                MEDIA_APP.to_string(),
                AttributeValue::Text("plex".to_string()),
            ),
            (
                MEDIA_PLAYBACK.to_string(),
                AttributeValue::Text("playing".to_string()),
            ),
            (LOCK.to_string(), AttributeValue::Text("locked".to_string())),
            (DOOR.to_string(), AttributeValue::Text("closed".to_string())),
            (
                GARAGE_DOOR.to_string(),
                AttributeValue::Text("stopped".to_string()),
            ),
            (COVER_POSITION.to_string(), AttributeValue::Integer(50)),
            (COVER_TILT.to_string(), AttributeValue::Integer(15)),
        ]),
        metadata: Metadata {
            source: "test".to_string(),
            accuracy: None,
            vendor_specific: HashMap::from([(
                "friendly_name".to_string(),
                serde_json::Value::String("Hallway sensor".to_string()),
            )]),
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
    };

    registry
        .upsert(device.clone())
        .await
        .expect("valid expanded canonical device should succeed");

    assert_eq!(registry.get(&device.id), Some(device));
}

#[test]
fn device_command_accepts_lock_action_without_value() {
    DeviceCommand {
        capability: LOCK.to_string(),
        action: "lock".to_string(),
        value: None,
        transition_secs: None,
    }
    .validate()
    .expect("lock command should validate");
}

#[test]
fn device_command_accepts_media_source_set_with_value() {
    DeviceCommand {
        capability: MEDIA_SOURCE.to_string(),
        action: "set".to_string(),
        value: Some(AttributeValue::Text("hdmi2".to_string())),
        transition_secs: None,
    }
    .validate()
    .expect("media source set command should validate");
}

#[test]
fn device_command_rejects_reset_with_value() {
    let error = DeviceCommand {
        capability: ENERGY_TOTAL.to_string(),
        action: "reset".to_string(),
        value: Some(AttributeValue::Integer(0)),
        transition_secs: None,
    }
    .validate()
    .err()
    .expect("energy reset should not accept a value");

    assert_eq!(
        error.to_string(),
        "command action 'reset' for capability 'energy_total' does not accept a value"
    );
}

#[test]
fn custom_attribute_keys_require_adapter_namespace() {
    assert!(is_custom_attribute_key("custom.test_adapter.label"));
    assert!(is_custom_attribute_key(
        "custom.test_adapter_b.input_source"
    ));
    assert!(is_custom_attribute_key(
        "custom.test_adapter_c.effect_profile.active"
    ));

    assert!(!is_custom_attribute_key("custom"));
    assert!(!is_custom_attribute_key("custom."));
    assert!(!is_custom_attribute_key("custom.label"));
    assert!(!is_custom_attribute_key("label.custom"));
    assert!(!is_custom_attribute_key("custom.open-meteo.label"));
    assert!(!is_custom_attribute_key("custom.test_adapter.Label"));
}

#[tokio::test]
async fn registry_accepts_valid_light_capabilities() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let device = Device {
        id: DeviceId("test_adapter:light:1".to_string()),
        room_id: None,
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
            (
                COLOR_HEX.to_string(),
                AttributeValue::Text("#ff8000".to_string()),
            ),
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
                    (
                        "unit".to_string(),
                        AttributeValue::Text("kelvin".to_string()),
                    ),
                ])),
            ),
            (POWER.to_string(), AttributeValue::Text("on".to_string())),
            (
                STATE.to_string(),
                AttributeValue::Text("online".to_string()),
            ),
            (
                COLOR_MODE.to_string(),
                AttributeValue::Text("rgb".to_string()),
            ),
            (EFFECT.to_string(), AttributeValue::Text("none".to_string())),
            (TRANSITION.to_string(), AttributeValue::Float(1.5)),
            (ILLUMINANCE.to_string(), AttributeValue::Integer(320)),
            (LED_INDICATION.to_string(), AttributeValue::Bool(true)),
        ]),
        metadata: Metadata {
            source: "test".to_string(),
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
    };

    registry
        .upsert(device.clone())
        .await
        .expect("valid light device should succeed");

    assert_eq!(registry.get(&device.id), Some(device));
}

#[tokio::test]
async fn registry_rejects_invalid_brightness_percentage() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = Device {
        id: DeviceId("test_adapter:light:bad-brightness".to_string()),
        room_id: None,
        kind: DeviceKind::Light,
        attributes: HashMap::from([(BRIGHTNESS.to_string(), AttributeValue::Integer(101))]),
        metadata: Metadata {
            source: "test".to_string(),
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
    };

    let error = registry
        .upsert(invalid)
        .await
        .expect_err("invalid brightness should fail");

    assert!(error
        .to_string()
        .contains("expected integer percentage between 0 and 100"));
}

#[tokio::test]
async fn registry_rejects_invalid_color_temperature_unit_or_range() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = Device {
        id: DeviceId("test_adapter:light:bad-temp".to_string()),
        room_id: None,
        kind: DeviceKind::Light,
        attributes: HashMap::from([(
            COLOR_TEMPERATURE.to_string(),
            AttributeValue::Object(HashMap::from([
                ("value".to_string(), AttributeValue::Integer(1000)),
                (
                    "unit".to_string(),
                    AttributeValue::Text("kelvin".to_string()),
                ),
            ])),
        )]),
        metadata: Metadata {
            source: "test".to_string(),
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
    };

    let error = registry
        .upsert(invalid)
        .await
        .expect_err("invalid color temperature should fail");

    assert!(error
        .to_string()
        .contains("color temperature in kelvin must be between 2200 and 7000"));
}

#[tokio::test]
async fn registry_rejects_invalid_hex_color_shape() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = Device {
        id: DeviceId("test_adapter:light:bad-hex".to_string()),
        room_id: None,
        kind: DeviceKind::Light,
        attributes: HashMap::from([(
            COLOR_HEX.to_string(),
            AttributeValue::Text("orange".to_string()),
        )]),
        metadata: Metadata {
            source: "test".to_string(),
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
    };

    let error = registry
        .upsert(invalid)
        .await
        .expect_err("invalid hex color should fail");

    assert!(error
        .to_string()
        .contains("expected hex color string like '#ff8800'"));
}

#[tokio::test]
async fn registry_accepts_custom_attribute_keys() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let device = Device {
        id: DeviceId("test:custom-attributes".to_string()),
        room_id: None,
        kind: DeviceKind::Sensor,
        attributes: HashMap::from([
            (
                TEMPERATURE_OUTDOOR.to_string(),
                measurement_value(21.5, "celsius"),
            ),
            (
                "custom.test_adapter.station_label".to_string(),
                AttributeValue::Text("patio".to_string()),
            ),
        ]),
        metadata: Metadata {
            source: "test".to_string(),
            accuracy: None,
            vendor_specific: HashMap::from([(
                "station_id".to_string(),
                serde_json::json!("abc123"),
            )]),
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
    };

    registry
        .upsert(device.clone())
        .await
        .expect("custom namespaced attributes should validate");

    assert_eq!(registry.get(&device.id), Some(device));
}

#[tokio::test]
async fn registry_rejects_unknown_non_custom_attribute_keys() {
    let bus = EventBus::new(16);
    let registry = DeviceRegistry::new(bus);
    let invalid = Device {
        id: DeviceId("test:unknown-attribute".to_string()),
        room_id: None,
        kind: DeviceKind::Sensor,
        attributes: HashMap::from([(
            "label".to_string(),
            AttributeValue::Text("patio".to_string()),
        )]),
        metadata: Metadata {
            source: "test".to_string(),
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: Utc::now(),
        last_seen: Utc::now(),
    };

    let error = registry
        .upsert(invalid)
        .await
        .expect_err("unknown plain attribute should fail");

    assert!(error.to_string().contains("unknown attribute 'label'"));
    assert!(error.to_string().contains("custom.<adapter>.<field>"));
}

#[test]
fn device_command_validates_power_toggle_without_value() {
    DeviceCommand {
        capability: POWER.to_string(),
        action: "toggle".to_string(),
        value: None,
        transition_secs: None,
    }
    .validate()
    .expect("power toggle command should validate");
}

#[test]
fn device_command_rejects_set_without_value() {
    let error = DeviceCommand {
        capability: BRIGHTNESS.to_string(),
        action: "set".to_string(),
        value: None,
        transition_secs: None,
    }
    .validate()
    .err()
    .expect("missing set value should fail");

    assert_eq!(
        error.to_string(),
        "command action 'set' for capability 'brightness' requires a value"
    );
}

#[test]
fn device_command_rejects_value_for_power_on() {
    let error = DeviceCommand {
        capability: POWER.to_string(),
        action: "on".to_string(),
        value: Some(AttributeValue::Text("on".to_string())),
        transition_secs: None,
    }
    .validate()
    .err()
    .expect("power on should not accept a value");

    assert_eq!(
        error.to_string(),
        "command action 'on' for capability 'power' does not accept a value"
    );
}

#[test]
fn device_command_accepts_kelvin_7000() {
    DeviceCommand {
        capability: COLOR_TEMPERATURE.to_string(),
        action: "set".to_string(),
        value: Some(AttributeValue::Object(HashMap::from([
            ("value".to_string(), AttributeValue::Integer(7000)),
            (
                "unit".to_string(),
                AttributeValue::Text("kelvin".to_string()),
            ),
        ]))),
        transition_secs: None,
    }
    .validate()
    .expect("7000 kelvin should validate");
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

    let error = registry
        .upsert(invalid)
        .await
        .expect_err("invalid measurement should fail");

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

    let error = registry
        .upsert(invalid)
        .await
        .expect_err("invalid integer should fail");

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
        subscriber
            .recv()
            .await
            .expect("system error event received"),
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
    let config =
        Config::load_from_file("/home/andy/projects/rust_home/smart-home/config/default.toml")
            .expect("default config loads successfully");

    assert_eq!(config.runtime.event_bus_capacity, 1024);
    assert_eq!(config.logging.level, "info");
    assert!(config.persistence.enabled);
    assert_eq!(
        config.persistence.backend,
        crate::config::PersistenceBackend::Sqlite
    );
    assert_eq!(
        config.persistence.database_url.as_deref(),
        Some("sqlite://data/smart-home.db")
    );
    assert!(config.persistence.auto_create);
    assert!(config.persistence.history.enabled);
    assert_eq!(config.persistence.history.retention_days, Some(30));
    assert_eq!(config.persistence.history.default_query_limit, 200);
    assert_eq!(config.persistence.history.max_query_limit, 1000);
    assert!(config.api.cors.enabled);
    assert_eq!(
        config.api.cors.allowed_origins,
        vec!["http://127.0.0.1:8080".to_string()]
    );
    assert!(!config.telemetry.enabled);
    assert!(config.telemetry.selection.device_ids.is_empty());
    // Verify the adapters map is present and each entry is a JSON object — field-level
    // assertions for specific adapters belong in each adapter crate's own tests.
    assert!(!config.adapters.is_empty());
    for (_name, value) in &config.adapters {
        assert!(
            value.is_object(),
            "each adapter config entry must be a JSON object"
        );
    }
}

#[test]
fn config_loads_arbitrary_adapter_config_as_generic_json() {
    let path = write_temp_config(
        r#"
[runtime]
event_bus_capacity = 1024

[logging]
level = "info"

[adapters.test_adapter]
enabled = true
some_field = "hello"
numeric_field = 42
"#,
    );

    let config = Config::load_from_file(&path).expect("generic adapter config should load");
    let _ = fs::remove_file(&path);

    let adapter = config
        .adapters
        .get("test_adapter")
        .expect("test_adapter config entry should exist");
    assert!(
        adapter.is_object(),
        "adapter config should be a JSON object"
    );
    assert_eq!(adapter["enabled"], serde_json::json!(true));
    assert_eq!(adapter["some_field"], serde_json::json!("hello"));
    assert_eq!(adapter["numeric_field"], serde_json::json!(42));
    assert!(adapter.get("absent_field").is_none());
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

[adapters.test_adapter]
enabled = true
some_field = "value"
"#,
    );

    let error = Config::load_from_file(&path).expect_err("missing database url should fail");
    let _ = fs::remove_file(&path);

    assert_eq!(
        error.to_string(),
        "persistence.database_url is required when persistence is enabled"
    );
}

#[test]
fn config_rejects_enabled_cors_without_allowed_origins() {
    let path = write_temp_config(
        r#"
[runtime]
event_bus_capacity = 1024

[api.cors]
enabled = true

[logging]
level = "info"
"#,
    );

    let error = Config::load_from_file(&path).expect_err("missing cors origins should fail");
    let _ = fs::remove_file(&path);

    assert_eq!(
        error.to_string(),
        "api.cors.allowed_origins must not be empty when api.cors.enabled is true"
    );
}

#[test]
fn config_rejects_cors_origin_with_path() {
    let path = write_temp_config(
        r#"
[runtime]
event_bus_capacity = 1024

[api.cors]
enabled = true
allowed_origins = ["http://127.0.0.1:8080/dashboard"]

[logging]
level = "info"
"#,
    );

    let error = Config::load_from_file(&path).expect_err("cors origin with path should fail");
    let _ = fs::remove_file(&path);

    assert_eq!(
        error.to_string(),
        "api.cors.allowed_origins must be bare origins without path, query, or fragment: 'http://127.0.0.1:8080/dashboard'"
    );
}

#[test]
fn config_accepts_enabled_cors_with_explicit_origins() {
    let path = write_temp_config(
        r#"
[runtime]
event_bus_capacity = 1024

[api.cors]
enabled = true
allowed_origins = ["http://127.0.0.1:8080", "http://localhost:8080"]

[logging]
level = "info"
"#,
    );

    let config = Config::load_from_file(&path).expect("valid cors config should load");
    let _ = fs::remove_file(&path);

    assert!(config.api.cors.enabled);
    assert_eq!(
        config.api.cors.allowed_origins,
        vec![
            "http://127.0.0.1:8080".to_string(),
            "http://localhost:8080".to_string()
        ]
    );
}
