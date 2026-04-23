use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{Datelike, DateTime, Duration as ChronoDuration, NaiveDate, TimeZone, Timelike, Utc};
use cron::Schedule;
use homecmdr_core::adapter::Adapter;
use homecmdr_core::bus::EventBus;
use homecmdr_core::command::DeviceCommand;
use homecmdr_core::model::{
    AttributeValue, Device, DeviceGroup, DeviceId, DeviceKind, GroupId, Metadata, Room, RoomId,
};
use homecmdr_core::registry::DeviceRegistry;
use homecmdr_core::runtime::{Runtime, RuntimeConfig};
use homecmdr_core::store::{AutomationExecutionHistoryEntry, AutomationRuntimeState, DeviceStore};
use homecmdr_lua_host::{DEFAULT_MAX_INSTRUCTIONS, ExecutionMode};
use tokio::time::{sleep, timeout, Duration};

use super::*;
use crate::conditions::evaluate_condition;
use crate::runner::{execute_automation, ExecutionControl};
use crate::schedule::{next_schedule_time, next_wall_clock_occurrence};
use crate::state::{next_scheduled_fire_after, AutomationStateStore};
use crate::triggers::device::recover_lagged_event_automations;
use crate::types::{Condition, Trigger, DEFAULT_BACKSTOP_TIMEOUT};

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
    async fn load_all_devices(&self) -> anyhow::Result<Vec<Device>> {
        Ok(Vec::new())
    }
    async fn load_all_rooms(&self) -> anyhow::Result<Vec<Room>> {
        Ok(Vec::new())
    }
    async fn load_all_groups(&self) -> anyhow::Result<Vec<DeviceGroup>> {
        Ok(Vec::new())
    }
    async fn save_device(&self, _device: &Device) -> anyhow::Result<()> {
        Ok(())
    }
    async fn save_room(&self, _room: &Room) -> anyhow::Result<()> {
        Ok(())
    }
    async fn save_group(&self, _group: &DeviceGroup) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_device(&self, _id: &DeviceId) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_room(&self, _id: &RoomId) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_group(&self, _id: &GroupId) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_device_history(
        &self,
        _id: &DeviceId,
        _start: Option<DateTime<Utc>>,
        _end: Option<DateTime<Utc>>,
        _limit: usize,
    ) -> anyhow::Result<Vec<homecmdr_core::store::DeviceHistoryEntry>> {
        Ok(Vec::new())
    }
    async fn load_attribute_history(
        &self,
        _id: &DeviceId,
        _attribute: &str,
        _start: Option<DateTime<Utc>>,
        _end: Option<DateTime<Utc>>,
        _limit: usize,
    ) -> anyhow::Result<Vec<homecmdr_core::store::AttributeHistoryEntry>> {
        Ok(Vec::new())
    }
    async fn save_command_audit(
        &self,
        _entry: &homecmdr_core::store::CommandAuditEntry,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_command_audit(
        &self,
        _device_id: Option<&DeviceId>,
        _start: Option<DateTime<Utc>>,
        _end: Option<DateTime<Utc>>,
        _limit: usize,
    ) -> anyhow::Result<Vec<homecmdr_core::store::CommandAuditEntry>> {
        Ok(Vec::new())
    }
    async fn save_scene_execution(
        &self,
        _entry: &homecmdr_core::store::SceneExecutionHistoryEntry,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_scene_history(
        &self,
        _scene_id: &str,
        _start: Option<DateTime<Utc>>,
        _end: Option<DateTime<Utc>>,
        _limit: usize,
    ) -> anyhow::Result<Vec<homecmdr_core::store::SceneExecutionHistoryEntry>> {
        Ok(Vec::new())
    }
    async fn save_automation_execution(
        &self,
        _entry: &AutomationExecutionHistoryEntry,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_automation_history(
        &self,
        _automation_id: &str,
        _start: Option<DateTime<Utc>>,
        _end: Option<DateTime<Utc>>,
        _limit: usize,
    ) -> anyhow::Result<Vec<AutomationExecutionHistoryEntry>> {
        Ok(Vec::new())
    }
    async fn load_automation_runtime_state(
        &self,
        automation_id: &str,
    ) -> anyhow::Result<Option<AutomationRuntimeState>> {
        Ok(self
            .automation_state
            .lock()
            .expect("automation state lock")
            .get(automation_id)
            .cloned())
    }
    async fn save_automation_runtime_state(
        &self,
        state: &AutomationRuntimeState,
    ) -> anyhow::Result<()> {
        self.automation_state
            .lock()
            .expect("automation state lock")
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
                (
                    "unit".to_string(),
                    AttributeValue::Text("celsius".to_string()),
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
    }
}

fn attribute_device(id: &str, attribute: &str, value: AttributeValue) -> Device {
    Device {
        id: DeviceId(id.to_string()),
        room_id: None,
        kind: DeviceKind::Sensor,
        attributes: HashMap::from([(attribute.to_string(), value)]),
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

#[test]
fn loads_device_trigger_automation_catalog() {
    let dir = temp_dir("homecmdr-automations");
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
    let dir = temp_dir("homecmdr-automations");
    write_automation(
        &dir,
        "event.lua",
        r#"return {
            id = "event_watch",
            name = "Event Watch",
            trigger = {
                type = "adapter_lifecycle",
                adapter = "test",
                event = "started",
            },
            conditions = {
                {
                    type = "sun_position",
                    after = "sunset",
                },
            },
            execute = function(ctx, event)
            end,
        }"#,
    );

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let trigger_types = catalog
        .summaries()
        .into_iter()
        .map(|summary| (summary.trigger_type, summary.condition_count))
        .collect::<Vec<_>>();
    assert!(trigger_types.contains(&("adapter_lifecycle", 1)));
}

#[test]
fn loads_wall_clock_and_cron_trigger_catalog() {
    let dir = temp_dir("homecmdr-automations");
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
    let dir = temp_dir("homecmdr-automations");
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
    let dir = temp_dir("homecmdr-automations");
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
    let dir = temp_dir("homecmdr-automations");
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

    let error =
        AutomationCatalog::load_from_directory(&dir, None).expect_err("catalog should fail");
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

    let next = next_wall_clock_occurrence(6, 30, after, TriggerContext::default())
        .expect("next schedule exists");
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

    let next = next_schedule_time(&trigger, after, TriggerContext::default())
        .expect("next schedule exists");
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
        timezone: None,
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
    let dir = temp_dir("homecmdr-automations");
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
    let dir = temp_dir("homecmdr-automations");
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
        RuntimeConfig {
            event_bus_capacity: 32,
        },
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
    let dir = temp_dir("homecmdr-automations");
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
        .expect("target exists");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let runner = AutomationRunner::new(catalog);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("trigger on");
    sleep(Duration::from_millis(300)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("trigger reset");
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

    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("trigger on again");
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
    let dir = temp_dir("homecmdr-automations");
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
        RuntimeConfig {
            event_bus_capacity: 32,
        },
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
        .expect("target exists");

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
    let dir = temp_dir("homecmdr-automations");
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
        RuntimeConfig {
            event_bus_capacity: 32,
        },
    ));
    let state_store = Arc::new(MemoryStateStore::default());
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("sensor upsert succeeds");
    runtime
        .registry()
        .upsert(target_device("test:device"))
        .await
        .expect("target exists");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let runner = AutomationRunner::new(catalog).with_state_store(state_store.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("first trigger");
    sleep(Duration::from_millis(100)).await;
    let first_state = state_store
        .load_automation_runtime_state("cooldown_check")
        .await
        .expect("load state succeeds")
        .expect("state exists");

    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("reset trigger");
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("second trigger");
    sleep(Duration::from_millis(100)).await;

    let second_state = state_store
        .load_automation_runtime_state("cooldown_check")
        .await
        .expect("load state succeeds")
        .expect("state exists");
    assert_eq!(
        second_state.last_triggered_at,
        first_state.last_triggered_at
    );

    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automation_dedupe_window_skips_identical_trigger_payloads() {
    let dir = temp_dir("homecmdr-automations");
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
        RuntimeConfig {
            event_bus_capacity: 32,
        },
    ));
    let state_store = Arc::new(MemoryStateStore::default());
    runtime
        .registry()
        .upsert(target_device("test:device"))
        .await
        .expect("target exists");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let runner = AutomationRunner::new(catalog).with_state_store(state_store.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    runtime.bus().publish(homecmdr_core::event::Event::SystemError {
        message: "boom".to_string(),
    });
    sleep(Duration::from_millis(100)).await;
    let first_state = state_store
        .load_automation_runtime_state("dedupe_check")
        .await
        .expect("load state succeeds")
        .expect("state exists");

    runtime.bus().publish(homecmdr_core::event::Event::SystemError {
        message: "boom".to_string(),
    });
    sleep(Duration::from_millis(100)).await;
    let second_state = state_store
        .load_automation_runtime_state("dedupe_check")
        .await
        .expect("load state succeeds")
        .expect("state exists");
    assert_eq!(
        second_state.last_triggered_at,
        first_state.last_triggered_at
    );
    assert_eq!(
        second_state.last_trigger_fingerprint,
        first_state.last_trigger_fingerprint
    );

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn resumable_schedule_uses_persisted_last_scheduled_time() {
    let dir = temp_dir("homecmdr-automations");
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
                    last_scheduled_at: Some(
                        Utc.from_utc_datetime(
                            &NaiveDate::from_ymd_opt(2026, 4, 16)
                                .expect("valid date")
                                .and_hms_opt(6, 30, 0)
                                .expect("valid time"),
                        ),
                    ),
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

    let next = next_scheduled_fire_after(
        automation,
        Some(state_store),
        now,
        TriggerContext::default(),
    )
    .await
    .flatten()
    .expect("next resumed schedule exists");
    assert_eq!(next.day(), 17);
    assert_eq!(next.hour(), 6);
    assert_eq!(next.minute(), 30);
}

#[tokio::test(flavor = "multi_thread")]
async fn wall_clock_automation_executes_on_schedule() {
    let dir = temp_dir("homecmdr-automations");
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
    let runner = AutomationRunner::new(catalog).with_trigger_context(TriggerContext {
        latitude: None,
        longitude: None,
        timezone: None,
    });
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    timeout(Duration::from_secs(75), async {
        loop {
            let brightness = runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness")
                .cloned();
            if brightness == Some(AttributeValue::Integer(scheduled_for.minute() as i64)) {
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("wall clock automation fires within 75 s");

    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn condition_device_state_blocks_automation_when_not_met() {
    let dir = temp_dir("homecmdr-automations");
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
        .expect("sensor device exists");
    runtime
        .registry()
        .upsert(attribute_device(
            "test:presence",
            "occupancy",
            AttributeValue::Text("unoccupied".to_string()),
        ))
        .await
        .expect("presence device exists");
    let failed = evaluate_condition(
        &Condition::DeviceState {
            device_id: "test:presence".to_string(),
            attribute: "occupancy".to_string(),
            equals: Some(AttributeValue::Text("occupied".to_string())),
            threshold: None,
        },
        runtime.as_ref(),
        &AttributeValue::Null,
        Utc::now(),
        TriggerContext::default(),
    )
    .await;
    assert!(failed.is_err());

    runtime
        .registry()
        .upsert(attribute_device(
            "test:presence",
            "occupancy",
            AttributeValue::Text("occupied".to_string()),
        ))
        .await
        .expect("presence update succeeds");
    let passed = evaluate_condition(
        &Condition::DeviceState {
            device_id: "test:presence".to_string(),
            attribute: "occupancy".to_string(),
            equals: Some(AttributeValue::Text("occupied".to_string())),
            threshold: None,
        },
        runtime.as_ref(),
        &AttributeValue::Null,
        Utc::now(),
        TriggerContext::default(),
    )
    .await;
    assert!(passed.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn adapter_lifecycle_automation_executes_on_started_event() {
    let dir = temp_dir("homecmdr-automations");
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
    runtime.bus().publish(homecmdr_core::event::Event::AdapterStarted {
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
    let dir = temp_dir("homecmdr-automations");
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
    runtime.bus().publish(homecmdr_core::event::Event::SystemError {
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
    let dir = temp_dir("homecmdr-automations");
    let scripts_dir = temp_dir("homecmdr-scripts");
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
        Arc::new(AtomicBool::new(false)),
        DEFAULT_MAX_INSTRUCTIONS,
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
    let dir = temp_dir("homecmdr-automations");
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
                ctx:sleep(0.25)
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
    let dir = temp_dir("homecmdr-automations");
    write_automation(
        &dir,
        "slow.lua",
        r#"return {
            id = "slow_check",
            name = "Slow Check",
            mode = { type = "parallel", max = 1 },
            trigger = {
                type = "device_state_change",
                device_id = "test:rain",
                attribute = "custom.test.rain",
                equals = true,
            },
            execute = function(ctx, event)
                ctx:sleep(0.2)
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
                        concurrency: Arc::new(std::sync::Mutex::new(HashMap::new())),
                        max_instructions: DEFAULT_MAX_INSTRUCTIONS,
                        trigger_context: TriggerContext::default(),
                        backstop_timeout: DEFAULT_BACKSTOP_TIMEOUT,
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
    let dir = temp_dir("homecmdr-automations");
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

    recover_lagged_event_automations(
        runtime.clone(),
        &catalog,
        None,
        ExecutionControl {
            concurrency: Arc::new(std::sync::Mutex::new(HashMap::new())),
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
            trigger_context: TriggerContext::default(),
            backstop_timeout: DEFAULT_BACKSTOP_TIMEOUT,
        },
        2,
        None,
        None,
    );

    let recovered_value = timeout(Duration::from_secs(2), async {
        loop {
            let brightness = runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("target device exists")
                .attributes
                .get("brightness")
                .cloned();
            if let Some(AttributeValue::Integer(v)) = brightness {
                if v != 0 {
                    return v;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lag recovery should execute automation");

    assert_eq!(recovered_value, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_lua_execution_times_out_before_mutating_state() {
    let dir = temp_dir("homecmdr-automations");
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
                while true do end
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
        Arc::new(AtomicBool::new(false)),
        100_000u64,
    )
    .expect_err("infinite loop should be killed by compute limit");

    assert!(error.to_string().contains("compute limit exceeded"));
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
    let dir = temp_dir("homecmdr-automations");
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

// ── execution mode tests ──────────────────────────────────────────────────

#[test]
fn default_mode_is_parallel_max_8() {
    assert_eq!(ExecutionMode::default(), ExecutionMode::Parallel { max: 8 });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_mode_drops_concurrent_trigger() {
    let dir = temp_dir("homecmdr-automations");
    write_automation(
        &dir,
        "single.lua",
        r#"return {
            id = "single_check",
            name = "Single Check",
            mode = "single",
            trigger = {
                type = "device_state_change",
                device_id = "test:rain",
                attribute = "custom.test.rain",
                equals = true,
            },
            execute = function(ctx, event)
                ctx:sleep(0.2)
                ctx:command("test:device", {
                    capability = "brightness",
                    action = "set",
                    value = 5,
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
        .expect("sensor");
    runtime
        .registry()
        .upsert(target_device("test:device"))
        .await
        .expect("target");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let observer = Arc::new(RecordingObserver::default());
    let runner = AutomationRunner::new(catalog).with_observer(observer.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    // fire two triggers; first runs 200 ms, second arrives while first is active
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("first trigger");
    sleep(Duration::from_millis(10)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("reset");
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("second trigger");

    sleep(Duration::from_millis(400)).await;

    let entries = observer.entries.lock().expect("observer lock");
    let ok_count = entries.iter().filter(|e| e.status == "ok").count();
    let skipped_count = entries.iter().filter(|e| e.status == "skipped").count();
    assert_eq!(ok_count, 1, "first trigger should complete");
    assert_eq!(skipped_count, 1, "second trigger should be dropped");

    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_mode_allows_concurrent_up_to_max() {
    let dir = temp_dir("homecmdr-automations");
    write_automation(
        &dir,
        "parallel.lua",
        r#"return {
            id = "parallel_check",
            name = "Parallel Check",
            mode = { type = "parallel", max = 2 },
            trigger = {
                type = "device_state_change",
                device_id = "test:rain",
                attribute = "custom.test.rain",
                equals = true,
            },
            execute = function(ctx, event)
                ctx:sleep(0.15)
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
        .expect("sensor");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let observer = Arc::new(RecordingObserver::default());
    let runner = AutomationRunner::new(catalog).with_observer(observer.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    // fire two triggers in quick succession — both should run concurrently
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("first trigger");
    sleep(Duration::from_millis(5)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("reset");
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("second trigger");

    timeout(Duration::from_secs(2), async {
        loop {
            let count = observer
                .entries
                .lock()
                .expect("lock")
                .iter()
                .filter(|e| e.status == "ok")
                .count();
            if count >= 2 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both parallel executions should complete");

    let entries = observer.entries.lock().expect("observer lock");
    let skipped = entries.iter().filter(|e| e.status == "skipped").count();
    assert_eq!(skipped, 0, "no triggers should be dropped within max");

    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_mode_drops_beyond_max() {
    let dir = temp_dir("homecmdr-automations");
    write_automation(
        &dir,
        "parallel_limited.lua",
        r#"return {
            id = "parallel_limited",
            name = "Parallel Limited",
            mode = { type = "parallel", max = 1 },
            trigger = {
                type = "device_state_change",
                device_id = "test:rain",
                attribute = "custom.test.rain",
                equals = true,
            },
            execute = function(ctx, event)
                ctx:sleep(0.2)
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
        .expect("sensor");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let observer = Arc::new(RecordingObserver::default());
    let runner = AutomationRunner::new(catalog).with_observer(observer.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("first trigger");
    sleep(Duration::from_millis(10)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("reset");
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("second trigger");

    sleep(Duration::from_millis(400)).await;

    let entries = observer.entries.lock().expect("observer lock");
    let ok_count = entries.iter().filter(|e| e.status == "ok").count();
    let skipped_count = entries.iter().filter(|e| e.status == "skipped").count();
    assert_eq!(ok_count, 1, "one trigger should complete");
    assert_eq!(skipped_count, 1, "over-max trigger should be dropped");

    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_mode_runs_triggers_sequentially() {
    let dir = temp_dir("homecmdr-automations");
    write_automation(
        &dir,
        "queued.lua",
        r#"return {
            id = "queued_check",
            name = "Queued Check",
            mode = { type = "queued", max = 4 },
            trigger = {
                type = "device_state_change",
                device_id = "test:rain",
                attribute = "custom.test.rain",
                equals = true,
            },
            execute = function(ctx, event)
                ctx:sleep(0.1)
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
        .expect("sensor");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let observer = Arc::new(RecordingObserver::default());
    let runner = AutomationRunner::new(catalog).with_observer(observer.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    // fire two triggers; second queues behind first
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("first trigger");
    sleep(Duration::from_millis(5)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("reset");
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("second trigger");

    timeout(Duration::from_secs(2), async {
        loop {
            let count = observer
                .entries
                .lock()
                .expect("lock")
                .iter()
                .filter(|e| e.status == "ok")
                .count();
            if count >= 2 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both queued executions should complete");

    let entries = observer.entries.lock().expect("observer lock");
    let ok_count = entries.iter().filter(|e| e.status == "ok").count();
    let skipped_count = entries.iter().filter(|e| e.status == "skipped").count();
    assert_eq!(ok_count, 2, "both triggers should complete in queue");
    assert_eq!(skipped_count, 0);

    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_mode_drops_when_queue_full() {
    let dir = temp_dir("homecmdr-automations");
    write_automation(
        &dir,
        "queued_limited.lua",
        r#"return {
            id = "queued_limited",
            name = "Queued Limited",
            mode = { type = "queued", max = 1 },
            trigger = {
                type = "device_state_change",
                device_id = "test:rain",
                attribute = "custom.test.rain",
                equals = true,
            },
            execute = function(ctx, event)
                ctx:sleep(0.2)
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
        .expect("sensor");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let observer = Arc::new(RecordingObserver::default());
    let runner = AutomationRunner::new(catalog).with_observer(observer.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    // trigger 1: starts running (active=1)
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("first trigger");
    sleep(Duration::from_millis(10)).await;
    // trigger 2: queued (queue.len()=0 < max=1)
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("reset");
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("second trigger");
    sleep(Duration::from_millis(10)).await;
    // trigger 3: dropped (queue.len()=1 >= max=1)
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("reset 2");
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("third trigger");

    // wait for 2 completions (running + queued)
    timeout(Duration::from_secs(2), async {
        loop {
            let count = observer
                .entries
                .lock()
                .expect("lock")
                .iter()
                .filter(|e| e.status == "ok")
                .count();
            if count >= 2 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("two executions should complete");

    let entries = observer.entries.lock().expect("observer lock");
    let ok_count = entries.iter().filter(|e| e.status == "ok").count();
    let skipped_count = entries.iter().filter(|e| e.status == "skipped").count();
    assert_eq!(ok_count, 2, "running + queued trigger should complete");
    assert_eq!(
        skipped_count, 1,
        "over-queue-capacity trigger should be dropped"
    );

    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_mode_cancels_running_and_starts_new() {
    let dir = temp_dir("homecmdr-automations");
    // The 50_000-iteration loop ensures the hook fires before ctx:command when cancel is set,
    // while completing quickly when cancel remains false (trigger 2).
    write_automation(
        &dir,
        "restart.lua",
        r#"return {
            id = "restart_check",
            name = "Restart Check",
            mode = "restart",
            trigger = {
                type = "device_state_change",
                device_id = "test:rain",
                attribute = "custom.test.rain",
                equals = true,
            },
            execute = function(ctx, event)
                ctx:sleep(0.25)
                for i = 1, 50000 do end
                ctx:command("test:device", {
                    capability = "brightness",
                    action = "set",
                    value = 5,
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
        .expect("sensor");
    runtime
        .registry()
        .upsert(target_device("test:device"))
        .await
        .expect("target");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let observer = Arc::new(RecordingObserver::default());
    let runner = AutomationRunner::new(catalog).with_observer(observer.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    // fire trigger 1 — starts sleeping 250 ms
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("first trigger");
    sleep(Duration::from_millis(50)).await;
    // fire trigger 2 while trigger 1 is sleeping — restarts, cancels trigger 1
    runtime
        .registry()
        .upsert(sample_device("test:rain", false))
        .await
        .expect("reset");
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("second trigger");

    // wait until both executions record a result
    timeout(Duration::from_secs(3), async {
        loop {
            let count = observer.entries.lock().expect("lock").len();
            if count >= 2 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both restart executions should settle");

    let entries = observer.entries.lock().expect("observer lock");
    let ok_count = entries.iter().filter(|e| e.status == "ok").count();
    let error_count = entries.iter().filter(|e| e.status == "error").count();
    assert_eq!(
        ok_count, 1,
        "trigger 2 (restart) should complete successfully"
    );
    assert_eq!(error_count, 1, "trigger 1 should be cancelled");
    let cancelled = entries
        .iter()
        .find(|e| e.status == "error")
        .and_then(|e| e.error.as_deref())
        .unwrap_or("");
    assert!(
        cancelled.contains("execution cancelled"),
        "cancelled error should mention cancellation; got: {cancelled}"
    );

    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sleep_in_automation_completes_without_timeout() {
    let dir = temp_dir("homecmdr-automations");
    write_automation(
        &dir,
        "sleep.lua",
        r#"return {
            id = "sleep_check",
            name = "Sleep Check",
            trigger = {
                type = "device_state_change",
                device_id = "test:rain",
                attribute = "custom.test.rain",
                equals = true,
            },
            execute = function(ctx, event)
                ctx:sleep(0.05)
                ctx:command("test:device", {
                    capability = "brightness",
                    action = "set",
                    value = 88,
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
        .expect("sensor");
    runtime
        .registry()
        .upsert(target_device("test:device"))
        .await
        .expect("target");

    let catalog = AutomationCatalog::load_from_directory(&dir, None).expect("catalog loads");
    let observer = Arc::new(RecordingObserver::default());
    let runner = AutomationRunner::new(catalog).with_observer(observer.clone());
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runner.run(runtime).await }
    });

    sleep(Duration::from_millis(25)).await;
    runtime
        .registry()
        .upsert(sample_device("test:rain", true))
        .await
        .expect("trigger");

    timeout(Duration::from_secs(2), async {
        loop {
            if !observer.entries.lock().expect("lock").is_empty() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sleep automation should complete without timing out");

    let entries = observer.entries.lock().expect("observer lock");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, "ok");

    task.abort();
    let _ = task.await;
}
