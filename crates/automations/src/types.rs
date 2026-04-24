//! Shared types used throughout the automations crate.
//!
//! Public types (prefixed with `Automation`) are re-exported from the crate
//! root and used by the API layer.  Everything else is `pub(crate)` and
//! represents the internal representation of trigger/condition/policy config
//! parsed from automation Lua files.

use std::path::PathBuf;

use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use homecmdr_core::store::SceneStepResult;
use serde::Serialize;
use tokio::time::Duration;

pub const DEFAULT_BACKSTOP_TIMEOUT: Duration = Duration::from_secs(3600);

// ── Public types ──────────────────────────────────────────────────────────────
// These types cross the crate boundary — the API layer uses them to return
// automation summaries and execution results to callers.

/// A lightweight description of an automation returned by the list and get
/// endpoints.  Does not include the full trigger/condition config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub trigger_type: &'static str,
    pub condition_count: usize,
}

/// The full in-memory representation of a loaded automation.  Includes the
/// parsed trigger, conditions, concurrency mode, and runtime-state policy.
#[derive(Debug, Clone)]
pub struct Automation {
    pub summary: AutomationSummary,
    pub mode: homecmdr_lua_host::ExecutionMode,
    pub(crate) path: PathBuf,
    pub(crate) trigger: Trigger,
    pub(crate) conditions: Vec<Condition>,
    pub(crate) runtime_state_policy: RuntimeStatePolicy,
}

/// The result returned to the caller after manually executing an automation via
/// the API.  `status` is `"ok"`, `"skipped"`, `"error"`, or `"timeout"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationExecutionResult {
    pub status: String,
    pub error: Option<String>,
    pub results: Vec<SceneStepResult>,
    pub duration_ms: i64,
}

/// Describes a single file that failed to parse during a catalog reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReloadError {
    pub file: String,
    pub message: String,
}

/// Geographic and timezone context supplied by the server config.  Used by
/// solar triggers (sunrise/sunset) and time-window conditions.
#[derive(Debug, Clone, Copy, Default)]
pub struct TriggerContext {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub timezone: Option<Tz>,
}

// ── Crate-internal types ──────────────────────────────────────────────────────
// The types below are only used within this crate.  They represent the parsed
// trigger/condition/policy config that comes out of each automation Lua file.

/// All supported trigger kinds.  The runner spawns a different background loop
/// for each variant (event bus, interval timer, or calendar/solar scheduler).
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

/// Which lifecycle event on an adapter should fire the trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdapterLifecycleEvent {
    Started,
}

/// Optional numeric bounds for a device-state or weather-state trigger.
/// At least one of `above` or `below` must be set.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThresholdTrigger {
    pub(crate) above: Option<f64>,
    pub(crate) below: Option<f64>,
}

/// All supported condition kinds checked before an automation runs.
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

/// Optional numeric bounds for a condition check.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThresholdCondition {
    pub(crate) above: Option<f64>,
    pub(crate) below: Option<f64>,
}

/// One side of a `sun_position` condition: a solar event plus an optional
/// minute offset (e.g. "30 minutes after sunrise").
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SolarConditionPoint {
    pub(crate) event: SolarEventKind,
    pub(crate) offset_mins: i64,
}

/// Whether a solar condition point refers to sunrise or sunset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SolarEventKind {
    Sunrise,
    Sunset,
}

/// Optional runtime-state rules declared in the automation's `state` table.
/// Controls cooldown windows, deduplication, and resumable scheduling.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RuntimeStatePolicy {
    pub(crate) cooldown_secs: Option<u64>,
    pub(crate) dedupe_window_secs: Option<u64>,
    pub(crate) resumable_schedule: bool,
}

/// Runtime state loaded from the store at the start of each trigger check.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoadedAutomationRuntimeState {
    pub(crate) last_triggered_at: Option<DateTime<Utc>>,
    pub(crate) last_trigger_fingerprint: Option<String>,
    pub(crate) last_scheduled_at: Option<DateTime<Utc>>,
}

/// The outcome of a runtime-state check: either proceed with execution or
/// skip this trigger.
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
