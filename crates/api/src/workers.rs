//! Long-running background tasks that keep data in sync.
//!
//! Two workers are started at server startup:
//!
//! * `monitor_runtime_health` — listens to the event bus and updates the health
//!   dashboard when adapters start or produce errors.
//!
//! * `run_persistence_worker` — listens to the event bus and writes every
//!   device/room/group change to the database in real time.

use std::sync::Arc;

use homecmdr_core::event::Event;
use homecmdr_core::runtime::Runtime;
use homecmdr_core::store::DeviceStore;

use crate::helpers::reconcile_device_store;
use crate::state::HealthState;

/// Watch the runtime event bus and keep the health dashboard up to date.
///
/// This runs for the lifetime of the server.  It starts the adapter poll loop
/// (`runtime.run()`) and concurrently monitors the event bus for adapter
/// lifecycle events.  When `runtime.run()` returns (i.e. on shutdown), the
/// monitor task is stopped.
pub async fn monitor_runtime_health(runtime: Arc<Runtime>, health: HealthState) {
    let mut receiver = runtime.bus().subscribe();

    let monitor_task = tokio::spawn({
        let health = health.clone();
        async move {
            loop {
                match receiver.recv().await {
                    // An adapter successfully completed its first poll.
                    Ok(Event::AdapterStarted { adapter }) => health.adapter_started(&adapter),

                    // Something went wrong.  If the error message starts with
                    // an adapter name (e.g. "my_adapter failed to connect")
                    // we record it against that adapter; otherwise we mark the
                    // whole runtime as errored.
                    Ok(Event::SystemError { message }) => {
                        if let Some(adapter) = adapter_name_from_system_error(&message) {
                            health.adapter_error(adapter, message.clone());
                        } else {
                            health.runtime_error(message);
                        }
                    }
                    Ok(_) => {}

                    // The channel buffer was full and some events were dropped.
                    // Record a health warning but keep running.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        health.runtime_error(format!(
                            "health monitor lagged and dropped {skipped} runtime events"
                        ));
                    }

                    // The channel was closed — the runtime is shutting down.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    // `runtime.run()` blocks here, driving all adapter poll loops.
    runtime.run().await;

    monitor_task.abort();
    let _ = monitor_task.await;
}

/// Write device/room/group changes to the database as they happen.
///
/// Listens to the runtime event bus and saves each relevant change immediately.
/// On shutdown (signalled via `shutdown`) it does a final full reconciliation
/// to make sure nothing was missed before the process exits.
///
/// If the worker falls behind the event stream (the channel buffer overflows)
/// it falls back to a full reconciliation rather than trying to reconstruct the
/// missed individual events.
pub async fn run_persistence_worker(
    runtime: Arc<Runtime>,
    store: Arc<dyn DeviceStore>,
    health: HealthState,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let mut receiver = runtime.bus().subscribe();
    tokio::pin!(shutdown);

    loop {
        let event = tokio::select! {
            // Shutdown signal received — flush everything to the DB and exit.
            _ = &mut shutdown => {
                if let Err(error) = reconcile_device_store(
                    runtime.registry().list_rooms(),
                    runtime.registry().list_groups(),
                    runtime.registry().list(),
                    store.clone(),
                )
                .await
                {
                    health.persistence_error(format!(
                        "failed to flush persisted registry state during shutdown: {error}"
                    ));
                    tracing::error!(error = %error, "failed to flush persisted registry state during shutdown");
                } else {
                    health.persistence_ok();
                }
                break;
            }
            event = receiver.recv() => event,
        };

        match event {
            Ok(Event::DeviceAdded { device }) => {
                if let Err(error) = store.save_device(&device).await {
                    health.persistence_error(format!(
                        "failed to persist added device '{}': {error}",
                        device.id.0
                    ));
                    tracing::error!(device_id = %device.id.0, error = %error, "failed to persist added device");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::DeviceRoomChanged { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!(
                            "failed to persist room change for '{}': {error}",
                            id.0
                        ));
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device room change");
                    } else {
                        health.persistence_ok();
                    }
                }
            }
            Ok(Event::DeviceStateChanged { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!(
                            "failed to persist state change for '{}': {error}",
                            id.0
                        ));
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device state change");
                    } else {
                        health.persistence_ok();
                    }
                } else {
                    tracing::warn!(device_id = %id.0, "device state changed event received after device disappeared from registry");
                }
            }
            Ok(Event::DeviceRemoved { id }) => {
                if let Err(error) = store.delete_device(&id).await {
                    health.persistence_error(format!(
                        "failed to delete persisted device '{}': {error}",
                        id.0
                    ));
                    tracing::error!(device_id = %id.0, error = %error, "failed to delete persisted device");
                } else {
                    health.persistence_ok();
                }
            }
            // DeviceSeen fires very frequently (every poll) — we persist the
            // updated `last_seen` timestamp but keep it lightweight.
            Ok(Event::DeviceSeen { id, .. }) => {
                if let Some(device) = runtime.registry().get(&id) {
                    if let Err(error) = store.save_device(&device).await {
                        health.persistence_error(format!(
                            "failed to persist last_seen for '{}': {error}",
                            id.0
                        ));
                        tracing::error!(device_id = %id.0, error = %error, "failed to persist device last_seen update");
                    } else {
                        health.persistence_ok();
                    }
                }
            }
            Ok(Event::RoomAdded { room } | Event::RoomUpdated { room }) => {
                if let Err(error) = store.save_room(&room).await {
                    health.persistence_error(format!(
                        "failed to persist room '{}': {error}",
                        room.id.0
                    ));
                    tracing::error!(room_id = %room.id.0, error = %error, "failed to persist room");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::GroupAdded { group } | Event::GroupUpdated { group }) => {
                if let Err(error) = store.save_group(&group).await {
                    health.persistence_error(format!(
                        "failed to persist group '{}': {error}",
                        group.id.0
                    ));
                    tracing::error!(group_id = %group.id.0, error = %error, "failed to persist group");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::GroupMembersChanged { id, .. }) => {
                if let Some(group) = runtime.registry().get_group(&id) {
                    if let Err(error) = store.save_group(&group).await {
                        health.persistence_error(format!(
                            "failed to persist group membership change for '{}': {error}",
                            id.0
                        ));
                        tracing::error!(group_id = %id.0, error = %error, "failed to persist group membership change");
                    } else {
                        health.persistence_ok();
                    }
                }
            }
            Ok(Event::GroupRemoved { id }) => {
                if let Err(error) = store.delete_group(&id).await {
                    health.persistence_error(format!(
                        "failed to delete persisted group '{}': {error}",
                        id.0
                    ));
                    tracing::error!(group_id = %id.0, error = %error, "failed to delete persisted group");
                } else {
                    health.persistence_ok();
                }
            }
            Ok(Event::RoomRemoved { id }) => {
                if let Err(error) = store.delete_room(&id).await {
                    health.persistence_error(format!("failed to delete room '{}': {error}", id.0));
                    tracing::error!(room_id = %id.0, error = %error, "failed to delete persisted room");
                } else {
                    health.persistence_ok();
                }

                // When a room is deleted, devices that were in it now have no
                // room assignment.  Re-save them so the DB reflects that.
                for device in runtime.registry().list() {
                    if device.room_id.is_none() {
                        if let Err(error) = store.save_device(&device).await {
                            health.persistence_error(format!(
                                "failed to persist room removal update for '{}': {error}",
                                device.id.0
                            ));
                            tracing::error!(device_id = %device.id.0, error = %error, "failed to persist room removal device update");
                        } else {
                            health.persistence_ok();
                        }
                    }
                }
            }
            // These events don't change anything we persist — ignore them.
            Ok(
                Event::AdapterStarted { .. }
                | Event::SceneCatalogReloadStarted
                | Event::SceneCatalogReloaded { .. }
                | Event::SceneCatalogReloadFailed { .. }
                | Event::AutomationCatalogReloadStarted
                | Event::AutomationCatalogReloaded { .. }
                | Event::AutomationCatalogReloadFailed { .. }
                | Event::ScriptsReloadStarted
                | Event::ScriptsReloaded { .. }
                | Event::ScriptsReloadFailed { .. }
                | Event::PluginCatalogReloaded { .. }
                | Event::PluginCatalogReloadFailed { .. }
                | Event::SystemError { .. },
            ) => {}

            // The channel buffer overflowed and we missed some events.
            // Do a full reconciliation to bring the DB back in sync.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                health.persistence_error(format!(
                    "persistence worker lagged and dropped {skipped} runtime events"
                ));
                tracing::warn!(
                    skipped,
                    "persistence subscriber lagged; reconciling registry state"
                );

                if let Err(error) = reconcile_device_store(
                    runtime.registry().list_rooms(),
                    runtime.registry().list_groups(),
                    runtime.registry().list(),
                    store.clone(),
                )
                .await
                {
                    health.persistence_error(format!(
                        "failed to reconcile persisted registry state after lag: {error}"
                    ));
                    tracing::error!(error = %error, "failed to reconcile persisted registry state after lag");
                } else {
                    health.persistence_ok();
                }
            }

            // The channel was closed — the runtime is shutting down.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,

            // These events require no persistence action.
            Ok(Event::DeviceCommandDispatched { .. }) => {}
        }
    }
}

/// Try to extract an adapter name from a system error message.
///
/// By convention, adapter error messages start with the adapter name followed
/// by a space (e.g. `"my_lights failed to connect: …"`).  We use this to
/// attribute errors to the right adapter in the health dashboard.
/// Returns `None` if the message doesn't match that pattern.
pub fn adapter_name_from_system_error(message: &str) -> Option<&str> {
    message
        .split_once(' ')
        .map(|(prefix, _)| prefix)
        // A bare word without an underscore is unlikely to be an adapter name.
        .filter(|prefix| prefix.contains('_'))
}
