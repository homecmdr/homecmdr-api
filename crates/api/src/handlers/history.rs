//! Device attribute history and command audit log handlers.
//!
//! These endpoints require persistence to be enabled — they return HTTP 503
//! when the store is unavailable.  All three support optional `start`, `end`,
//! and `limit` query parameters for pagination.
//!
//! - `GET /devices/{id}/history` — full state snapshots for a device over
//!   time.
//! - `GET /devices/{id}/history/{attribute}` — history for a single attribute
//!   (e.g. `brightness`, `power`) rather than the whole device state.
//! - `GET /audit/commands` — log of every command sent through the API with
//!   its source, target device, and outcome.

use axum::extract::{Path, Query, State};
use axum::Json;
use homecmdr_core::model::DeviceId;

use crate::dto::{
    ApiError, AttributeHistoryResponse, CommandAuditResponse, DeviceHistoryResponse, HistoryQuery,
};
use crate::helpers::{
    ensure_device_exists, ensure_history_query_range, history_store, internal_api_error,
};
use crate::state::AppState;

/// Returns full state snapshots for a device over the requested time window.
/// Returns 404 if the device is not currently in the registry.
pub async fn get_device_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<DeviceHistoryResponse>, ApiError> {
    let device_id = DeviceId(id.clone());
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    ensure_device_exists(&state, &device_id, &id)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_device_history(&device_id, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(DeviceHistoryResponse {
        device_id: id,
        entries,
    }))
}

/// Returns history for a single attribute of a device (e.g. just
/// `brightness` values over time).  Returns 404 if the device does not exist.
pub async fn get_attribute_history(
    State(state): State<AppState>,
    Path((id, attribute)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<AttributeHistoryResponse>, ApiError> {
    let device_id = DeviceId(id.clone());
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    ensure_device_exists(&state, &device_id, &id)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_attribute_history(&device_id, &attribute, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(AttributeHistoryResponse {
        device_id: id,
        attribute,
        entries,
    }))
}

/// Returns the command audit log — every API command that was attempted,
/// including the source (device / room / group), the target device ID, the
/// command payload, and whether it succeeded or failed.
pub async fn get_command_audit(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<CommandAuditResponse>, ApiError> {
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_command_audit(None, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(CommandAuditResponse { entries }))
}
