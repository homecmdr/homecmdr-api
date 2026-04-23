//! Axum router construction — maps URL paths to handler functions.
//!
//! The API is split into four security tiers, each wrapped in its own
//! `route_layer` that checks the caller's bearer token role before the
//! request reaches the handler:
//!
//! | Tier          | Role required        | Example routes                  |
//! |---------------|----------------------|---------------------------------|
//! | **public**    | none                 | `/health`, `/ready`             |
//! | **read**      | `read` or higher     | `GET /devices`, `GET /events`   |
//! | **write**     | `write` or higher    | `POST /devices/{id}/command`    |
//! | **admin**     | `admin`              | `POST /scenes/reload`, API keys |
//! | **automation**| `automation`/`admin` | `GET /files`, `PUT /files/{*}`  |
//!
//! Rate limiting (when enabled in config) is applied as an extra layer on
//! write routes only.
//!
//! CORS and request tracing are applied to the entire merged router.

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;
use homecmdr_core::config::Config;
use homecmdr_core::store::ApiKeyRole;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::handlers::{
    adapters, admin, automations, devices, events, health, history, plugins, scenes,
};
use crate::middleware::{check_auth, SharedRateLimit};
use crate::state::AppState;

/// Builds the complete Axum `Router` from `AppState` and config.
///
/// All route tiers are merged onto a single router, which is then wrapped
/// with the CORS layer and the `TraceLayer` for structured HTTP request logs.
pub fn app(state: AppState, config: &Config) -> Router {
    // Public — no authentication required.
    let public_routes = Router::new()
        .route("/health", get(health::health))
        .route("/ready", get(health::ready));

    // Read — bearer token with at least `read` role.
    let read_routes = Router::new()
        .route("/adapters", get(adapters::adapters))
        .route("/adapters/{id}", get(adapters::get_adapter))
        .route("/capabilities", get(adapters::list_capabilities))
        .route("/scenes", get(scenes::list_scenes))
        .route("/scenes/{id}/history", get(scenes::get_scene_history))
        .route("/automations", get(automations::list_automations))
        .route("/automations/{id}", get(automations::get_automation))
        .route(
            "/automations/{id}/history",
            get(automations::get_automation_history),
        )
        .route("/rooms", get(devices::list_rooms))
        .route("/rooms/{id}", get(devices::get_room))
        .route("/rooms/{id}/devices", get(devices::list_room_devices))
        .route("/groups", get(devices::list_groups))
        .route("/groups/{id}", get(devices::get_group))
        .route("/groups/{id}/devices", get(devices::list_group_devices))
        .route("/devices", get(devices::list_devices))
        .route("/devices/{id}", get(devices::get_device))
        .route("/devices/{id}/history", get(history::get_device_history))
        .route(
            "/devices/{id}/history/{attribute}",
            get(history::get_attribute_history),
        )
        .route("/audit/commands", get(history::get_command_audit))
        .route("/events", get(events::events))
        .route("/plugins", get(plugins::list_plugins))
        .route("/plugins/{name}", get(plugins::get_plugin))
        .route_layer({
            let s = state.clone();
            middleware::from_fn(move |req: Request, next: Next| {
                let s = s.clone();
                async move { check_auth(s, req, next, ApiKeyRole::Read).await }
            })
        });

    // Write — bearer token with at least `write` role.
    // Rate limiting is applied here (and only here) so that command endpoints
    // cannot be called faster than the configured requests-per-second budget.
    let rate_limiter: Option<SharedRateLimit> = config.api.rate_limit.enabled.then(|| {
        SharedRateLimit::new(
            config.api.rate_limit.requests_per_second,
            config.api.rate_limit.burst_size,
        )
    });
    let write_routes = Router::new()
        .route("/rooms", post(devices::create_room))
        .route("/rooms/{id}", delete(devices::delete_room))
        .route("/rooms/{id}/command", post(devices::command_room_devices))
        .route("/devices/{id}/room", post(devices::assign_device_room))
        .route("/devices/{id}/command", post(devices::command_device))
        .route("/groups", post(devices::create_group))
        .route("/groups/{id}", delete(devices::delete_group))
        .route("/groups/{id}/members", post(devices::set_group_members))
        .route("/groups/{id}/command", post(devices::command_group_devices))
        .route("/scenes/{id}/execute", post(scenes::execute_scene))
        .route(
            "/automations/{id}/enabled",
            post(automations::set_automation_enabled),
        )
        .route(
            "/automations/{id}/execute",
            post(automations::execute_automation_manually),
        )
        .route(
            "/automations/{id}/validate",
            post(automations::validate_automation),
        )
        .route("/ingest/devices", post(events::ingest_devices))
        .route_layer(middleware::from_fn(move |req: Request, next: Next| {
            let rate_limiter = rate_limiter.clone();
            async move {
                if let Some(limiter) = &rate_limiter {
                    if !limiter.try_acquire() {
                        return (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            "rate limit exceeded",
                        )
                            .into_response();
                    }
                }
                next.run(req).await
            }
        }))
        .route_layer({
            let s = state.clone();
            middleware::from_fn(move |req: Request, next: Next| {
                let s = s.clone();
                async move { check_auth(s, req, next, ApiKeyRole::Write).await }
            })
        });

    // Admin — bearer token with `admin` role.
    let admin_routes = Router::new()
        .route("/diagnostics", get(admin::diagnostics))
        .route(
            "/diagnostics/reload_watch",
            get(admin::reload_watch_diagnostics),
        )
        .route("/scenes/reload", post(admin::reload_scenes))
        .route("/automations/reload", post(admin::reload_automations))
        .route("/scripts/reload", post(admin::reload_scripts))
        .route("/plugins/reload", post(admin::reload_plugins))
        .route(
            "/auth/keys",
            post(admin::create_api_key).get(admin::list_api_keys),
        )
        .route("/auth/keys/{id}", delete(admin::delete_api_key))
        .route_layer({
            let s = state.clone();
            middleware::from_fn(move |req: Request, next: Next| {
                let s = s.clone();
                async move { check_auth(s, req, next, ApiKeyRole::Admin).await }
            })
        });

    // Automation — bearer token with `automation` or `admin` role.
    let automation_routes = Router::new()
        .route("/files", get(plugins::list_files))
        .route(
            "/files/{*path}",
            get(plugins::get_file).put(plugins::put_file),
        )
        .route_layer({
            let s = state.clone();
            middleware::from_fn(move |req: Request, next: Next| {
                let s = s.clone();
                async move { check_auth(s, req, next, ApiKeyRole::Automation).await }
            })
        });

    let router = Router::new()
        .merge(public_routes)
        .merge(read_routes)
        .merge(write_routes)
        .merge(admin_routes)
        .merge(automation_routes);

    router
        .layer(cors_layer(config))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Builds the CORS layer from config.
///
/// Returns an empty (permissive-to-nothing) layer when CORS is disabled, or
/// a layer that allows the exact origins listed in config with any method
/// and any header.  Typically used when a browser-based dashboard is hosted
/// on a different origin to the API.
pub fn cors_layer(config: &Config) -> CorsLayer {
    if !config.api.cors.enabled {
        return CorsLayer::new();
    }

    let allowed_origins = config
        .api
        .cors
        .allowed_origins
        .iter()
        .map(|origin| {
            origin
                .parse()
                .expect("validated CORS origin parses as header value")
        })
        .collect::<Vec<_>>();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed_origins))
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
}

/// Resolves when `Ctrl+C` is pressed or a `SIGTERM` signal is received.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
