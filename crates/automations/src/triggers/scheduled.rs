//! Scheduled trigger loop for automations.
//!
//! One background task is spawned per scheduled automation (wall_clock, cron,
//! sunrise, sunset).  The task computes the next fire time, sleeps until then,
//! and spawns the automation execution.  For resumable-schedule automations it
//! consults the persisted state on startup so it can catch up on any slots
//! that were missed while the server was down.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use homecmdr_core::runtime::Runtime;
use homecmdr_core::store::DeviceStore;

use crate::catalog::AutomationCatalog;
use crate::events::scheduled_trigger_event;
use crate::runner::{spawn_automation_execution, ExecutionControl};
use crate::schedule::next_schedule_time;
use crate::state::{next_scheduled_fire_after, AutomationStateStore};
use crate::runner::AutomationExecutionObserver;
use crate::types::{Automation, TriggerContext};

// ── Scheduled trigger loop ────────────────────────────────────────────────────
// Computes the next fire time on startup (consulting saved state for resumable
// schedules), sleeps until then, and then advances to the following slot.
// Exits cleanly if the trigger produces no future occurrences.

/// Run a scheduled trigger loop for `automation`.  Sleeps until the next
/// scheduled fire time, then spawns the automation and advances to the
/// following slot.
pub(crate) async fn run_scheduled_trigger_loop(
    runtime: Arc<Runtime>,
    catalog: AutomationCatalog,
    automation: Automation,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
    trigger_context: TriggerContext,
) {
    let next_fire_at = next_scheduled_fire_after(
        &automation,
        state_store.as_ref().map(|store| AutomationStateStore {
            store: store.clone(),
        }),
        Utc::now(),
        trigger_context,
    )
    .await
    .unwrap_or_else(|| next_schedule_time(&automation.trigger, Utc::now(), trigger_context));
    let mut next_fire_at = match next_fire_at {
        Some(next_fire_at) => next_fire_at,
        None => {
            tracing::error!(automation = %automation.summary.id, "scheduled automation has no next fire time");
            return;
        }
    };

    loop {
        let now = Utc::now();
        let sleep_duration = next_fire_at
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default();
        tokio::time::sleep(sleep_duration).await;

        if !catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
            next_fire_at =
                match next_schedule_time(&automation.trigger, Utc::now(), trigger_context) {
                    Some(next_fire_at) => next_fire_at,
                    None => break,
                };
            continue;
        }

        let scheduled_for = next_fire_at;
        next_fire_at = match next_schedule_time(&automation.trigger, scheduled_for, trigger_context)
        {
            Some(next_fire_at) => next_fire_at,
            None => break,
        };

        let event = scheduled_trigger_event(&automation.trigger, scheduled_for, trigger_context);
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
