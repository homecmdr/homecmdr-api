//! Small utility functions shared by multiple handlers and workers.
//!
//! These are things that don't belong to any one feature area — hashing,
//! lock helpers, history validation, error formatting, and so on.

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use axum::http::StatusCode;
use homecmdr_automations::AutomationSummary;
use homecmdr_core::capability::CapabilityOwnershipPolicy;
use homecmdr_core::capability::CapabilitySchema;
use homecmdr_core::model::{Device, DeviceGroup, DeviceId, Room};
use homecmdr_core::store::{CommandAuditEntry, DeviceStore, SceneExecutionHistoryEntry};
use sha2::{Digest, Sha256};

use crate::dto::{
    ApiError, AutomationResponse, CapabilityOwnershipResponse, CapabilitySchemaResponse,
    HistoryQuery,
};
use crate::state::AppState;

// ── Lock helpers ──────────────────────────────────────────────────────────────
// These recover from a poisoned lock (caused by a thread panic) rather than
// propagating the panic.  In practice a panic inside a lock is rare, but
// recovering gracefully is better than crashing the whole server.

pub fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|p| p.into_inner())
}

pub fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|p| p.into_inner())
}

// ── Hashing ───────────────────────────────────────────────────────────────────

/// Return the SHA-256 hash of `input` as a lowercase hex string.
/// Used to hash bearer tokens before storing or comparing them — we never
/// keep plaintext tokens anywhere.
pub fn sha256_hex(input: &str) -> String {
    let hash = Sha256::digest(input.as_bytes());
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

// ── Persistence reconciliation ────────────────────────────────────────────────

/// Sync the database to match the current in-memory registry state.
///
/// After startup (when we restore persisted devices into the registry) there
/// may be devices/rooms/groups in the database that no longer exist in memory.
/// This function removes any "stale" rows and upserts everything that is
/// currently known.
///
/// Also called by the persistence worker when it falls behind the event stream
/// and misses some events.
pub async fn reconcile_device_store(
    rooms: Vec<Room>,
    groups: Vec<DeviceGroup>,
    devices: Vec<Device>,
    store: Arc<dyn DeviceStore>,
) -> Result<()> {
    // Build a set of IDs currently in the database and a set currently in
    // memory, then delete anything in the database but not in memory.
    let persisted_room_ids = store
        .load_all_rooms()
        .await
        .context("failed to load persisted rooms for reconciliation")?
        .into_iter()
        .map(|room| room.id)
        .collect::<std::collections::HashSet<_>>();
    let current_room_ids = rooms
        .iter()
        .map(|room| room.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let persisted_group_ids = store
        .load_all_groups()
        .await
        .context("failed to load persisted groups for reconciliation")?
        .into_iter()
        .map(|group| group.id)
        .collect::<std::collections::HashSet<_>>();
    let current_group_ids = groups
        .iter()
        .map(|group| group.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let persisted_ids = store
        .load_all_devices()
        .await
        .context("failed to load persisted devices for reconciliation")?
        .into_iter()
        .map(|device| device.id)
        .collect::<std::collections::HashSet<_>>();
    let current_ids = devices
        .iter()
        .map(|device| device.id.clone())
        .collect::<std::collections::HashSet<_>>();

    // Remove stale rooms (in DB but no longer in memory).
    for stale_id in persisted_room_ids.difference(&current_room_ids) {
        store.delete_room(stale_id).await.with_context(|| {
            format!(
                "failed to delete stale room '{}' during reconciliation",
                stale_id.0
            )
        })?;
    }

    for room in rooms {
        store.save_room(&room).await.with_context(|| {
            format!("failed to save room '{}' during reconciliation", room.id.0)
        })?;
    }

    // Remove stale groups.
    for stale_id in persisted_group_ids.difference(&current_group_ids) {
        store.delete_group(stale_id).await.with_context(|| {
            format!(
                "failed to delete stale group '{}' during reconciliation",
                stale_id.0
            )
        })?;
    }

    for group in groups {
        store.save_group(&group).await.with_context(|| {
            format!(
                "failed to save group '{}' during reconciliation",
                group.id.0
            )
        })?;
    }

    // Remove stale devices.
    for stale_id in persisted_ids.difference(&current_ids) {
        store.delete_device(stale_id).await.with_context(|| {
            format!(
                "failed to delete stale device '{}' during reconciliation",
                stale_id.0
            )
        })?;
    }

    for device in devices {
        store.save_device(&device).await.with_context(|| {
            format!(
                "failed to save device '{}' during reconciliation",
                device.id.0
            )
        })?;
    }

    Ok(())
}

// ── History guard helpers ─────────────────────────────────────────────────────

/// Return the device store for history queries, or a 404 error if history
/// is disabled or persistence is not configured.
pub fn history_store(state: &AppState) -> Result<Arc<dyn DeviceStore>, ApiError> {
    if !state.history.enabled {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "history is disabled"));
    }

    state.store.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "history is unavailable because persistence is disabled",
        )
    })
}

/// Return a 400 error if the caller supplied a `start` that is after `end`.
pub fn ensure_history_query_range(query: &HistoryQuery) -> Result<(), ApiError> {
    if let (Some(start), Some(end)) = (query.start, query.end) {
        if start > end {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "history query start must be <= end",
            ));
        }
    }

    Ok(())
}

/// Return a 404 error if the given device ID is not in the in-memory registry.
pub fn ensure_device_exists(
    state: &AppState,
    device_id: &DeviceId,
    raw_id: &str,
) -> Result<(), ApiError> {
    if state.runtime.registry().get(device_id).is_none() {
        return Err(ApiError::not_found(format!("device '{raw_id}' not found")));
    }

    Ok(())
}

// ── Error formatting ──────────────────────────────────────────────────────────

/// Convert an unexpected internal error into a 500 response and log it.
/// Use this for errors the caller can't do anything about (DB failures, etc.).
pub fn internal_api_error(error: anyhow::Error) -> ApiError {
    tracing::error!(error = %error, "internal API error");
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
}

// ── Capability response helpers ───────────────────────────────────────────────
// These thin wrappers exist so handlers can call a local function without
// needing to import from `dto` directly.

pub fn capability_schema_response(schema: CapabilitySchema) -> CapabilitySchemaResponse {
    crate::dto::capability_schema_response(schema)
}

pub fn capability_ownership_response(
    policy: CapabilityOwnershipPolicy,
) -> CapabilityOwnershipResponse {
    crate::dto::capability_ownership_response(policy)
}

// ── Automation response builder ───────────────────────────────────────────────

/// Build an `AutomationResponse` from a summary, enriched with the latest run
/// info from the history store (if history is enabled).
pub async fn automation_response(
    state: &AppState,
    summary: AutomationSummary,
) -> Result<AutomationResponse, ApiError> {
    let automation_id = summary.id.clone();
    // Fetch the single most-recent history entry to populate `last_run` / `last_error`.
    let latest_run = if state.history.enabled {
        if let Some(store) = &state.store {
            store
                .load_automation_history(&automation_id, None, None, 1)
                .await
                .map_err(internal_api_error)?
                .into_iter()
                .next()
        } else {
            None
        }
    } else {
        None
    };

    Ok(AutomationResponse {
        id: summary.id,
        name: summary.name,
        description: summary.description,
        trigger_type: summary.trigger_type,
        condition_count: summary.condition_count,
        status: if {
            let controller = read_lock(&state.automation_control);
            controller.is_enabled(&automation_id).unwrap_or(true)
        } {
            "enabled"
        } else {
            "disabled"
        },
        last_run: latest_run.as_ref().map(|entry| entry.executed_at),
        last_error: latest_run.and_then(|entry| entry.error),
    })
}

// ── Fire-and-forget persistence helpers ──────────────────────────────────────
// These spawn background tasks so that writing to the database never blocks
// the HTTP response.

/// Save a command audit record in the background.  Does nothing if history
/// is disabled or persistence is not configured.
pub fn persist_command_audit(state: &AppState, entry: CommandAuditEntry) {
    let Some(store) = state.store.clone() else {
        return;
    };
    if !state.history.enabled {
        return;
    }

    tokio::spawn(async move {
        if let Err(error) = store.save_command_audit(&entry).await {
            tracing::error!(error = %error, device_id = %entry.device_id.0, "failed to persist command audit history");
        }
    });
}

/// Save a scene execution record in the background.
pub fn persist_scene_history(state: &AppState, entry: SceneExecutionHistoryEntry) {
    let Some(store) = state.store.clone() else {
        return;
    };
    if !state.history.enabled {
        return;
    }

    tokio::spawn(async move {
        if let Err(error) = store.save_scene_execution(&entry).await {
            tracing::error!(error = %error, scene_id = %entry.scene_id, "failed to persist scene execution history");
        }
    });
}
