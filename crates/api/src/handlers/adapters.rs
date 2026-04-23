//! Adapter and capability introspection handlers.
//!
//! These read-only endpoints let clients discover which adapters are
//! registered and what device capabilities the system supports.
//!
//! - `GET /adapters` — list all registered adapters with their health status.
//! - `GET /adapters/{id}` — detailed view of a single adapter.
//! - `GET /capabilities` — the full capability catalog: every capability key,
//!   its value schema, whether it is read-only, and the allowed action names.

use axum::extract::{Path, State};
use axum::Json;
use homecmdr_core::capability::{ALL_CAPABILITIES, CAPABILITY_OWNERSHIP};

use crate::dto::{
    AdapterDetailResponse, AdapterSummary, ApiError, CapabilityCatalogResponse, CapabilityResponse,
};
use crate::helpers::{capability_ownership_response, capability_schema_response};
use crate::state::AppState;

/// Returns a summary (name + status) for every registered adapter.
pub async fn adapters(State(state): State<AppState>) -> Json<Vec<AdapterSummary>> {
    Json(state.health.response().adapters)
}

/// Returns the detailed health and metadata for a single adapter by name.
/// Returns 404 if no adapter with that name is registered.
pub async fn get_adapter(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AdapterDetailResponse>, ApiError> {
    state
        .health
        .adapter_detail(&id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("adapter '{id}' not found")))
}

/// Returns every capability definition known to the system, sorted by key.
///
/// This is purely static data compiled into the binary — it does not depend
/// on which adapters are currently running.  Clients can use it to build
/// generic UIs that understand what each capability value means and what
/// commands are valid for it.
pub async fn list_capabilities() -> Json<CapabilityCatalogResponse> {
    let mut capabilities = ALL_CAPABILITIES
        .iter()
        .flat_map(|group| group.iter())
        .map(|definition| CapabilityResponse {
            domain: definition.domain,
            key: definition.key,
            schema: capability_schema_response(definition.schema),
            read_only: definition.read_only,
            actions: definition.actions.to_vec(),
            description: definition.description,
        })
        .collect::<Vec<_>>();
    capabilities.sort_by(|a, b| a.key.cmp(b.key));

    Json(CapabilityCatalogResponse {
        capabilities,
        ownership: capability_ownership_response(CAPABILITY_OWNERSHIP),
    })
}
