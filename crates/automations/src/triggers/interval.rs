//! Interval trigger loop for automations.
//!
//! One background task is spawned per `interval` automation.  The task fires
//! the automation every `every_secs` seconds using a Tokio interval timer with
//! `MissedTickBehavior::Skip` — if a tick is late, the missed ticks are dropped
//! rather than delivered all at once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use homecmdr_core::model::AttributeValue;
use homecmdr_core::runtime::Runtime;
use homecmdr_core::store::DeviceStore;

use crate::catalog::AutomationCatalog;
use crate::runner::{spawn_automation_execution, ExecutionControl};
use crate::state::AutomationStateStore;
use crate::runner::AutomationExecutionObserver;
use crate::types::Automation;

// ── Interval trigger loop ─────────────────────────────────────────────────────
// Ticks every `every_secs` seconds and hands the automation off to the runner.
// Skips the tick if the automation is currently disabled.

/// Run an interval trigger loop for `automation`.  Ticks every `every_secs`
/// seconds and hands off to [`spawn_automation_execution`] if the automation
/// is enabled.
pub(crate) async fn run_interval_trigger_loop(
    runtime: Arc<Runtime>,
    catalog: AutomationCatalog,
    automation: Automation,
    every_secs: u64,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(every_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        if !catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
            continue;
        }

        let event = AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("interval".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(Utc::now().to_rfc3339()),
            ),
            (
                "every_secs".to_string(),
                AttributeValue::Integer(every_secs as i64),
            ),
        ]));

        spawn_automation_execution(
            automation.clone(),
            runtime.clone(),
            event,
            scripts_root.clone(),
            execution.clone(),
            observer.clone(),
            state_store.as_ref().map(|store| AutomationStateStore {
                store: store.clone(),
            }),
        );
    }
}
