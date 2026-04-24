//! Application state — the shared data that every request handler can read.
//!
//! `AppState` is cloned cheaply (all expensive things are behind `Arc`) and
//! handed to every axum handler via `State<AppState>`.  Think of it as the
//! "global variables" for the running server — but safe, because everything
//! that can change is protected by a lock.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use homecmdr_automations::{
    AutomationCatalog, AutomationController, AutomationExecutionObserver, AutomationRunner,
    TriggerContext,
};
use homecmdr_core::runtime::Runtime;
use homecmdr_core::store::{ApiKeyStore, AutomationExecutionHistoryEntry, DeviceStore, PersonStore};
use homecmdr_core::person_registry::PersonRegistry;
use homecmdr_plugin_host::PluginManifest;
use homecmdr_scenes::SceneRunner;
use tokio::sync::watch;

use crate::dto::{
    AdapterDetailResponse, AdapterErrorSnapshot, AdapterSummary, ApiError, ComponentStatus,
    HealthResponse,
};

/// Everything every HTTP handler needs — passed in through axum's `State<AppState>`.
///
/// Cloning this is cheap: the heavy fields (`Runtime`, stores, catalogs) are
/// all behind `Arc`, so a clone just increments a reference count.
#[derive(Clone)]
pub struct AppState {
    /// The device runtime: holds the registry of all known devices and the
    /// event bus that adapters publish to.
    pub runtime: Arc<Runtime>,

    /// The loaded set of scenes (Lua scripts that control devices).
    /// Wrapped in a lock so a reload can swap it out while requests are running.
    pub scenes: Arc<RwLock<SceneRunner>>,

    /// The loaded set of automations (event-driven Lua scripts).
    pub automations: Arc<RwLock<Arc<AutomationCatalog>>>,

    /// Controls which automations are enabled/disabled at runtime.
    pub automation_control: Arc<RwLock<Arc<AutomationController>>>,

    /// Used to hand a freshly-reloaded `AutomationRunner` to the supervisor
    /// task without restarting the whole server.
    pub automation_runner_tx: watch::Sender<AutomationRunner>,

    /// Optional observer that saves automation run results to the database.
    pub automation_observer: Option<Arc<dyn AutomationExecutionObserver>>,

    /// Location/timezone context used by time-based automation triggers
    /// (sunrise, sunset, wall-clock, cron).
    pub trigger_context: TriggerContext,

    /// Tracks the health of each component so `/health` and `/ready` can report it.
    pub health: HealthState,

    /// The device/room/group database (SQLite or PostgreSQL).  `None` when
    /// persistence is disabled in config.
    pub store: Option<Arc<dyn DeviceStore>>,

    /// The API-key database — usually the same backing store as `store`.
    /// `None` when persistence is disabled.
    pub auth_key_store: Option<Arc<dyn ApiKeyStore>>,

    /// The person/zone database — usually the same backing store.
    /// `None` when persistence is disabled.
    pub person_store: Option<Arc<dyn PersonStore>>,

    /// The person registry — manages persons, zones, and presence state.
    /// `None` when persistence is disabled.
    pub person_registry: Option<Arc<PersonRegistry>>,

    /// SHA-256 hash of the master key from config.  Compared against the hash
    /// of bearer tokens on every authenticated request.
    pub master_key_hash: String,

    /// Settings that control how much history is stored and queryable.
    pub history: HistorySettings,

    // ── Feature flags and directory paths from config ─────────────────────────
    pub scenes_enabled: bool,
    pub automations_enabled: bool,
    pub scenes_directory: String,
    pub automations_directory: String,
    /// Whether to watch the scenes directory for file changes and auto-reload.
    pub scenes_watch: bool,
    /// Whether to watch the automations directory for file changes and auto-reload.
    pub automations_watch: bool,
    pub scripts_watch: bool,
    pub scripts_enabled: bool,
    pub scripts_directory: String,
    /// Maximum time an automation is allowed to run before it is forcibly stopped.
    pub backstop_timeout: Duration,
    pub plugins_enabled: bool,
    pub plugins_directory: String,

    /// In-memory list of installed plugin manifests, updated by `POST /plugins/reload`.
    pub plugin_catalog: Arc<RwLock<Vec<PluginManifest>>>,

    /// Names of adapters whose devices are managed by an external IPC process.
    /// Commands to these devices are dispatched via the event bus rather than
    /// calling an in-process adapter factory.
    pub ipc_adapter_names: Arc<HashSet<String>>,
}

// Compile-time check that AppState can be shared safely across threads.
const _: fn() = || {
    fn assert_sync<T: Sync>() {}
    fn assert_send<T: Send>() {}
    assert_sync::<AppState>();
    assert_send::<AppState>();
};

/// Settings read from config that govern the history/audit feature.
#[derive(Clone)]
pub struct HistorySettings {
    /// `false` if persistence is disabled, or if history is explicitly turned off.
    pub enabled: bool,
    /// Number of entries returned when the caller doesn't specify a limit.
    pub default_limit: usize,
    /// Hard cap — callers cannot request more entries than this.
    pub max_limit: usize,
}

impl HistorySettings {
    pub fn from_config(config: &homecmdr_core::config::Config) -> Self {
        Self {
            enabled: config.persistence.enabled && config.persistence.history.enabled,
            default_limit: config.persistence.history.default_query_limit,
            max_limit: config.persistence.history.max_query_limit,
        }
    }

    /// Return the effective limit for a history query, or an error if the
    /// requested limit is out of range.
    pub fn resolve_limit(&self, requested: Option<usize>) -> Result<usize, ApiError> {
        use axum::http::StatusCode;
        let limit = requested.unwrap_or(self.default_limit);
        if limit == 0 {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "history query limit must be greater than zero",
            ));
        }
        if limit > self.max_limit {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("history query limit must be <= {}", self.max_limit),
            ));
        }

        Ok(limit)
    }
}

/// A slimmed-down version of `AppState` that contains only what is needed to
/// perform a reload (scenes, automations, or scripts).  Extracted from
/// `AppState` by `reload::reload_controller_from_state` so reload logic doesn't
/// need to drag in the whole state.
#[derive(Clone)]
pub struct ReloadController {
    pub scenes_enabled: bool,
    pub automations_enabled: bool,
    pub scripts_enabled: bool,
    pub scenes_directory: String,
    pub automations_directory: String,
    pub scripts_directory: String,
    pub scenes: Arc<RwLock<SceneRunner>>,
    pub automations: Arc<RwLock<Arc<AutomationCatalog>>>,
    pub automation_control: Arc<RwLock<Arc<AutomationController>>>,
    pub automation_runner_tx: watch::Sender<AutomationRunner>,
    pub automation_observer: Option<Arc<dyn AutomationExecutionObserver>>,
    pub store: Option<Arc<dyn DeviceStore>>,
    pub trigger_context: TriggerContext,
    pub runtime: Arc<Runtime>,
    pub backstop_timeout: Duration,
    pub person_registry: Option<Arc<PersonRegistry>>,
}

/// The return type of `build_adapters`: a list of adapter instances, their
/// names (for health tracking), and the set of names that belong to IPC adapters.
pub type BuiltAdapters = (
    Vec<Box<dyn homecmdr_core::adapter::Adapter>>,
    Vec<String>,
    HashSet<String>,
);

/// Thread-safe wrapper around a `HealthSnapshot`.
/// Handlers read from it; background workers write to it as things change.
#[derive(Clone)]
pub struct HealthState {
    pub inner: Arc<RwLock<HealthSnapshot>>,
}

/// Point-in-time view of every component's health.
#[derive(Clone)]
pub struct HealthSnapshot {
    /// `false` until startup has finished.  The `/ready` endpoint returns 503 until this is `true`.
    pub startup_complete: bool,
    pub runtime: ComponentStatus,
    pub persistence: ComponentStatus,
    pub automations: ComponentStatus,
    /// One entry per adapter, keyed by adapter name.
    pub adapters: HashMap<String, AdapterHealth>,
}

/// Health record for a single adapter.
#[derive(Clone)]
pub struct AdapterHealth {
    pub current: ComponentStatus,
    /// When the adapter last reported a successful poll.
    pub last_success: Option<DateTime<Utc>>,
    /// The most recent error, if any.
    pub last_error: Option<AdapterErrorSnapshot>,
}

impl AdapterHealth {
    pub fn starting(message: impl Into<String>) -> Self {
        Self {
            current: ComponentStatus::starting(message),
            last_success: None,
            last_error: None,
        }
    }

    pub fn ok() -> Self {
        let current = ComponentStatus::ok();
        Self {
            last_success: current.last_updated,
            current,
            last_error: None,
        }
    }
}

impl HealthState {
    /// Create a new `HealthState` with every component set to `"starting"`.
    pub fn new(
        adapter_names: &[String],
        persistence_enabled: bool,
        automations_enabled: bool,
    ) -> Self {
        let adapters = adapter_names
            .iter()
            .cloned()
            .map(|name| (name, AdapterHealth::starting("waiting for adapter start")))
            .collect();

        Self {
            inner: Arc::new(RwLock::new(HealthSnapshot {
                startup_complete: false,
                runtime: ComponentStatus::starting("starting runtime"),
                persistence: if persistence_enabled {
                    ComponentStatus::starting("starting persistence worker")
                } else {
                    ComponentStatus::ok()
                },
                automations: if automations_enabled {
                    ComponentStatus::starting("starting automation runner")
                } else {
                    ComponentStatus::ok()
                },
                adapters,
            })),
        }
    }

    /// Called once when startup has finished.  Sets runtime to `"ok"` and
    /// flips the flag that makes `/ready` return 200.
    pub fn mark_startup_complete(&self) {
        let mut snapshot = self.write();
        snapshot.startup_complete = true;
        snapshot.runtime.set_ok();
    }

    pub fn adapter_started(&self, adapter: &str) {
        let mut snapshot = self.write();
        let status = snapshot
            .adapters
            .entry(adapter.to_string())
            .or_insert_with(AdapterHealth::ok);
        status.current.set_ok();
        status.last_success = status.current.last_updated;
    }

    pub fn adapter_error(&self, adapter: &str, message: impl Into<String>) {
        let mut snapshot = self.write();
        let message = message.into();
        let status = snapshot
            .adapters
            .entry(adapter.to_string())
            .or_insert_with(|| AdapterHealth::starting("adapter not started"));
        status.current.set_error(message.clone());
        status.last_error = status
            .current
            .last_updated
            .map(|observed_at| AdapterErrorSnapshot {
                message,
                observed_at,
            });
    }

    pub fn runtime_error(&self, message: impl Into<String>) {
        self.write().runtime.set_error(message);
    }

    pub fn persistence_ok(&self) {
        self.write().persistence.set_ok();
    }

    pub fn persistence_error(&self, message: impl Into<String>) {
        self.write().persistence.set_error(message);
    }

    pub fn automations_ok(&self) {
        self.write().automations.set_ok();
    }

    pub fn automations_error(&self, message: impl Into<String>) {
        self.write().automations.set_error(message);
    }

    /// Build the JSON-friendly `HealthResponse` that `/health` returns.
    pub fn response(&self) -> HealthResponse {
        let snapshot = self.read();
        let mut adapters = snapshot
            .adapters
            .iter()
            .map(|(name, status)| AdapterSummary {
                name: name.clone(),
                status: status.current.status.clone(),
                message: status.current.message.clone(),
                last_updated: status.current.last_updated,
                last_success: status.last_success,
                last_error: status.last_error.clone(),
            })
            .collect::<Vec<_>>();
        adapters.sort_by(|a, b| a.name.cmp(&b.name));

        // The system is fully ready only when startup has finished AND every
        // individual component reports "ok".
        let ready = snapshot.startup_complete
            && snapshot.runtime.status == "ok"
            && snapshot.persistence.status == "ok"
            && snapshot.automations.status == "ok"
            && snapshot
                .adapters
                .values()
                .all(|status| status.current.status == "ok");
        let overall_status = if ready { "ok" } else { "degraded" };

        HealthResponse {
            status: overall_status.to_string(),
            ready,
            runtime: snapshot.runtime.clone(),
            persistence: snapshot.persistence.clone(),
            automations: snapshot.automations.clone(),
            adapters,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.response().ready
    }

    pub fn adapter_detail(&self, adapter: &str) -> Option<AdapterDetailResponse> {
        let snapshot = self.read();
        snapshot
            .adapters
            .get(adapter)
            .map(|status| AdapterDetailResponse {
                name: adapter.to_string(),
                runtime_status: status.current.status.clone(),
                health: status.current.clone(),
                last_success: status.last_success,
                last_error: status.last_error.clone(),
            })
    }

    // Convenience wrappers that recover from a poisoned lock (e.g. if a thread
    // panicked while holding it) rather than propagating the panic.
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, HealthSnapshot> {
        match self.inner.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, HealthSnapshot> {
        match self.inner.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// An `AutomationExecutionObserver` that writes execution results to the database.
/// Wired in at startup when both persistence and history are enabled.
#[derive(Clone)]
pub struct StoreAutomationObserver {
    pub store: Arc<dyn DeviceStore>,
}

impl AutomationExecutionObserver for StoreAutomationObserver {
    fn record(&self, entry: AutomationExecutionHistoryEntry) {
        // Fire-and-forget: spawn a background task so the automation runner
        // is never blocked waiting for a database write.
        let store = self.store.clone();
        tokio::spawn(async move {
            if let Err(error) = store.save_automation_execution(&entry).await {
                tracing::error!(error = %error, automation_id = %entry.automation_id, "failed to persist automation execution history");
            }
        });
    }
}

/// Build an `AutomationRunner` from a catalog, wiring in an optional observer
/// and an optional state store.  Used at startup and after every reload.
pub fn build_automation_runner(
    catalog: AutomationCatalog,
    observer: Option<Arc<dyn AutomationExecutionObserver>>,
    store: Option<Arc<dyn DeviceStore>>,
    trigger_context: TriggerContext,
    backstop_timeout: Duration,
    person_registry: Option<Arc<PersonRegistry>>,
) -> AutomationRunner {
    let runner = if let Some(observer) = observer {
        AutomationRunner::new(catalog)
            .with_observer(observer)
            .with_trigger_context(trigger_context)
            .with_backstop_timeout(backstop_timeout)
    } else {
        AutomationRunner::new(catalog)
            .with_trigger_context(trigger_context)
            .with_backstop_timeout(backstop_timeout)
    };

    let runner = if let Some(store) = store {
        runner.with_state_store(store)
    } else {
        runner
    };

    if let Some(registry) = person_registry {
        runner.with_person_registry(registry)
    } else {
        runner
    }
}

/// Test-only helper that constructs a minimal `AppState` without needing the
/// full startup sequence.  This keeps test setup concise.
#[cfg(test)]
pub fn make_state(
    runtime: Arc<Runtime>,
    scenes: SceneRunner,
    automations: Arc<AutomationCatalog>,
    trigger_context: TriggerContext,
    health: HealthState,
    store: Option<Arc<dyn DeviceStore>>,
    auth_key_store: Option<Arc<dyn ApiKeyStore>>,
    master_key_hash: String,
    history: HistorySettings,
    scenes_enabled: bool,
    automations_enabled: bool,
    scenes_directory: String,
    automations_directory: String,
    scripts_root: Option<PathBuf>,
) -> AppState {
    let observer = if history.enabled {
        store.clone().map(|store| {
            Arc::new(StoreAutomationObserver { store }) as Arc<dyn AutomationExecutionObserver>
        })
    } else {
        None
    };
    let runner = build_automation_runner(
        (*automations).clone(),
        observer.clone(),
        store.clone(),
        trigger_context,
        Duration::from_secs(3600),
        None,
    );
    let control = Arc::new(runner.controller());
    let (automation_runner_tx, _automation_runner_rx) = watch::channel(runner);

    AppState {
        runtime,
        scenes: Arc::new(RwLock::new(scenes)),
        automations: Arc::new(RwLock::new(automations)),
        automation_control: Arc::new(RwLock::new(control)),
        automation_runner_tx,
        automation_observer: observer,
        trigger_context,
        health,
        store,
        auth_key_store,
        person_store: None,
        person_registry: None,
        master_key_hash,
        history,
        scenes_enabled,
        automations_enabled,
        scenes_directory,
        automations_directory,
        scenes_watch: false,
        automations_watch: false,
        scripts_watch: false,
        scripts_enabled: scripts_root.is_some(),
        scripts_directory: scripts_root
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default(),
        backstop_timeout: Duration::from_secs(3600),
        plugins_enabled: false,
        plugins_directory: String::new(),
        plugin_catalog: Arc::new(RwLock::new(Vec::new())),
        ipc_adapter_names: Arc::new(HashSet::new()),
    }
}
