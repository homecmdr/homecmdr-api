//! Automation listing, enable/disable, manual execution, validation, and
//! history handlers.
//!
//! Automations are Lua scripts that fire automatically in response to trigger
//! conditions (device state changes, time-based rules, sunrise/sunset, etc.).
//! This module exposes the endpoints that let clients inspect and control them
//! at runtime.
//!
//! - `GET /automations` — list all loaded automations with their current
//!   enabled state.
//! - `GET /automations/{id}` — details for a single automation.
//! - `POST /automations/{id}/enabled` — enable or disable an automation
//!   without reloading the catalog.
//! - `POST /automations/{id}/execute` — run an automation manually with an
//!   optional trigger payload.
//! - `POST /automations/{id}/validate` — re-parse and validate an automation
//!   without running it.
//! - `GET /automations/{id}/history` — paginated execution history.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use homecmdr_core::model::AttributeValue;

use crate::dto::{
    ApiError, AutomationEnabledRequest, AutomationExecuteResponse, AutomationHistoryResponse,
    AutomationResponse, AutomationValidateResponse, HistoryQuery, ManualAutomationRequest,
    ReloadResponse,
};
use crate::helpers::{
    automation_response, ensure_history_query_range, history_store, internal_api_error, read_lock,
};
use crate::reload::{
    reload_automations_internal, reload_controller_from_state, reload_response_from_result,
};
use crate::state::AppState;

/// Returns all automations with their current enabled flags and last-run
/// metadata.
pub async fn list_automations(
    State(state): State<AppState>,
) -> Result<Json<Vec<AutomationResponse>>, ApiError> {
    let controller = {
        let guard = read_lock(&state.automation_control);
        guard.clone()
    };
    let mut automations = Vec::new();
    for summary in controller.summaries() {
        automations.push(automation_response(&state, summary).await?);
    }
    Ok(Json(automations))
}

/// Re-reads all automation files from disk and swaps the in-memory catalog.
/// Admin-tier endpoint.
pub async fn reload_automations(
    State(state): State<AppState>,
) -> Result<Json<ReloadResponse>, ApiError> {
    let controller = reload_controller_from_state(&state);

    let result = tokio::task::spawn_blocking(move || reload_automations_internal(&controller))
        .await
        .map_err(|error| internal_api_error(anyhow::anyhow!(error.to_string())))?;
    Ok(Json(reload_response_from_result("automations", result)))
}

/// Returns details for a single automation, or 404 if it is not in the
/// catalog.
pub async fn get_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AutomationResponse>, ApiError> {
    let summary = state
        .automation_control
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("automation '{id}' not found")))?;

    Ok(Json(automation_response(&state, summary).await?))
}

/// Enables or disables an automation.
///
/// The change takes effect immediately for the running automation runner
/// without reloading the catalog.  The enabled state is preserved across
/// subsequent catalog reloads (until the server restarts).
pub async fn set_automation_enabled(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<AutomationEnabledRequest>,
) -> Result<Json<AutomationResponse>, ApiError> {
    let controller = {
        let guard = read_lock(&state.automation_control);
        guard.clone()
    };
    controller
        .set_enabled(&id, request.enabled)
        .map_err(|error| {
            if error.to_string().contains("not found") {
                ApiError::not_found(error.to_string())
            } else {
                ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
            }
        })?;

    let summary = controller
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("automation '{id}' not found")))?;
    Ok(Json(automation_response(&state, summary).await?))
}

/// Re-parses an automation's Lua source and validates its trigger
/// configuration without executing it.
///
/// Useful for catching syntax errors or misconfigured triggers after
/// editing a file, before a full catalog reload.
pub async fn validate_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AutomationValidateResponse>, ApiError> {
    let controller = {
        let guard = read_lock(&state.automation_control);
        guard.clone()
    };
    let summary = controller.validate(&id).map_err(|error| {
        if error.to_string().contains("not found") {
            ApiError::not_found(error.to_string())
        } else {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
    })?;

    Ok(Json(AutomationValidateResponse {
        status: "ok",
        automation: automation_response(&state, summary).await?,
    }))
}

/// Runs an automation immediately, bypassing its trigger conditions.
///
/// An optional `trigger_payload` JSON object can be provided to simulate a
/// specific trigger context (e.g. to test a device-state-change handler).
/// If omitted, a synthetic `{"type": "manual"}` payload is used.
///
/// Runs synchronously and returns the full execution result including any
/// errors and per-step durations.
pub async fn execute_automation_manually(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ManualAutomationRequest>,
) -> Result<Json<AutomationExecuteResponse>, ApiError> {
    let trigger_payload = request.trigger_payload.unwrap_or_else(|| {
        AttributeValue::Object(HashMap::from([(
            "type".to_string(),
            AttributeValue::Text("manual".to_string()),
        )]))
    });

    let controller = {
        let guard = read_lock(&state.automation_control);
        guard.clone()
    };
    let execution = controller
        .execute(
            &id,
            state.runtime.clone(),
            trigger_payload,
            state.trigger_context,
        )
        .map_err(|error| {
            if error.to_string().contains("not found") {
                ApiError::not_found(error.to_string())
            } else {
                ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
            }
        })?;

    Ok(Json(AutomationExecuteResponse {
        status: execution.status,
        error: execution.error,
        duration_ms: execution.duration_ms,
        results: execution.results,
    }))
}

/// Returns paginated execution history for a single automation.
///
/// Supports optional `start`, `end`, and `limit` query parameters.  Requires
/// persistence to be enabled; returns 503 otherwise.
pub async fn get_automation_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<AutomationHistoryResponse>, ApiError> {
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_automation_history(&id, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(AutomationHistoryResponse {
        automation_id: id,
        entries,
    }))
}
