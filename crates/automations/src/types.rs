use std::path::PathBuf;

use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use homecmdr_core::store::SceneStepResult;
use serde::Serialize;
use tokio::time::Duration;

pub const DEFAULT_BACKSTOP_TIMEOUT: Duration = Duration::from_secs(3600);

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub trigger_type: &'static str,
    pub condition_count: usize,
}

#[derive(Debug, Clone)]
pub struct Automation {
    pub summary: AutomationSummary,
    pub mode: homecmdr_lua_host::ExecutionMode,
    pub(crate) path: PathBuf,
    pub(crate) trigger: Trigger,
    pub(crate) conditions: Vec<Condition>,
    pub(crate) runtime_state_policy: RuntimeStatePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationExecutionResult {
    pub status: String,
    pub error: Option<String>,
    pub results: Vec<SceneStepResult>,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReloadError {
    pub file: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TriggerContext {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub timezone: Option<Tz>,
}

// ── Crate-internal types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Trigger {
    DeviceStateChange {
        device_id: String,
        attribute: Option<String>,
        equals: Option<homecmdr_core::model::AttributeValue>,
        threshold: Option<ThresholdTrigger>,
        debounce_secs: Option<u64>,
        duration_secs: Option<u64>,
    },
    WeatherState {
        device_id: String,
        attribute: String,
        equals: Option<homecmdr_core::model::AttributeValue>,
        threshold: Option<ThresholdTrigger>,
        debounce_secs: Option<u64>,
        duration_secs: Option<u64>,
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
pub(crate) enum AdapterLifecycleEvent {
    Started,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThresholdTrigger {
    pub(crate) above: Option<f64>,
    pub(crate) below: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Condition {
    DeviceState {
        device_id: String,
        attribute: String,
        equals: Option<homecmdr_core::model::AttributeValue>,
        threshold: Option<ThresholdCondition>,
    },
    TimeWindow {
        start: NaiveTime,
        end: NaiveTime,
    },
    Presence {
        device_id: String,
        attribute: String,
        equals: homecmdr_core::model::AttributeValue,
    },
    RoomState {
        room_id: String,
        min_devices: Option<usize>,
        max_devices: Option<usize>,
    },
    SunPosition {
        after: Option<SolarConditionPoint>,
        before: Option<SolarConditionPoint>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThresholdCondition {
    pub(crate) above: Option<f64>,
    pub(crate) below: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SolarConditionPoint {
    pub(crate) event: SolarEventKind,
    pub(crate) offset_mins: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SolarEventKind {
    Sunrise,
    Sunset,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RuntimeStatePolicy {
    pub(crate) cooldown_secs: Option<u64>,
    pub(crate) dedupe_window_secs: Option<u64>,
    pub(crate) resumable_schedule: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LoadedAutomationRuntimeState {
    pub(crate) last_triggered_at: Option<DateTime<Utc>>,
    pub(crate) last_trigger_fingerprint: Option<String>,
    pub(crate) last_scheduled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub(crate) enum TriggerDecision {
    Execute,
    Skip,
}

impl TriggerDecision {
    pub(crate) fn is_skip(&self) -> bool {
        matches!(self, Self::Skip)
    }
}
