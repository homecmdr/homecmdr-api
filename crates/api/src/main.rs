//! Entry point for the HomeCmdr API binary.
//!
//! This file just declares the sub-modules that make up the API, re-exports the
//! items that tests need to reach via `use super::*`, and then calls `startup::run()`
//! to actually boot the server.  All real logic lives in the sub-modules below.

// ── Sub-module declarations ───────────────────────────────────────────────────
// Each module lives in its own file under `src/`.

pub mod dto; // Request / response data shapes (serialised to/from JSON)
pub mod helpers; // Small utility functions shared across handlers
pub mod middleware; // Auth checking and rate limiting
pub mod reload; // Hot-reload logic for scenes, automations, and scripts
pub mod router; // Builds the axum Router and groups routes by permission tier
pub mod startup; // Boots the server: loads config, wires everything up, starts tasks
pub mod state; // AppState — the shared bag of data every request handler can read
pub mod workers; // Long-running background tasks (persistence, health monitor)

pub mod handlers {
    //! HTTP request handlers, one file per resource area.
    pub mod adapters; // GET /adapters, GET /capabilities
    pub mod admin; // Diagnostics, reload, API-key management
    pub mod automations; // CRUD + execute for automations
    pub mod devices; // Rooms, groups, devices, and commands
    pub mod events; // WebSocket event stream + IPC ingest endpoint
    pub mod health; // GET /health, GET /ready
    pub mod history; // Device/attribute/command history queries
    pub mod persons; // Person/zone CRUD + location ingest
    pub mod plugins; // Plugin list + Lua file read/write
    pub mod scenes; // Scene list and execute
}

#[cfg(test)]
mod tests;

// ── Re-exports (used by tests via `use super::*`) ────────────────────────────
// Tests live in `tests.rs` and import everything they need through `use super::*`.
// Any type or function that tests reference must be re-exported here.

// Helpers
pub use helpers::{reconcile_device_store, sha256_hex};

// State items
#[cfg(test)]
pub use state::make_state;
pub use state::{
    build_automation_runner, AdapterHealth, AppState, BuiltAdapters, HealthSnapshot, HealthState,
    HistorySettings, ReloadController, StoreAutomationObserver,
};

// Workers
pub use workers::{monitor_runtime_health, run_persistence_worker};

// Router
pub use router::{app, cors_layer, shutdown_signal};

// Startup functions
pub use startup::{
    build_adapters, config_path_from_args, create_device_store, history_selection_from_config,
    history_selection_from_telemetry, init_tracing, resolve_database_url,
    trigger_context_from_config, SHUTDOWN_DRAIN_SECS,
};

// Standard-library types used in tests
pub use std::collections::HashSet;
pub use std::path::PathBuf;
pub use std::sync::RwLock;
pub use std::time::{Duration, Instant};

// Third-party types used in tests
pub use anyhow::Result;
pub use chrono::{DateTime, Utc};
pub use serde_json::json;
pub use tokio::sync::watch;

// Automation types
pub use homecmdr_automations::{
    AutomationCatalog, AutomationController, AutomationExecutionObserver, AutomationRunner,
    TriggerContext,
};

// Core types
pub use homecmdr_core::adapter::Adapter;
pub use homecmdr_core::event::Event;
pub use homecmdr_core::runtime::Runtime;
pub use homecmdr_core::store::{
    ApiKeyRole, ApiKeyStore, AttributeHistoryEntry, AutomationExecutionHistoryEntry,
    CommandAuditEntry, DeviceHistoryEntry, DeviceStore, SceneExecutionHistoryEntry,
    SceneStepResult,
};
pub use store_sql::HistorySelection;

// ── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    startup::run().await
}
