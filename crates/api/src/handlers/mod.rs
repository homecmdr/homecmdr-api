//! HTTP request handlers, one module per resource domain.
//!
//! Each submodule contains `async fn` handlers that are wired up in
//! [`crate::router`].  Handlers receive an Axum `State<AppState>` extractor
//! and return JSON responses or `ApiError` on failure.
//!
//! | Module          | Routes it owns                                      |
//! |-----------------|-----------------------------------------------------|
//! | `health`        | `GET /health`, `GET /ready`                         |
//! | `adapters`      | `GET /adapters`, `GET /capabilities`                |
//! | `devices`       | Rooms, groups, devices, device commands             |
//! | `scenes`        | Listing, executing, and reloading scenes            |
//! | `automations`   | Listing, enabling, executing, and validating autos  |
//! | `history`       | Device attribute history and command audit log      |
//! | `events`        | WebSocket event stream, device ingest endpoint      |
//! | `plugins`       | Plugin catalog and Lua file management              |
//! | `admin`         | Diagnostics, catalog reloads, API key management    |

pub mod adapters;
pub mod admin;
pub mod automations;
pub mod devices;
pub mod events;
pub mod health;
pub mod history;
pub mod plugins;
pub mod scenes;
