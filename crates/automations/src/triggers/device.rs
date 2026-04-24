//! Event-bus trigger loop for automations.
//!
//! A single background task subscribes to the runtime event bus and, for each
//! event, checks every event-bus automation to see whether it matches.  If it
//! does, the automation is handed to [`spawn_automation_execution`].
//!
//! When the event bus falls behind (the channel buffer fills up and events are
//! dropped), the loop detects the lag and recovers by reading the current
//! device state from the registry so no trigger is silently missed.

use std::path::PathBuf;
use std::sync::Arc;

use homecmdr_core::runtime::Runtime;
use homecmdr_core::store::DeviceStore;

use crate::catalog::AutomationCatalog;
use crate::events::{automation_event_from_registry_snapshot, automation_event_from_runtime_event};
use crate::runner::{spawn_automation_execution, ExecutionControl};
use crate::state::AutomationStateStore;
use crate::runner::AutomationExecutionObserver;

// ── Event trigger loop ────────────────────────────────────────────────────────
// Subscribes to the event bus and dispatches matching events to the runner.
// Runs for the lifetime of the server.

/// Subscribe to the runtime event bus and dispatch matching events to the
/// automation runner.  Handles lag recovery by falling back to a registry
/// snapshot when events are dropped.
pub(crate) async fn run_event_trigger_loop(
    runtime: Arc<Runtime>,
    catalog: AutomationCatalog,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
) {
    let mut receiver = runtime.bus().subscribe();

    loop {
        match receiver.recv().await {
            Ok(event) => {
                for automation in &catalog.automations {
                    if !catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
                        continue;
                    }
                    if let Some(event_value) =
                        automation_event_from_runtime_event(automation, &event)
                    {
                        let state_store = state_store.as_ref().map(|store| AutomationStateStore {
                            store: store.clone(),
                        });
                        spawn_automation_execution(
                            automation.clone(),
                            runtime.clone(),
                            event_value,
                            scripts_root.clone(),
                            execution.clone(),
                            observer.clone(),
                            state_store,
                        );
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "automation event trigger loop lagged behind");
                recover_lagged_event_automations(
                    runtime.clone(),
                    &catalog,
                    scripts_root.clone(),
                    execution.clone(),
                    skipped,
                    observer.clone(),
                    state_store.clone(),
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

// ── Lag recovery ─────────────────────────────────────────────────────────────
// When the event bus drops messages because the consumer is too slow, we don't
// know exactly which events were missed.  Instead, we read the current device
// state from the registry and synthesize events as if each device had just
// changed, so no automation fires are permanently lost.

/// Called when the event channel reports it has lagged and dropped messages.
/// Reads each device's current state from the registry and spawns any automations
/// whose trigger condition is currently satisfied.
pub(crate) fn recover_lagged_event_automations(
    runtime: Arc<Runtime>,
    catalog: &AutomationCatalog,
    scripts_root: Option<PathBuf>,
    execution: ExecutionControl,
    skipped: u64,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    state_store: Option<Arc<dyn DeviceStore>>,
) {
    for automation in &catalog.automations {
        if !catalog.is_enabled(&automation.summary.id).unwrap_or(true) {
            continue;
        }
        if let Some(event_value) =
            automation_event_from_registry_snapshot(automation, runtime.registry(), skipped)
        {
            spawn_automation_execution(
                automation.clone(),
                runtime.clone(),
                event_value,
                scripts_root.clone(),
                execution.clone(),
                observer.clone(),
                state_store.as_ref().map(|store| AutomationStateStore {
                    store: store.clone(),
                }),
            );
        }
    }
}
