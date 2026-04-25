//! Device, room, and group management handlers.
//!
//! This module covers the majority of the write surface of the API:
//! creating/deleting rooms and groups, assigning devices to rooms, and
//! sending commands to individual devices, whole rooms, or groups.
//!
//! ## Command dispatch
//!
//! For WASM adapters a command is sent directly through the runtime's
//! `command_device` method, which calls the adapter synchronously.
//!
//! For **IPC adapters** (adapters running as external processes), the runtime
//! returns `Ok(false)` because it has no direct handle to the child process.
//! In that case the handler publishes a `DeviceCommandDispatched` event onto
//! the bus.  The IPC adapter process is subscribed to the `/events` WebSocket
//! and handles the command asynchronously.
//!
//! Every command attempt — success, failure, or dispatch — is recorded in the
//! command audit log (when persistence is enabled).

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use homecmdr_core::command::DeviceCommand;
use homecmdr_core::event::Event;
use homecmdr_core::model::{DeviceGroup, DeviceId, GroupId, Room, RoomId};
use homecmdr_core::store::CommandAuditEntry;
use serde_json::json;

use crate::dto::{
    ApiError, AssignRoomRequest, CreateGroupRequest, CreateRoomRequest, GroupCommandResult,
    RoomCommandResult, SetGroupMembersRequest,
};
use crate::helpers::persist_command_audit;
use crate::state::AppState;

/// Returns all rooms in the registry.
pub async fn list_rooms(State(state): State<AppState>) -> Json<Vec<Room>> {
    Json(state.runtime.registry().list_rooms())
}

/// Creates a new room (or updates an existing one with the same ID).
///
/// Both `id` and `name` are required and must not be empty strings.
pub async fn create_room(
    State(state): State<AppState>,
    Json(request): Json<CreateRoomRequest>,
) -> Result<Json<Room>, ApiError> {
    let id = request.id.trim();
    let name = request.name.trim();

    if id.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "room id must not be empty",
        ));
    }
    if name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "room name must not be empty",
        ));
    }

    let room = Room {
        id: RoomId(id.to_string()),
        name: name.to_string(),
    };
    state.runtime.registry().upsert_room(room.clone()).await;
    Ok(Json(room))
}

/// Returns a single room by ID, or 404 if it does not exist.
pub async fn get_room(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Room>, ApiError> {
    state
        .runtime
        .registry()
        .get_room(&RoomId(id.clone()))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("room '{id}' not found")))
}

/// Deletes a room.  Returns 404 if the room does not exist.
/// Devices that were in the room are **not** deleted — their `room_id` is
/// cleared by the registry when the room is removed.
pub async fn delete_room(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let room_id = RoomId(id.clone());
    if !state.runtime.registry().remove_room(&room_id).await {
        return Err(ApiError::not_found(format!("room '{id}' not found")));
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Returns all devices whose `room_id` matches the given room.
/// Returns 404 if the room itself does not exist (even if it has no devices).
pub async fn list_room_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<homecmdr_core::model::Device>>, ApiError> {
    let room_id = RoomId(id.clone());
    if state.runtime.registry().get_room(&room_id).is_none() {
        return Err(ApiError::not_found(format!("room '{id}' not found")));
    }

    Ok(Json(
        state.runtime.registry().list_devices_in_room(&room_id),
    ))
}

/// Validates the common fields required when creating or updating a group.
fn validate_group_payload(id: &str, name: &str, members: &[String]) -> Result<(), ApiError> {
    if id.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "group id must not be empty",
        ));
    }
    if name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "group name must not be empty",
        ));
    }
    for member in members {
        if member.trim().is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "group members must not contain empty device IDs",
            ));
        }
    }

    Ok(())
}

/// Returns all device groups.
pub async fn list_groups(State(state): State<AppState>) -> Json<Vec<DeviceGroup>> {
    Json(state.runtime.registry().list_groups())
}

/// Creates a new group (or replaces an existing one with the same ID).
pub async fn create_group(
    State(state): State<AppState>,
    Json(request): Json<CreateGroupRequest>,
) -> Result<Json<DeviceGroup>, ApiError> {
    let id = request.id.trim();
    let name = request.name.trim();
    validate_group_payload(id, name, &request.members)?;

    let group = DeviceGroup {
        id: GroupId(id.to_string()),
        name: name.to_string(),
        members: request.members.into_iter().map(DeviceId).collect(),
    };

    state
        .runtime
        .registry()
        .upsert_group(group.clone())
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    Ok(Json(group))
}

/// Returns a single group by ID, or 404 if it does not exist.
pub async fn get_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DeviceGroup>, ApiError> {
    state
        .runtime
        .registry()
        .get_group(&GroupId(id.clone()))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("group '{id}' not found")))
}

/// Deletes a group.  Returns 404 if it does not exist.
pub async fn delete_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let group_id = GroupId(id.clone());
    if !state.runtime.registry().remove_group(&group_id).await {
        return Err(ApiError::not_found(format!("group '{id}' not found")));
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Replaces the full member list of an existing group.
/// Returns the updated group, or 404 if the group does not exist.
pub async fn set_group_members(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<SetGroupMembersRequest>,
) -> Result<Json<DeviceGroup>, ApiError> {
    let group_id = GroupId(id.clone());
    if state.runtime.registry().get_group(&group_id).is_none() {
        return Err(ApiError::not_found(format!("group '{id}' not found")));
    }

    state
        .runtime
        .registry()
        .set_group_members(
            &group_id,
            request.members.into_iter().map(DeviceId).collect(),
        )
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    state
        .runtime
        .registry()
        .get_group(&group_id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("group '{id}' not found")))
}

/// Returns all devices that are members of the given group.
/// Returns 404 if the group does not exist.
pub async fn list_group_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<homecmdr_core::model::Device>>, ApiError> {
    let group_id = GroupId(id.clone());
    if state.runtime.registry().get_group(&group_id).is_none() {
        return Err(ApiError::not_found(format!("group '{id}' not found")));
    }

    Ok(Json(
        state.runtime.registry().list_devices_in_group(&group_id),
    ))
}

/// Returns all devices, or a filtered subset when query parameters are provided.
///
/// Supported filters (mutually exclusive; `ids` takes precedence if both are given):
///
/// - `?ids=open_meteo:humidity&ids=open_meteo:temperature_outdoor` — returns
///   exactly the listed devices in the order requested, deduplicating any
///   repeated IDs.  Unknown IDs are silently omitted.
/// - `?adapter=open_meteo` — returns every device whose ID starts with
///   `"open_meteo:"`.  Returns an empty array for an unknown adapter name
///   (use `GET /adapters/{id}/devices` if you want a 404 for missing adapters).
pub async fn list_devices(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> Json<Vec<homecmdr_core::model::Device>> {
    let ids = requested_device_ids(raw_query.as_deref());
    if !ids.is_empty() {
        // ?ids= filter — deduplicate while preserving order.
        let mut seen = std::collections::HashSet::new();
        let devices = ids
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .filter_map(|id| state.runtime.registry().get(&DeviceId(id)))
            .collect();
        return Json(devices);
    }

    if let Some(adapter) = requested_adapter(raw_query.as_deref()) {
        return Json(state.runtime.registry().list_devices_for_adapter(&adapter));
    }

    Json(state.runtime.registry().list())
}

/// Parses `?ids=value` query parameters from the raw query string.
///
/// Multiple `ids` values are collected into a `Vec<String>`.  Returns an
/// empty vec when no `ids` parameter is present.
fn requested_device_ids(raw_query: Option<&str>) -> Vec<String> {
    raw_query
        .into_iter()
        .flat_map(|query| url::form_urlencoded::parse(query.as_bytes()))
        .filter_map(|(key, value)| (key == "ids").then(|| value.into_owned()))
        .collect()
}

/// Parses a single `?adapter=value` query parameter from the raw query string.
///
/// Returns the first adapter name found, or `None` when the parameter is absent
/// or its value is blank.
fn requested_adapter(raw_query: Option<&str>) -> Option<String> {
    raw_query
        .into_iter()
        .flat_map(|query| url::form_urlencoded::parse(query.as_bytes()))
        .find_map(|(key, value)| {
            (key == "adapter" && !value.is_empty()).then(|| value.into_owned())
        })
}

/// Returns a single device by its full ID (e.g. `"elgato-lights:Key Light"`).
/// Returns 404 if the device is not in the registry.
pub async fn get_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<homecmdr_core::model::Device>, ApiError> {
    state
        .runtime
        .registry()
        .get(&DeviceId(id.clone()))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("device '{id}' not found")))
}

/// Assigns (or un-assigns) a device to a room.
///
/// Pass `{"room_id": "living-room"}` to assign, or `{"room_id": null}` to
/// remove the device from its current room.  Returns the updated device.
pub async fn assign_device_room(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<AssignRoomRequest>,
) -> Result<Json<homecmdr_core::model::Device>, ApiError> {
    let device_id = DeviceId(id.clone());

    if state.runtime.registry().get(&device_id).is_none() {
        return Err(ApiError::not_found(format!("device '{id}' not found")));
    }

    let room_id = request.room_id.map(RoomId);
    state
        .runtime
        .registry()
        .assign_device_to_room(&device_id, room_id)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    state
        .runtime
        .registry()
        .get(&device_id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("device '{id}' not found")))
}

/// Sends a command to a single device.
///
/// For WASM adapters the command is dispatched synchronously through the
/// runtime and returns `{"status":"ok"}`.  For IPC adapters the command is
/// published as a `DeviceCommandDispatched` bus event and returns
/// `{"status":"dispatched"}` — the adapter process handles it asynchronously.
///
/// Every attempt is written to the command audit log.
pub async fn command_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(command): Json<DeviceCommand>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let device_id = DeviceId(id.clone());

    if state.runtime.registry().get(&device_id).is_none() {
        return Err(ApiError::not_found(format!("device '{id}' not found")));
    }

    command
        .validate()
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    let command_to_run = command.clone();
    match state
        .runtime
        .command_device(&device_id, command_to_run)
        .await
    {
        Ok(true) => {
            persist_command_audit(
                &state,
                CommandAuditEntry {
                    recorded_at: Utc::now(),
                    source: "device".to_string(),
                    room_id: state
                        .runtime
                        .registry()
                        .get(&device_id)
                        .and_then(|device| device.room_id),
                    device_id: device_id.clone(),
                    command,
                    status: "ok".to_string(),
                    message: None,
                },
            );
            Ok(Json(json!({ "status": "ok" })))
        }
        Ok(false) => {
            // If this device belongs to an IPC adapter, dispatch the command
            // via the event bus.  The adapter process subscribes to the
            // WebSocket `/events` stream and handles it.
            let adapter_name = device_id.0.split_once(':').map(|(n, _)| n.to_string());
            let is_ipc = adapter_name
                .as_deref()
                .map(|n| state.ipc_adapter_names.contains(n))
                .unwrap_or(false);

            if is_ipc {
                state.runtime.bus().publish(Event::DeviceCommandDispatched {
                    id: device_id.clone(),
                    command: command.clone(),
                });
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "device".to_string(),
                        room_id: state
                            .runtime
                            .registry()
                            .get(&device_id)
                            .and_then(|device| device.room_id),
                        device_id: device_id.clone(),
                        command,
                        status: "dispatched".to_string(),
                        message: None,
                    },
                );
                Ok(Json(json!({ "status": "dispatched" })))
            } else {
                Err(ApiError::not_implemented(format!(
                    "device commands are not implemented for '{id}'"
                )))
            }
        }
        Err(error) => {
            persist_command_audit(
                &state,
                CommandAuditEntry {
                    recorded_at: Utc::now(),
                    source: "device".to_string(),
                    room_id: state
                        .runtime
                        .registry()
                        .get(&device_id)
                        .and_then(|device| device.room_id),
                    device_id: device_id.clone(),
                    command,
                    status: "error".to_string(),
                    message: Some(error.to_string()),
                },
            );
            Err(ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))
        }
    }
}

/// Sends the same command to every device in a room.
///
/// Returns a per-device result list rather than failing the whole request if
/// one device errors.  Returns 404 if the room does not exist.
pub async fn command_room_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(command): Json<DeviceCommand>,
) -> Result<Json<Vec<RoomCommandResult>>, ApiError> {
    let room_id = RoomId(id.clone());
    if state.runtime.registry().get_room(&room_id).is_none() {
        return Err(ApiError::not_found(format!("room '{id}' not found")));
    }

    command
        .validate()
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    let devices = state.runtime.registry().list_devices_in_room(&room_id);
    let mut results = Vec::with_capacity(devices.len());

    for device in devices {
        let device_id = device.id.clone();
        let audit_command = command.clone();
        match state
            .runtime
            .command_device(&device_id, command.clone())
            .await
        {
            Ok(true) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "room".to_string(),
                        room_id: Some(room_id.clone()),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "ok".to_string(),
                        message: None,
                    },
                );
                results.push(RoomCommandResult {
                    device_id: device_id.0,
                    status: "ok",
                    message: None,
                });
            }
            Ok(false) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "room".to_string(),
                        room_id: Some(room_id.clone()),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "unsupported".to_string(),
                        message: Some("device commands are not implemented".to_string()),
                    },
                );
                results.push(RoomCommandResult {
                    device_id: device_id.0,
                    status: "unsupported",
                    message: Some("device commands are not implemented".to_string()),
                });
            }
            Err(error) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "room".to_string(),
                        room_id: Some(room_id.clone()),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "error".to_string(),
                        message: Some(error.to_string()),
                    },
                );
                results.push(RoomCommandResult {
                    device_id: device_id.0,
                    status: "error",
                    message: Some(error.to_string()),
                });
            }
        }
    }

    Ok(Json(results))
}

/// Sends the same command to every device in a group.
///
/// Returns a per-device result list rather than failing the whole request if
/// one device errors.  Returns 404 if the group does not exist.
pub async fn command_group_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(command): Json<DeviceCommand>,
) -> Result<Json<Vec<GroupCommandResult>>, ApiError> {
    let group_id = GroupId(id.clone());
    if state.runtime.registry().get_group(&group_id).is_none() {
        return Err(ApiError::not_found(format!("group '{id}' not found")));
    }

    command
        .validate()
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;

    let devices = state.runtime.registry().list_devices_in_group(&group_id);
    let mut results = Vec::with_capacity(devices.len());

    for device in devices {
        let device_id = device.id.clone();
        let audit_command = command.clone();
        match state
            .runtime
            .command_device(&device_id, command.clone())
            .await
        {
            Ok(true) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "group".to_string(),
                        room_id: device.room_id.clone(),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "ok".to_string(),
                        message: None,
                    },
                );
                results.push(GroupCommandResult {
                    device_id: device_id.0,
                    status: "ok",
                    message: None,
                });
            }
            Ok(false) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "group".to_string(),
                        room_id: device.room_id.clone(),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "unsupported".to_string(),
                        message: Some("device commands are not implemented".to_string()),
                    },
                );
                results.push(GroupCommandResult {
                    device_id: device_id.0,
                    status: "unsupported",
                    message: Some("device commands are not implemented".to_string()),
                });
            }
            Err(error) => {
                persist_command_audit(
                    &state,
                    CommandAuditEntry {
                        recorded_at: Utc::now(),
                        source: "group".to_string(),
                        room_id: device.room_id.clone(),
                        device_id: device_id.clone(),
                        command: audit_command,
                        status: "error".to_string(),
                        message: Some(error.to_string()),
                    },
                );
                results.push(GroupCommandResult {
                    device_id: device_id.0,
                    status: "error",
                    message: Some(error.to_string()),
                });
            }
        }
    }

    Ok(Json(results))
}
