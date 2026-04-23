//! Liveness and readiness check handlers.
//!
//! These are the only routes that do **not** require authentication, so they
//! are safe to poll from a load balancer or container orchestrator.
//!
//! - `GET /health` — always returns 200 with a JSON status summary.  Use
//!   this to confirm the process is alive.
//! - `GET /ready` — returns 200 only after startup is fully complete
//!   (adapters initialised, catalogs loaded, etc.).  Returns 503 while the
//!   system is still booting.  Use this to gate traffic in a deployment.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use crate::dto::{ApiError, HealthResponse, ReadyResponse};
use crate::state::AppState;

/// Returns current system health as JSON.  Always succeeds with HTTP 200.
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(state.health.response())
}

/// Returns HTTP 200 + `{"status":"ok"}` once the system is fully ready.
/// Returns HTTP 503 while startup is still in progress.
pub async fn ready(State(state): State<AppState>) -> Result<Json<ReadyResponse>, ApiError> {
    if state.health.is_ready() {
        Ok(Json(ReadyResponse { status: "ok" }))
    } else {
        Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "system is not ready",
        ))
    }
}
