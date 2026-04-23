//! Scene listing, execution, reload, and history handlers.
//!
//! Scenes are Lua scripts that group multiple device commands together and
//! run them as a single named action.  The execution model supports several
//! modes (run-once, queue, saturate) controlled by the scene definition
//! itself.
//!
//! - `GET /scenes` — list all loaded scenes.
//! - `POST /scenes/{id}/execute` — run a scene by ID.
//! - `GET /scenes/{id}/history` — paginated execution history for a scene.
//!
//! The `reload_scenes` handler is also here but is only wired into the
//! admin tier in the router.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use homecmdr_scenes::{SceneRunOutcome, SceneSummary};

use crate::dto::ReloadResponse;
use crate::dto::{ApiError, HistoryQuery, SceneExecuteResponse, SceneHistoryResponse};
use crate::helpers::{
    ensure_history_query_range, history_store, internal_api_error, persist_scene_history, read_lock,
};
use crate::reload::{
    reload_controller_from_state, reload_response_from_result, reload_scenes_internal,
};
use crate::state::AppState;
use homecmdr_core::store::{SceneExecutionHistoryEntry, SceneStepResult};

/// Returns a summary of every scene in the currently loaded catalog.
pub async fn list_scenes(State(state): State<AppState>) -> Json<Vec<SceneSummary>> {
    let runner = read_lock(&state.scenes);
    Json(runner.summaries())
}

/// Re-reads all scene files from disk and swaps the in-memory catalog.
/// Admin-tier endpoint; wired at `POST /scenes/reload` in the router.
pub async fn reload_scenes(
    State(state): State<AppState>,
) -> Result<Json<ReloadResponse>, ApiError> {
    let controller = reload_controller_from_state(&state);

    let result = tokio::task::spawn_blocking(move || reload_scenes_internal(&controller))
        .await
        .map_err(|error| internal_api_error(anyhow::anyhow!(error.to_string())))?;
    Ok(Json(reload_response_from_result("scenes", result)))
}

/// Executes a scene by ID and returns the per-step results.
///
/// Possible HTTP responses:
/// - **200 OK** with `{"status":"ok","results":[...]}` — scene ran to completion.
/// - **202 Accepted** — scene was queued (runs in the background).
/// - **423 Locked** — scene is already running and its execution mode is
///   `saturate` (drops duplicate concurrent runs).
/// - **400 Bad Request** — scene failed during execution.
/// - **404 Not Found** — no scene with that ID exists.
///
/// Each execution attempt is written to the scene history log regardless of
/// outcome.
pub async fn execute_scene(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let executed_at = Utc::now();
    let runner = {
        let guard = read_lock(&state.scenes);
        guard.clone()
    };
    let outcome = runner
        .execute(&id, state.runtime.clone())
        .await
        .map_err(|error| {
            persist_scene_history(
                &state,
                SceneExecutionHistoryEntry {
                    executed_at,
                    scene_id: id.clone(),
                    status: "error".to_string(),
                    error: Some(error.to_string()),
                    results: Vec::new(),
                },
            );
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        })?;

    match outcome {
        SceneRunOutcome::NotFound => Err(ApiError::not_found(format!("scene '{id}' not found"))),
        SceneRunOutcome::Dropped => {
            persist_scene_history(
                &state,
                SceneExecutionHistoryEntry {
                    executed_at,
                    scene_id: id.clone(),
                    status: "skipped".to_string(),
                    error: Some("scene already running (execution mode saturated)".to_string()),
                    results: Vec::new(),
                },
            );
            Err(ApiError::new(
                StatusCode::LOCKED,
                format!("scene '{id}' is already running"),
            ))
        }
        SceneRunOutcome::Queued => {
            persist_scene_history(
                &state,
                SceneExecutionHistoryEntry {
                    executed_at,
                    scene_id: id.clone(),
                    status: "queued".to_string(),
                    error: None,
                    results: Vec::new(),
                },
            );
            Ok(StatusCode::ACCEPTED.into_response())
        }
        SceneRunOutcome::Completed(results) => {
            persist_scene_history(
                &state,
                SceneExecutionHistoryEntry {
                    executed_at,
                    scene_id: id.clone(),
                    status: "ok".to_string(),
                    error: None,
                    results: results
                        .iter()
                        .map(|r| SceneStepResult {
                            target: r.target.clone(),
                            status: r.status.to_string(),
                            message: r.message.clone(),
                        })
                        .collect(),
                },
            );
            Ok(Json(SceneExecuteResponse {
                status: "ok",
                results,
            })
            .into_response())
        }
    }
}

/// Returns paginated execution history for a single scene.
///
/// Supports optional `start`, `end`, and `limit` query parameters.  Requires
/// persistence to be enabled; returns 503 otherwise.
pub async fn get_scene_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<SceneHistoryResponse>, ApiError> {
    let store = history_store(&state)?;
    ensure_history_query_range(&query)?;
    let limit = state.history.resolve_limit(query.limit)?;

    let entries = store
        .load_scene_history(&id, query.start, query.end, limit)
        .await
        .map_err(internal_api_error)?;

    Ok(Json(SceneHistoryResponse {
        scene_id: id,
        entries,
    }))
}
