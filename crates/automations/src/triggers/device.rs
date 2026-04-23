use std::path::PathBuf;
use std::sync::Arc;

use homecmdr_core::runtime::Runtime;
use homecmdr_core::store::DeviceStore;

use crate::catalog::AutomationCatalog;
use crate::events::{automation_event_from_registry_snapshot, automation_event_from_runtime_event};
use crate::runner::{spawn_automation_execution, ExecutionControl};
use crate::state::AutomationStateStore;
use crate::runner::AutomationExecutionObserver;

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
