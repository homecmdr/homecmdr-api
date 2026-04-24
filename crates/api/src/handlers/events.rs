//! WebSocket event stream and IPC device ingest handlers.
//!
//! ## Event stream (`GET /events`)
//!
//! Upgrades the connection to a WebSocket.  Every event published on the
//! internal runtime bus is serialised to JSON and forwarded to the client.
//! The `DeviceSeen` event is deliberately dropped here — it fires on every
//! adapter poll cycle and would flood the connection with no useful signal.
//!
//! Client → server frames are consumed to keep the connection alive (ping
//! frames are answered automatically by tungstenite when the stream is
//! driven), but their content is ignored.
//!
//! If the subscriber falls behind the broadcast channel the client receives
//! a `{"type":"system.error","message":"subscriber lagged, events dropped"}`
//! frame notifying it that some events were missed.
//!
//! ## Device ingest (`POST /ingest/devices`)
//!
//! Accepts a batch of device state updates from an IPC adapter process.
//! Only adapters declared as IPC in their `.plugin.toml` are allowed to call
//! this endpoint — other adapters get a 403 Forbidden response.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use homecmdr_core::event::Event;
use homecmdr_core::model::{DeviceId, DeviceKind, Metadata};
use homecmdr_core::runtime::Runtime;
use serde_json::json;

use crate::dto::{ApiError, IngestDevicesRequest};
use crate::state::AppState;

/// Upgrades an HTTP connection to a WebSocket and starts forwarding runtime
/// events to the client.
pub async fn events(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_events_socket(socket, state.runtime))
}

/// Drives the WebSocket connection, relaying bus events to the client and
/// detecting disconnection.
pub async fn handle_events_socket(mut socket: WebSocket, runtime: Arc<Runtime>) {
    let mut receiver = runtime.bus().subscribe();

    loop {
        tokio::select! {
            // Drive the socket reader so that ping frames get answered and
            // close frames are detected promptly.
            msg = socket.recv() => {
                match msg {
                    // Client closed the connection or stream ended.
                    None => break,
                    // Received a close frame — acknowledge and stop.
                    Some(Ok(Message::Close(_))) => break,
                    // Any error on the receive side means the connection is gone.
                    Some(Err(_)) => break,
                    // Ping/pong/text/binary frames from the client are ignored;
                    // tungstenite automatically replies to pings when the stream
                    // is driven via recv().
                    Some(Ok(_)) => continue,
                }
            }

            // Forward events from the internal bus to the client.
            bus_result = receiver.recv() => {
                let frame = match bus_result {
                    Ok(Event::DeviceSeen { .. }) => continue,
                    Ok(event) => event_to_frame(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        json!({
                            "type": "system.error",
                            "message": "subscriber lagged, events dropped"
                        })
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };

                if socket
                    .send(Message::Text(frame.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Converts an internal `Event` into a JSON frame to send over the WebSocket.
///
/// The resulting object always has a `"type"` field using dot-namespaced
/// identifiers (e.g. `"device.state_changed"`, `"automation.catalog_reloaded"`).
/// Additional fields depend on the event kind.
///
/// Note: `DeviceSeen` is listed here for completeness but is filtered out
/// *before* `event_to_frame` is called in `handle_events_socket`.
pub fn event_to_frame(event: Event) -> serde_json::Value {
    match event {
        Event::DeviceStateChanged { id, attributes, .. } => {
            json!({ "type": "device.state_changed", "id": id.0, "state": attributes })
        }
        Event::DeviceAdded { device } => {
            json!({ "type": "device.state_changed", "id": device.id.0, "state": device.attributes })
        }
        Event::DeviceRemoved { id } => json!({ "type": "device.removed", "id": id.0 }),
        Event::DeviceSeen { .. } => json!({ "type": "internal.device_seen" }),
        Event::RoomAdded { room } => {
            json!({ "type": "room.added", "room": room })
        }
        Event::RoomUpdated { room } => {
            json!({ "type": "room.updated", "room": room })
        }
        Event::RoomRemoved { id } => json!({ "type": "room.removed", "id": id.0 }),
        Event::GroupAdded { group } => {
            json!({ "type": "group.added", "group": group })
        }
        Event::GroupUpdated { group } => {
            json!({ "type": "group.updated", "group": group })
        }
        Event::GroupRemoved { id } => json!({ "type": "group.removed", "id": id.0 }),
        Event::GroupMembersChanged { id, members } => {
            json!({
                "type": "group.members_changed",
                "id": id.0,
                "members": members.into_iter().map(|member| member.0).collect::<Vec<_>>()
            })
        }
        Event::DeviceRoomChanged { id, room_id } => {
            json!({ "type": "device.room_changed", "id": id.0, "room_id": room_id.map(|room| room.0) })
        }
        Event::AdapterStarted { adapter } => {
            json!({ "type": "adapter.started", "adapter": adapter })
        }
        Event::SceneCatalogReloadStarted => {
            json!({ "type": "scene.catalog_reload_started", "target": "scenes" })
        }
        Event::SceneCatalogReloaded {
            loaded_count,
            duration_ms,
        } => {
            json!({
                "type": "scene.catalog_reloaded",
                "target": "scenes",
                "loaded_count": loaded_count,
                "duration_ms": duration_ms,
                "errors": []
            })
        }
        Event::SceneCatalogReloadFailed {
            duration_ms,
            errors,
        } => {
            json!({
                "type": "scene.catalog_reload_failed",
                "target": "scenes",
                "loaded_count": 0,
                "duration_ms": duration_ms,
                "errors": errors
            })
        }
        Event::AutomationCatalogReloadStarted => {
            json!({ "type": "automation.catalog_reload_started", "target": "automations" })
        }
        Event::AutomationCatalogReloaded {
            loaded_count,
            duration_ms,
        } => {
            json!({
                "type": "automation.catalog_reloaded",
                "target": "automations",
                "loaded_count": loaded_count,
                "duration_ms": duration_ms,
                "errors": []
            })
        }
        Event::AutomationCatalogReloadFailed {
            duration_ms,
            errors,
        } => {
            json!({
                "type": "automation.catalog_reload_failed",
                "target": "automations",
                "loaded_count": 0,
                "duration_ms": duration_ms,
                "errors": errors
            })
        }
        Event::ScriptsReloadStarted => {
            json!({ "type": "scripts.reload_started", "target": "scripts" })
        }
        Event::ScriptsReloaded {
            loaded_count,
            duration_ms,
        } => {
            json!({
                "type": "scripts.reloaded",
                "target": "scripts",
                "loaded_count": loaded_count,
                "duration_ms": duration_ms,
                "errors": []
            })
        }
        Event::ScriptsReloadFailed {
            duration_ms,
            errors,
        } => {
            json!({
                "type": "scripts.reload_failed",
                "target": "scripts",
                "loaded_count": 0,
                "duration_ms": duration_ms,
                "errors": errors
            })
        }
        Event::PluginCatalogReloaded {
            loaded_count,
            duration_ms,
        } => {
            json!({
                "type": "plugin.catalog_reloaded",
                "target": "plugins",
                "loaded_count": loaded_count,
                "duration_ms": duration_ms,
                "errors": []
            })
        }
        Event::PluginCatalogReloadFailed {
            duration_ms,
            errors,
        } => {
            json!({
                "type": "plugin.catalog_reload_failed",
                "target": "plugins",
                "loaded_count": 0,
                "duration_ms": duration_ms,
                "errors": errors
            })
        }
        Event::SystemError { message } => {
            json!({ "type": "system.error", "message": message })
        }
        Event::DeviceCommandDispatched { id, command } => {
            json!({ "type": "device.command_dispatched", "id": id.0, "command": command })
        }
        // Person/zone events are streamed as typed frames.
        Event::PersonStateChanged { person_id, person_name, to, .. } => {
            json!({ "type": "person.state_changed", "id": person_id.0, "name": person_name, "state": to })
        }
        Event::PersonAdded { person } => {
            json!({ "type": "person.added", "person": person })
        }
        Event::PersonUpdated { person } => {
            json!({ "type": "person.updated", "person": person })
        }
        Event::PersonRemoved { person_id } => {
            json!({ "type": "person.removed", "id": person_id.0 })
        }
        Event::AllPersonsAway => {
            json!({ "type": "all_persons_away" })
        }
        Event::AnyPersonHome { person_id, person_name } => {
            json!({ "type": "any_person_home", "id": person_id.0, "name": person_name })
        }
        Event::ZoneAdded { zone } => {
            json!({ "type": "zone.added", "zone": zone })
        }
        Event::ZoneUpdated { zone } => {
            json!({ "type": "zone.updated", "zone": zone })
        }
        Event::ZoneRemoved { zone_id } => {
            json!({ "type": "zone.removed", "id": zone_id.0 })
        }
    }
}

/// Accepts a batch of device state updates from an IPC adapter process and
/// upserts them into the in-memory registry.
///
/// The adapter name is prepended to each `vendor_id` to form a fully-qualified
/// device ID (e.g. `"my-adapter:device-42"`).  Existing `room_id` assignments
/// are preserved — IPC adapters must not overwrite room data they did not set.
///
/// Only adapters registered as IPC (identified by their `.plugin.toml`
/// declaring `transport = "ipc"`) are permitted to call this endpoint.
/// Returns 403 for any other adapter name.
pub async fn ingest_devices(
    State(state): State<AppState>,
    Json(req): Json<IngestDevicesRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use homecmdr_core::model::AttributeValue;

    let adapter = req.adapter.trim().to_string();
    if adapter.is_empty() {
        return Err(ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "adapter name must not be empty".to_string(),
        ));
    }

    // Only IPC adapters may push state via this endpoint.
    if !state.ipc_adapter_names.contains(&adapter) {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            format!("adapter '{adapter}' is not a registered IPC adapter"),
        ));
    }

    fn json_to_attr(v: serde_json::Value) -> AttributeValue {
        match v {
            serde_json::Value::Bool(b) => AttributeValue::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    AttributeValue::Integer(i)
                } else {
                    AttributeValue::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => AttributeValue::Text(s),
            serde_json::Value::Array(arr) => {
                AttributeValue::Array(arr.into_iter().map(json_to_attr).collect())
            }
            serde_json::Value::Object(obj) => {
                AttributeValue::Object(obj.into_iter().map(|(k, v)| (k, json_to_attr(v))).collect())
            }
            serde_json::Value::Null => AttributeValue::Null,
        }
    }

    let mut upserted = 0usize;
    let now = Utc::now();

    for entry in req.devices {
        let device_id = DeviceId(format!("{}:{}", adapter, entry.vendor_id));

        let kind = match entry.kind.as_str() {
            "light" => DeviceKind::Light,
            "switch" => DeviceKind::Switch,
            "sensor" => DeviceKind::Sensor,
            _ => DeviceKind::Sensor,
        };

        let attributes: homecmdr_core::model::Attributes = match entry.attributes {
            serde_json::Value::Object(map) => {
                map.into_iter().map(|(k, v)| (k, json_to_attr(v))).collect()
            }
            _ => Default::default(),
        };

        let vendor_specific: std::collections::HashMap<String, serde_json::Value> =
            match entry.metadata {
                serde_json::Value::Object(map) => map.into_iter().collect(),
                _ => Default::default(),
            };

        // Preserve existing room_id if the device is already registered.
        let room_id = state
            .runtime
            .registry()
            .get(&device_id)
            .and_then(|d| d.room_id);

        let device = homecmdr_core::model::Device {
            id: device_id,
            room_id,
            kind,
            attributes,
            metadata: Metadata {
                source: adapter.clone(),
                accuracy: None,
                vendor_specific,
            },
            updated_at: now,
            last_seen: now,
        };

        match state.runtime.registry().upsert(device).await {
            Ok(()) => upserted += 1,
            Err(e) => {
                tracing::warn!(adapter = %adapter, "ingest_devices: upsert failed: {e}");
            }
        }
    }

    Ok(Json(json!({ "upserted": upserted })))
}
