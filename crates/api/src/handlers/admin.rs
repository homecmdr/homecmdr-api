//! Admin-tier handlers — diagnostics, catalog reloads, and API key management.
//!
//! All routes in this module require the `admin` role (or the master key).
//! They expose privileged operations that should not be accessible to regular
//! read/write clients.
//!
//! - `GET /diagnostics` — full system snapshot: device/room/group counts,
//!   scene/automation counts, history config, per-adapter health.
//! - `GET /diagnostics/reload_watch` — which directories are being watched
//!   for automatic hot-reload and whether each watcher is enabled.
//! - `POST /scenes/reload` — re-read scene files from disk.
//! - `POST /automations/reload` — re-read automation files from disk.
//! - `POST /scripts/reload` — acknowledge a scripts directory change.
//! - `POST /plugins/reload` — re-scan the plugin directory and update the catalog.
//! - `POST /auth/keys` / `GET /auth/keys` / `DELETE /auth/keys/{id}` — API
//!   key lifecycle management.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use homecmdr_core::event::Event;
use homecmdr_plugin_host::PluginManager;
use rand::Rng;

use homecmdr_core::model::PersonId;

use crate::dto::{
    ApiError, ApiKeyResponse, CreateApiKeyRequest, CreateApiKeyResponse, DiagnosticsResponse,
    ReloadErrorDetail, ReloadResponse, ReloadWatchItem, ReloadWatchResponse,
};
use crate::helpers::{internal_api_error, read_lock, sha256_hex, write_lock};
use crate::reload::{
    reload_automations_internal, reload_controller_from_state, reload_response_from_result,
    reload_scenes_internal, reload_scripts_internal,
};
use crate::state::AppState;

/// Returns a rich system snapshot for monitoring and debugging.
///
/// Combines health status, entity counts (devices, rooms, groups, scenes,
/// automations), history configuration, and per-adapter health into a single
/// response.
pub async fn diagnostics(State(state): State<AppState>) -> Json<DiagnosticsResponse> {
    let health = state.health.response();
    let scene_count = {
        let runner = read_lock(&state.scenes);
        runner.summaries().len()
    };
    let automation_count = {
        let catalog = read_lock(&state.automations);
        catalog.summaries().len()
    };

    Json(DiagnosticsResponse {
        status: health.status.clone(),
        ready: health.ready,
        devices: state.runtime.registry().list().len(),
        rooms: state.runtime.registry().list_rooms().len(),
        groups: state.runtime.registry().list_groups().len(),
        scenes: scene_count,
        automations: automation_count,
        history_enabled: state.history.enabled,
        default_history_limit: state.history.default_limit,
        max_history_limit: state.history.max_limit,
        runtime: health.runtime,
        persistence: health.persistence,
        automations_component: health.automations,
        adapters: health.adapters,
    })
}

/// Lists which directories are monitored for automatic hot-reload and whether
/// each watch is currently enabled according to config.
pub async fn reload_watch_diagnostics(State(state): State<AppState>) -> Json<ReloadWatchResponse> {
    Json(ReloadWatchResponse {
        status: "ok",
        watches: vec![
            ReloadWatchItem {
                target: "scenes",
                enabled: state.scenes_watch,
                directory: state.scenes_directory.clone(),
            },
            ReloadWatchItem {
                target: "automations",
                enabled: state.automations_watch,
                directory: state.automations_directory.clone(),
            },
            ReloadWatchItem {
                target: "scripts",
                enabled: state.scripts_watch,
                directory: state.scripts_directory.clone(),
            },
        ],
    })
}

/// Re-reads all scene files from disk and swaps the in-memory catalog.
///
/// Runs on a blocking thread so it does not tie up the async executor during
/// file I/O.
pub async fn reload_scenes(
    State(state): State<AppState>,
) -> Result<Json<ReloadResponse>, ApiError> {
    let controller = reload_controller_from_state(&state);

    let result = tokio::task::spawn_blocking(move || reload_scenes_internal(&controller))
        .await
        .map_err(|error| internal_api_error(anyhow::anyhow!(error.to_string())))?;
    Ok(Json(reload_response_from_result("scenes", result)))
}

/// Re-reads all automation files from disk, swaps the in-memory catalog, and
/// replaces the running automation runner.  Previously enabled/disabled states
/// are preserved across the reload.
pub async fn reload_automations(
    State(state): State<AppState>,
) -> Result<Json<ReloadResponse>, ApiError> {
    let controller = reload_controller_from_state(&state);

    let result = tokio::task::spawn_blocking(move || reload_automations_internal(&controller))
        .await
        .map_err(|error| internal_api_error(anyhow::anyhow!(error.to_string())))?;
    Ok(Json(reload_response_from_result("automations", result)))
}

/// Acknowledges a scripts directory change and counts the `.lua` files.
///
/// Scripts are not pre-parsed into a catalog — they are loaded on demand by
/// `require(...)` inside scenes/automations — so this handler only validates
/// that the directory is readable and returns the file count.
pub async fn reload_scripts(
    State(state): State<AppState>,
) -> Result<Json<ReloadResponse>, ApiError> {
    let controller = reload_controller_from_state(&state);

    let result = tokio::task::spawn_blocking(move || reload_scripts_internal(&controller))
        .await
        .map_err(|error| internal_api_error(anyhow::anyhow!(error.to_string())))?;
    Ok(Json(reload_response_from_result("scripts", result)))
}

/// Re-scans the plugin directory and updates the in-memory plugin catalog
/// (used by `GET /plugins`).  Does not restart any running adapter processes
/// — that requires a full server restart.
pub async fn reload_plugins(
    State(state): State<AppState>,
) -> Result<Json<ReloadResponse>, ApiError> {
    if !state.plugins_enabled {
        return Ok(Json(ReloadResponse {
            status: "error",
            target: "plugins",
            loaded_count: 0,
            errors: vec![ReloadErrorDetail {
                file: state.plugins_directory.clone(),
                message: "plugin loading is not enabled".to_string(),
            }],
            duration_ms: 0,
        }));
    }

    let started = std::time::Instant::now();
    let plugin_dir = std::path::PathBuf::from(&state.plugins_directory);
    let runtime = state.runtime.clone();

    let manifests = tokio::task::spawn_blocking(move || {
        if plugin_dir.exists() {
            PluginManager::scan_manifests(&plugin_dir)
        } else {
            Vec::new()
        }
    })
    .await
    .map_err(|e| internal_api_error(anyhow::anyhow!(e.to_string())))?;

    let loaded_count = manifests.len();
    let duration_ms = started.elapsed().as_millis() as u64;

    *write_lock(&state.plugin_catalog) = manifests;

    runtime.bus().publish(Event::PluginCatalogReloaded {
        loaded_count,
        duration_ms,
    });

    Ok(Json(ReloadResponse {
        status: "ok",
        target: "plugins",
        loaded_count,
        errors: Vec::new(),
        duration_ms: duration_ms as u128,
    }))
}

/// Creates a new API key with the requested label and role.
///
/// Generates a cryptographically random 32-byte token, stores its SHA-256
/// hash in the database, and returns the plaintext token **once** — it
/// cannot be retrieved again.  The caller must store it immediately.
pub async fn create_api_key(
    State(state): State<AppState>,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<Json<CreateApiKeyResponse>, ApiError> {
    let key_store = state
        .auth_key_store
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "key store not available"))?;

    let token: String = {
        let bytes: [u8; 32] = rand::thread_rng().gen();
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    };
    let token_hash = sha256_hex(&token);

    let person_id = req.person_id.map(PersonId);
    let record = key_store
        .create_api_key(&token_hash, &req.label, req.role, person_id.as_ref())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(CreateApiKeyResponse {
        id: record.id,
        label: record.label,
        role: record.role,
        person_id: record.person_id.map(|p| p.0),
        token,
        created_at: record.created_at,
    }))
}

/// Lists all API keys (without their plaintext tokens — those are never stored).
pub async fn list_api_keys(
    State(state): State<AppState>,
) -> Result<Json<Vec<ApiKeyResponse>>, ApiError> {
    let key_store = state
        .auth_key_store
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "key store not available"))?;

    let keys = key_store
        .list_api_keys()
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(
        keys.into_iter()
            .map(|r| ApiKeyResponse {
                id: r.id,
                label: r.label,
                role: r.role,
                person_id: r.person_id.map(|p| p.0),
                created_at: r.created_at,
                last_used_at: r.last_used_at,
            })
            .collect(),
    ))
}

/// Revokes (permanently deletes) an API key by its integer ID.
/// Returns 404 if the key does not exist.  Returns 204 No Content on success.
pub async fn delete_api_key(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let key_store = state
        .auth_key_store
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "key store not available"))?;

    let deleted = key_store
        .revoke_api_key(id)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("api key {id} not found")))
    }
}
