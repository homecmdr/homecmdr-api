//! Application entry point — wires all subsystems together and runs the
//! HTTP server.
//!
//! The [`run`] function is the single top-level async function called from
//! `main`.  It:
//!
//! 1. Loads config from disk (path from args or `HOMECMDR_CONFIG`).
//! 2. Opens the persistence store (SQLite or Postgres).
//! 3. Builds adapters from the plugin directory.
//! 4. Loads the initial scene/automation catalogs from disk.
//! 5. Restores previously persisted device state into the in-memory registry.
//! 6. Constructs `AppState` and hands it to the router.
//! 7. Spawns background tasks (persistence, history pruning, runtime health
//!    monitoring, automation runner, file watchers, IPC adapter processes).
//! 8. Runs the HTTP server until a shutdown signal is received, then drains
//!    in-flight requests and stops all background tasks cleanly.
//!
//! Helper functions at the bottom of this file handle individual pieces of
//! startup that are also called from integration tests.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use homecmdr_automations::{AutomationCatalog, AutomationExecutionObserver, TriggerContext};
use homecmdr_core::adapter::{Adapter, AdapterFactory};
use homecmdr_core::config::{Config, PersistenceBackend, TelemetrySelectionConfig};
use homecmdr_core::person_registry::PersonRegistry;
use homecmdr_core::runtime::Runtime;
use homecmdr_core::store::{ApiKeyStore, DeviceStore, PersonStore};
use homecmdr_plugin_host::{
    create_engine, IpcAdapterEntry, IpcAdapterHost, PluginManager, PluginManifest,
    WasmAdapterFactory,
};
use homecmdr_scenes::{SceneCatalog, SceneRunner};
use store_postgres::{PgHistorySelection, PostgresDeviceStore, PostgresHistoryConfig};
use store_sql::{HistorySelection, SqliteDeviceStore, SqliteHistoryConfig};
use tokio::sync::watch;
use tracing::Level;

use crate::helpers::sha256_hex;
use crate::reload::spawn_reload_watchers_if_enabled;
use crate::router::{app, shutdown_signal};
use crate::state::{
    build_automation_runner, AppState, BuiltAdapters, HealthState, HistorySettings,
    ReloadController, StoreAutomationObserver,
};
use crate::workers::{monitor_runtime_health, run_persistence_worker};

/// How long (seconds) to wait for in-flight requests to finish after a
/// shutdown signal before forcibly exiting.
pub const SHUTDOWN_DRAIN_SECS: u64 = 30;

/// Top-level async entry point.  Loads config, wires every subsystem, starts
/// the HTTP server, and waits for a shutdown signal.
pub async fn run() -> Result<()> {
    let config_path = config_path_from_args()?;
    let config = Config::load_from_file(&config_path)
        .with_context(|| format!("failed to load configuration from {config_path}"))?;

    init_tracing(&config.logging.level)?;

    let (device_store, auth_key_store, person_store) = create_device_store(&config)
        .await
        .context("failed to create persistence store")?;

    // Allow HOMECMDR_MASTER_KEY env var to override config value at runtime.
    let master_key = std::env::var("HOMECMDR_MASTER_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| config.auth.master_key.clone());
    let master_key_hash = sha256_hex(&master_key);

    let (adapters, adapter_summaries, ipc_adapter_names) =
        build_adapters(&config).context("failed to build adapters")?;
    let health = HealthState::new(
        &adapter_summaries,
        device_store.is_some(),
        config.automations.enabled,
    );

    let runtime = Arc::new(Runtime::new(adapters, config.runtime));
    let scripts_root = config
        .scripts
        .enabled
        .then(|| std::path::PathBuf::from(&config.scripts.directory));
    let scenes = if config.scenes.enabled {
        SceneRunner::new(
            SceneCatalog::load_from_directory(&config.scenes.directory, scripts_root.clone())
                .with_context(|| {
                    format!("failed to load scenes from {}", config.scenes.directory)
                })?,
        )
    } else {
        SceneRunner::new(SceneCatalog::empty())
    };
    let automations = if config.automations.enabled {
        Arc::new(
            AutomationCatalog::load_from_directory(&config.automations.directory, scripts_root)
                .with_context(|| {
                    format!(
                        "failed to load automations from {}",
                        config.automations.directory
                    )
                })?,
        )
    } else {
        Arc::new(AutomationCatalog::empty())
    };

    if let Some(store) = &device_store {
        let rooms = store
            .load_all_rooms()
            .await
            .context("failed to load persisted rooms")?;
        let groups = store
            .load_all_groups()
            .await
            .context("failed to load persisted groups")?;
        let devices = store
            .load_all_devices()
            .await
            .context("failed to load persisted devices")?;
        runtime.registry().restore_rooms(rooms);
        runtime
            .registry()
            .restore(devices)
            .context("failed to restore persisted devices into registry")?;
        runtime.registry().restore_groups(groups);
    }

    // Build the person registry if persistence is enabled.
    let person_registry = if let Some(ref ps) = person_store {
        let locale = &config.locale;
        match PersonRegistry::new(ps.clone(), runtime.bus().clone(), locale).await {
            Ok(reg) => {
                tracing::info!("PersonRegistry initialised");
                Some(Arc::new(reg))
            }
            Err(e) => {
                tracing::error!("failed to initialise PersonRegistry: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let automation_observer =
        device_store.clone().and_then(|store| {
            if config.persistence.history.enabled {
                Some(Arc::new(StoreAutomationObserver { store })
                    as Arc<dyn AutomationExecutionObserver>)
            } else {
                None
            }
        });
    let trigger_context = trigger_context_from_config(&config);
    let backstop_timeout = Duration::from_secs(config.automations.runner.backstop_timeout_secs);
    let automation_runner = build_automation_runner(
        (*automations).clone(),
        automation_observer.clone(),
        device_store.clone(),
        trigger_context,
        backstop_timeout,
        person_registry.clone(),
    );
    let automation_control = Arc::new(automation_runner.controller());
    let (automation_runner_tx, automation_runner_rx) = watch::channel(automation_runner.clone());

    // Scan the plugin directory once at startup to populate the catalog shown by GET /plugins.
    let initial_plugin_manifests = if config.plugins.enabled {
        let plugin_dir = std::path::Path::new(&config.plugins.directory);
        if plugin_dir.exists() {
            PluginManager::scan_manifests(plugin_dir)
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let app_state = AppState {
        runtime: runtime.clone(),
        scenes: Arc::new(RwLock::new(scenes)),
        automations: Arc::new(RwLock::new(automations.clone())),
        automation_control: Arc::new(RwLock::new(automation_control.clone())),
        automation_runner_tx,
        automation_observer: automation_observer.clone(),
        trigger_context,
        health: health.clone(),
        store: device_store.clone(),
        auth_key_store,
        person_store: person_store.clone(),
        person_registry,
        master_key_hash,
        history: HistorySettings::from_config(&config),
        scenes_enabled: config.scenes.enabled,
        automations_enabled: config.automations.enabled,
        scenes_directory: config.scenes.directory.clone(),
        automations_directory: config.automations.directory.clone(),
        scenes_watch: config.scenes.watch,
        automations_watch: config.automations.watch,
        scripts_watch: config.scripts.watch,
        scripts_enabled: config.scripts.enabled,
        scripts_directory: config.scripts.directory.clone(),
        backstop_timeout,
        plugins_enabled: config.plugins.enabled,
        plugins_directory: config.plugins.directory.clone(),
        plugin_catalog: Arc::new(RwLock::new(initial_plugin_manifests)),
        ipc_adapter_names: Arc::new(ipc_adapter_names),
    };
    let app = app(app_state.clone(), &config);
    let reload_controller = ReloadController {
        scenes_enabled: config.scenes.enabled,
        automations_enabled: config.automations.enabled,
        scripts_enabled: config.scripts.enabled,
        scenes_directory: config.scenes.directory.clone(),
        automations_directory: config.automations.directory.clone(),
        scripts_directory: config.scripts.directory.clone(),
        scenes: app_state.scenes.clone(),
        automations: app_state.automations.clone(),
        automation_control: app_state.automation_control.clone(),
        automation_runner_tx: app_state.automation_runner_tx.clone(),
        automation_observer: app_state.automation_observer.clone(),
        store: app_state.store.clone(),
        trigger_context,
        runtime: runtime.clone(),
        backstop_timeout,
        person_registry: app_state.person_registry.clone(),
    };
    let listener = tokio::net::TcpListener::bind(&config.api.bind_address)
        .await
        .with_context(|| format!("failed to bind API listener on {}", config.api.bind_address))?;

    let (persistence_shutdown_tx, persistence_shutdown_rx) = tokio::sync::oneshot::channel();
    let mut persistence_shutdown_tx = Some(persistence_shutdown_tx);
    let persistence_task = device_store.clone().map(|store| {
        let runtime = runtime.clone();
        let health = health.clone();
        tokio::spawn(async move {
            run_persistence_worker(runtime, store, health, persistence_shutdown_rx).await;
        })
    });

    // Periodically prune old history entries according to the configured retention policy.
    // This runs every hour regardless of load; the store implementation
    // decides which rows to delete based on `retention_days`.
    let prune_task = device_store.clone().map(|store| {
        let person_store_prune = person_store.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                if let Err(e) = store.prune_history().await {
                    tracing::warn!("history pruning failed: {e:#}");
                }
                if let Some(ref ps) = person_store_prune {
                    if let Err(e) = ps.prune_person_history().await {
                        tracing::warn!("person history pruning failed: {e:#}");
                    }
                }
            }
        })
    });

    let runtime_task = {
        let runtime = runtime.clone();
        let health = health.clone();
        tokio::spawn(async move {
            monitor_runtime_health(runtime, health).await;
        })
    };
    let automation_task = {
        let runtime = runtime.clone();
        let health = health.clone();
        let mut runner_rx = automation_runner_rx;
        tokio::spawn(async move {
            health.automations_ok();
            // Fuse the JoinHandle so select! doesn't panic with "JoinHandle
            // polled after completion" when both branches are ready simultaneously.
            // AbortHandle is kept separately since Fuse<JoinHandle> doesn't
            // expose abort().
            use futures_util::future::FutureExt as _;
            let initial_handle = {
                let runtime = runtime.clone();
                let initial = runner_rx.borrow().clone();
                tokio::spawn(async move {
                    initial.run(runtime).await;
                })
            };
            let mut abort_handle = initial_handle.abort_handle();
            let mut active_task = initial_handle.fuse();

            loop {
                tokio::select! {
                    _ = &mut active_task => {
                        health.automations_error("automation runner exited unexpectedly");
                        break;
                    }
                    changed = runner_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        abort_handle.abort();
                        let _ = (&mut active_task).await;

                        let runtime = runtime.clone();
                        let runner = runner_rx.borrow().clone();
                        let new_handle = tokio::spawn(async move {
                            runner.run(runtime).await;
                        });
                        abort_handle = new_handle.abort_handle();
                        active_task = new_handle.fuse();
                    }
                }
            }

            abort_handle.abort();
            let _ = active_task.await;
        })
    };

    let watcher_task = spawn_reload_watchers_if_enabled(&config, reload_controller.clone());

    // ── Spawn IPC adapter child processes ────────────────────────────────────
    // Derive a loopback API URL from the configured bind address so child
    // processes can reach the API regardless of whether the bind address is
    // 0.0.0.0 or 127.0.0.1.  Each IPC adapter process receives the master
    // key as its bearer token so it can call `/ingest/devices`.
    let ipc_host = if config.plugins.enabled && !app_state.ipc_adapter_names.is_empty() {
        let plugin_dir = std::path::Path::new(&config.plugins.directory);
        let port = config.api.bind_address.rsplit(':').next().unwrap_or("3000");
        let api_url = format!("http://127.0.0.1:{port}");

        let entries: Vec<IpcAdapterEntry> = PluginManager::scan_manifests(plugin_dir)
            .into_iter()
            .filter(|m| m.is_ipc() && config.adapters.contains_key(&m.plugin.name))
            .filter_map(|manifest| {
                // Find the manifest file path so we can derive the binary path.
                let manifest_path =
                    plugin_dir.join(format!("{}.plugin.toml", manifest.plugin.name));
                let binary_path = PluginManifest::binary_path_for(&manifest_path, &manifest);
                if !binary_path.exists() {
                    tracing::warn!(
                        adapter = %manifest.plugin.name,
                        "IPC adapter binary not found at '{}'; skipping",
                        binary_path.display()
                    );
                    return None;
                }
                let config_json = config
                    .adapters
                    .get(&manifest.plugin.name)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                Some(IpcAdapterEntry {
                    name: manifest.plugin.name.clone(),
                    binary_path,
                    config_json,
                    api_url: api_url.clone(),
                    api_token: master_key.clone(),
                })
            })
            .collect();

        let mut host = IpcAdapterHost::new(entries);
        host.spawn_all();
        Some(host)
    } else {
        None
    };

    health.mark_startup_complete();

    // Spawn the HTTP server so we can race it against a drain timeout after the
    // shutdown signal fires.
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = drain_rx.await;
            })
            .await
    });

    shutdown_signal().await;
    tracing::info!(
        "shutdown signal received, draining in-flight requests ({}s timeout)...",
        SHUTDOWN_DRAIN_SECS
    );
    let _ = drain_tx.send(());

    match tokio::time::timeout(Duration::from_secs(SHUTDOWN_DRAIN_SECS), server_task).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => return Err(anyhow::Error::from(e)).context("API server failed"),
        Ok(Err(join_err)) => tracing::error!("server task panicked: {join_err}"),
        Err(_elapsed) => tracing::warn!(
            "graceful drain timed out after {}s, forcing exit",
            SHUTDOWN_DRAIN_SECS
        ),
    }

    runtime_task.abort();
    let _ = runtime_task.await;

    automation_task.abort();
    let _ = automation_task.await;

    if let Some(host) = ipc_host {
        host.shutdown();
    }

    if let Some(task) = watcher_task {
        task.abort();
        let _ = task.await;
    }

    if let Some(task) = prune_task {
        task.abort();
        let _ = task.await;
    }

    if let Some(task) = persistence_task {
        if let Some(shutdown) = persistence_shutdown_tx.take() {
            let _ = shutdown.send(());
        }
        let _ = task.await;
    }

    Ok(())
}

/// Scans the plugin directory and instantiates all configured adapters.
///
/// Returns a tuple of:
/// - the list of instantiated `Adapter` trait objects (WASM adapters only —
///   IPC adapters are launched as child processes later in `run`)
/// - the names of all successfully registered adapters (both WASM and IPC)
/// - the set of adapter names that use the IPC transport
///
/// Errors if the WASM engine fails to initialise, a plugin directory scan
/// fails, two plugins claim the same adapter name, or a config entry names
/// an adapter that has no registered factory.
pub fn build_adapters(config: &Config) -> Result<BuiltAdapters> {
    // ── 1. WASM (runtime) factories ──────────────────────────────────────────
    let mut wasm_factories: std::collections::HashMap<&'static str, WasmAdapterFactory> =
        std::collections::HashMap::new();
    let mut ipc_adapter_names: HashSet<String> = HashSet::new();

    if config.plugins.enabled {
        let engine = create_engine().context("failed to initialise WASM engine")?;
        let plugin_dir = std::path::Path::new(&config.plugins.directory);

        if plugin_dir.exists() {
            // Collect all manifests once, partition into WASM vs IPC.
            let all_manifests = PluginManager::scan_manifests(plugin_dir);
            for manifest in all_manifests {
                if manifest.is_ipc() {
                    ipc_adapter_names.insert(manifest.plugin.name.clone());
                }
            }

            let scanned = PluginManager::scan(plugin_dir, &engine).with_context(|| {
                format!("failed to scan plugin directory '{}'", plugin_dir.display())
            })?;

            for factory in scanned {
                let name = factory.name();
                if wasm_factories.insert(name, factory).is_some() {
                    anyhow::bail!("duplicate WASM plugin registration for '{}'", name);
                }
            }
        } else {
            tracing::info!(
                directory = %plugin_dir.display(),
                "plugin directory does not exist; skipping WASM plugin scan"
            );
        }
    }

    // ── 2. Instantiate adapters listed in config ─────────────────────────────
    let mut adapters: Vec<Box<dyn Adapter>> = Vec::new();
    let mut summaries = Vec::new();

    for (name, adapter_config) in &config.adapters {
        if let Some(factory) = wasm_factories.get(name.as_str()) {
            let adapter_opt = factory
                .build(adapter_config.clone())
                .with_context(|| format!("failed to build WASM adapter '{name}'"))?;
            if let Some(adapter) = adapter_opt {
                summaries.push(name.clone());
                adapters.push(adapter);
            }
        } else if ipc_adapter_names.contains(name) {
            tracing::info!(
                adapter = %name,
                "IPC adapter '{}' registered — managed as external process",
                name
            );
            summaries.push(name.clone());
        } else {
            anyhow::bail!("no adapter factory registered for '{name}'");
        }
    }

    Ok((adapters, summaries, ipc_adapter_names))
}

/// Rewrites a relative `sqlite://` path using `HOMECMDR_DATA_DIR` when set.
///
/// If the path component of the URL is already absolute (starts with `/`) or the
/// env var is not set, the URL is returned unchanged.  When the env var is set
/// the parent directory tree is created so sqlx can open the database file
/// without requiring it to pre-exist.
pub fn resolve_database_url(url: &str, auto_create: bool) -> Result<String> {
    let data_dir = std::env::var("HOMECMDR_DATA_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let resolved = if let Some(ref data_dir) = data_dir {
        let path_part = url.strip_prefix("sqlite://").unwrap_or(url);
        if path_part.starts_with('/') {
            // Already absolute — nothing to do.
            url.to_string()
        } else {
            let full = std::path::Path::new(data_dir).join(path_part);
            format!("sqlite://{}", full.display())
        }
    } else {
        url.to_string()
    };

    // Ensure the parent directory exists when auto_create is requested.
    if auto_create {
        let path_part = resolved.strip_prefix("sqlite://").unwrap_or(&resolved);
        if let Some(parent) = std::path::Path::new(path_part).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create data directory '{}'", parent.display())
                })?;
            }
        }
    }

    Ok(resolved)
}

/// Opens the persistence store (SQLite or Postgres) and returns it as a
/// triple of trait objects: one for device data, one for API key management,
/// and one for person/zone data.
///
/// All three trait objects point at the same underlying store implementation;
/// they are split by trait so callers only get the surface they need.
///
/// Returns `(None, None, None)` when persistence is disabled in config.
pub async fn create_device_store(
    config: &Config,
) -> Result<(
    Option<Arc<dyn DeviceStore>>,
    Option<Arc<dyn ApiKeyStore>>,
    Option<Arc<dyn PersonStore>>,
)> {
    if !config.persistence.enabled {
        return Ok((None, None, None));
    }

    let database_url = config
        .persistence
        .database_url
        .as_deref()
        .context("persistence.database_url is required when persistence is enabled")?;

    let database_url = resolve_database_url(database_url, config.persistence.auto_create)
        .context("failed to resolve database URL")?;

    match config.persistence.backend {
        PersistenceBackend::Sqlite => {
            let store: Arc<SqliteDeviceStore> = Arc::new(
                SqliteDeviceStore::new_with_history(
                    &database_url,
                    config.persistence.auto_create,
                    SqliteHistoryConfig {
                        enabled: config.persistence.history.enabled,
                        retention: config
                            .persistence
                            .history
                            .retention_days
                            .map(|days| Duration::from_secs(days.saturating_mul(24 * 60 * 60))),
                        selection: history_selection_from_config(config),
                    },
                )
                .await
                .with_context(|| format!("failed to initialize SQLite store '{database_url}'"))?,
            );
            Ok((
                Some(store.clone() as Arc<dyn DeviceStore>),
                Some(store.clone() as Arc<dyn ApiKeyStore>),
                Some(store as Arc<dyn PersonStore>),
            ))
        }
        PersistenceBackend::Postgres => {
            let selection = if config.telemetry.enabled {
                PgHistorySelection {
                    device_ids: config.telemetry.selection.device_ids.clone(),
                    capabilities: config.telemetry.selection.capabilities.clone(),
                    adapter_names: config.telemetry.selection.adapter_names.clone(),
                }
            } else {
                PgHistorySelection::default()
            };
            let history_config = PostgresHistoryConfig {
                enabled: config.persistence.history.enabled,
                retention: config
                    .persistence
                    .history
                    .retention_days
                    .map(|days| Duration::from_secs(days.saturating_mul(24 * 60 * 60))),
                selection,
            };
            let store: Arc<PostgresDeviceStore> = Arc::new(
                PostgresDeviceStore::new_with_history(
                    &database_url,
                    config.persistence.auto_create,
                    history_config,
                )
                .await
                .with_context(|| {
                    format!("failed to initialize PostgreSQL store '{database_url}'")
                })?,
            );
            Ok((
                Some(store.clone() as Arc<dyn DeviceStore>),
                Some(store.clone() as Arc<dyn ApiKeyStore>),
                Some(store as Arc<dyn PersonStore>),
            ))
        }
    }
}

/// Translates the telemetry selection config into the SQLite store's own
/// `HistorySelection` type.
///
/// Returns a default (record-everything) selection when telemetry is disabled.
pub fn history_selection_from_config(config: &Config) -> HistorySelection {
    if !config.telemetry.enabled {
        return HistorySelection::default();
    }

    history_selection_from_telemetry(&config.telemetry.selection)
}

/// Maps the generic telemetry selection config fields into the SQLite-specific
/// `HistorySelection` struct.
pub fn history_selection_from_telemetry(selection: &TelemetrySelectionConfig) -> HistorySelection {
    HistorySelection {
        device_ids: selection.device_ids.clone(),
        capabilities: selection.capabilities.clone(),
        adapter_names: selection.adapter_names.clone(),
    }
}

/// Builds a `TriggerContext` from the locale section of config.
///
/// `TriggerContext` is passed to the automation runner so that time-based
/// triggers (sunrise, sunset, wall-clock) know the user's geographic
/// coordinates and timezone.
pub fn trigger_context_from_config(config: &Config) -> TriggerContext {
    TriggerContext {
        latitude: config.locale.latitude,
        longitude: config.locale.longitude,
        timezone: config.locale.timezone.parse().ok(),
    }
}

/// Determines the config file path to load.
///
/// Priority order:
/// 1. `HOMECMDR_CONFIG` environment variable (if set and non-empty)
/// 2. `--config <path>` command-line flag
/// 3. Default: `config/default.toml`
///
/// Returns an error if the args are malformed (e.g. `--config` with no
/// following value).
pub fn config_path_from_args() -> Result<String> {
    // Environment variable takes priority over the command-line flag.
    if let Ok(path) = std::env::var("HOMECMDR_CONFIG") {
        if !path.trim().is_empty() {
            return Ok(path);
        }
    }

    let mut args = std::env::args().skip(1);

    match (args.next().as_deref(), args.next()) {
        (Some("--config"), Some(path)) => Ok(path),
        (None, _) => Ok("config/default.toml".to_string()),
        _ => Err(anyhow::anyhow!("usage: api [--config <path>]")),
    }
}

/// Initialises the global `tracing` subscriber with the configured log level.
///
/// Valid level strings: `trace`, `debug`, `info`, `warn`, `error`.
/// Returns an error for any other value so the operator gets a clear message
/// at startup rather than silent default behaviour.
pub fn init_tracing(level: &str) -> Result<()> {
    let level = match level {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        other => return Err(anyhow::anyhow!("invalid logging.level '{other}'")),
    };

    tracing_subscriber::fmt().with_max_level(level).init();
    Ok(())
}
