//! Runtime-state persistence for automations.
//!
//! Some automations declare a `state` table in their Lua file that enables
//! cooldown windows (don't fire more than once per N seconds), deduplication
//! (don't fire twice with the same payload within a window), or resumable
//! scheduling (pick up where the schedule left off after a restart).
//!
//! This module reads and writes those state records through the `DeviceStore`
//! and provides the [`should_skip_trigger`] decision helper used by the
//! execution path.

use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use homecmdr_core::model::AttributeValue;
use homecmdr_core::store::{AutomationRuntimeState, DeviceStore};

use crate::schedule::next_schedule_time;
use crate::types::{Automation, LoadedAutomationRuntimeState, TriggerContext, TriggerDecision};

// ── AutomationStateStore ──────────────────────────────────────────────────────
// Thin wrapper around `DeviceStore` that handles I/O errors gracefully.
// A failed state load is logged and treated as "no previous state" so an
// automation is never silently skipped because the database was temporarily
// unavailable.

/// Wraps the persistent device store to provide typed load/save helpers for
/// automation runtime state.
#[derive(Clone)]
pub(crate) struct AutomationStateStore {
    pub(crate) store: Arc<dyn DeviceStore>,
}

impl AutomationStateStore {
    pub(crate) async fn load(&self, automation_id: &str) -> Option<LoadedAutomationRuntimeState> {
        match self
            .store
            .load_automation_runtime_state(automation_id)
            .await
        {
            Ok(state) => state.map(|state| LoadedAutomationRuntimeState {
                last_triggered_at: state.last_triggered_at,
                last_trigger_fingerprint: state.last_trigger_fingerprint,
                last_scheduled_at: state.last_scheduled_at,
            }),
            Err(error) => {
                tracing::error!(automation = %automation_id, error = %error, "failed to load automation runtime state");
                None
            }
        }
    }

    pub(crate) async fn save(&self, state: &AutomationRuntimeState) {
        if let Err(error) = self.store.save_automation_runtime_state(state).await {
            tracing::error!(automation = %state.automation_id, error = %error, "failed to save automation runtime state");
        }
    }
}

// ── should_skip_trigger ───────────────────────────────────────────────────────
// Checks cooldown and deduplication windows before an execution starts.
// Returns `Skip` if the automation fired too recently or if the same payload
// was seen within the deduplication window.

/// Decide whether this trigger should be skipped based on the automation's
/// runtime-state policy.  Returns [`TriggerDecision::Execute`] if the policy
/// allows execution, or [`TriggerDecision::Skip`] if a cooldown or dedup window
/// is still active.
pub(crate) async fn should_skip_trigger(
    automation: &Automation,
    event: &AttributeValue,
    state_store: Option<&Arc<dyn DeviceStore>>,
    scheduled_for: Option<DateTime<Utc>>,
) -> TriggerDecision {
    let policy = &automation.runtime_state_policy;
    if policy.cooldown_secs.is_none()
        && policy.dedupe_window_secs.is_none()
        && !policy.resumable_schedule
    {
        return TriggerDecision::Execute;
    }

    let Some(store) = state_store else {
        return TriggerDecision::Execute;
    };
    let state_store = AutomationStateStore {
        store: store.clone(),
    };
    let state = state_store
        .load(&automation.summary.id)
        .await
        .unwrap_or_default();
    let now = Utc::now();

    if let Some(cooldown_secs) = policy.cooldown_secs {
        if let Some(last_triggered_at) = state.last_triggered_at {
            if now < last_triggered_at + ChronoDuration::seconds(cooldown_secs as i64) {
                return TriggerDecision::Skip;
            }
        }
    }

    if let Some(dedupe_window_secs) = policy.dedupe_window_secs {
        let fingerprint = trigger_fingerprint(event);
        if let (Some(last_triggered_at), Some(last_fingerprint)) = (
            state.last_triggered_at,
            state.last_trigger_fingerprint.as_deref(),
        ) {
            if last_fingerprint == fingerprint
                && now < last_triggered_at + ChronoDuration::seconds(dedupe_window_secs as i64)
            {
                return TriggerDecision::Skip;
            }
        }
    }

    if policy.resumable_schedule {
        if let (Some(last_scheduled_at), Some(scheduled_for)) =
            (state.last_scheduled_at, scheduled_for)
        {
            if scheduled_for <= last_scheduled_at {
                return TriggerDecision::Skip;
            }
        }
    }

    TriggerDecision::Execute
}

// ── persist_runtime_state ─────────────────────────────────────────────────────
// Writes the current timestamp and a fingerprint of the trigger payload to the
// store after a successful execution, so future cooldown/dedup checks can see
// when and what last ran.

/// Save the execution timestamp and trigger fingerprint for `automation` so
/// future calls to [`should_skip_trigger`] can enforce cooldown and dedup rules.
pub(crate) async fn persist_runtime_state(
    automation: &Automation,
    event: &AttributeValue,
    state_store: Option<&AutomationStateStore>,
    scheduled_for: Option<DateTime<Utc>>,
) {
    let Some(state_store) = state_store else {
        return;
    };

    state_store
        .save(&AutomationRuntimeState {
            updated_at: Utc::now(),
            automation_id: automation.summary.id.clone(),
            last_triggered_at: Some(Utc::now()),
            last_trigger_fingerprint: Some(trigger_fingerprint(event).to_string()),
            last_scheduled_at: scheduled_for,
        })
        .await;
}

// ── next_scheduled_fire_after ─────────────────────────────────────────────────
// For resumable-schedule automations, looks up the last scheduled fire time
// from the store and returns the *next* occurrence after that.  This means
// a server restart won't repeat a slot that already fired.

/// For a resumable-schedule automation, return the next fire time based on the
/// last recorded scheduled-at timestamp.  Returns `None` if the automation does
/// not use resumable scheduling or if no state is available.
pub(crate) async fn next_scheduled_fire_after(
    automation: &Automation,
    state_store: Option<AutomationStateStore>,
    now: DateTime<Utc>,
    trigger_context: TriggerContext,
) -> Option<Option<DateTime<Utc>>> {
    if !automation.runtime_state_policy.resumable_schedule {
        return None;
    }
    let Some(state_store) = state_store else {
        return None;
    };

    let state = state_store
        .load(&automation.summary.id)
        .await
        .unwrap_or_default();
    let anchor = state.last_scheduled_at.unwrap_or(now);
    Some(next_schedule_time(&automation.trigger, anchor, trigger_context))
}

// ── trigger_fingerprint / canonicalize_attribute_value ────────────────────────
// Produces a stable, order-independent string key for any `AttributeValue`.
// Object fields are sorted by key before serialization so two identical objects
// with different field ordering produce the same fingerprint.

fn trigger_fingerprint(event: &AttributeValue) -> String {
    canonicalize_attribute_value(event)
}

fn canonicalize_attribute_value(value: &AttributeValue) -> String {
    match value {
        AttributeValue::Integer(value) => value.to_string(),
        AttributeValue::Float(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
        }
        AttributeValue::Bool(value) => value.to_string(),
        AttributeValue::Text(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
        }
        AttributeValue::Array(values) => {
            let values = values
                .iter()
                .map(canonicalize_attribute_value)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{values}]")
        }
        AttributeValue::Object(fields) => {
            let mut entries = fields.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let entries = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_else(|_| "null".to_string()),
                        canonicalize_attribute_value(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{entries}}}")
        }
        AttributeValue::Null => "null".to_string(),
    }
}
