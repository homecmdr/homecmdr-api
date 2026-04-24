//! Hot-reload logic for scenes, automations, and scripts.
//!
//! This module owns everything needed to re-read Lua assets from disk at
//! runtime without restarting the server.  It provides:
//!
//! - [`ReloadTarget`] — an enum naming the three reloadable asset types.
//! - Synchronous `reload_*_internal` functions that do the actual disk I/O
//!   and state swap.  They are designed to run inside
//!   `tokio::task::spawn_blocking` so they do not block the async runtime.
//! - [`spawn_reload_watchers_if_enabled`] — starts a background task that
//!   watches configured directories with `notify` and triggers automatic
//!   reloads when `.lua` files change.
//!
//! None of these functions restart the API server; they update the
//! in-memory catalogs held inside `AppState` and publish events onto the
//! runtime bus so interested subscribers (dashboards, automations, etc.)
//! can react.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use homecmdr_automations::AutomationCatalog;
use homecmdr_core::config::Config;
use homecmdr_core::event::Event;
use homecmdr_scenes::{SceneCatalog, SceneRunner};
use notify::{EventKind as NotifyEventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::dto::{ReloadErrorDetail, ReloadResponse};
use crate::helpers::{read_lock, write_lock};
use crate::state::{build_automation_runner, AppState, ReloadController};

/// Which category of Lua assets to reload.
///
/// Each variant maps to a separate directory on disk and a separate
/// in-memory catalog inside `AppState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReloadTarget {
    Scenes,
    Automations,
    Scripts,
}

/// Returned by a successful reload — how many files were loaded and how long
/// the operation took.
pub struct ReloadOutcome {
    pub loaded_count: usize,
    pub duration_ms: u128,
}

/// Returns the scripts root directory as a `PathBuf` if scripts are enabled,
/// or `None` if they are turned off in config.
///
/// Scene and automation catalogs accept an optional scripts root so that
/// `require(...)` calls inside scene/automation files can resolve shared
/// helper modules.
pub fn scripts_root_from_controller(controller: &ReloadController) -> Option<PathBuf> {
    controller
        .scripts_enabled
        .then(|| PathBuf::from(&controller.scripts_directory))
}

/// Builds a `ReloadController` snapshot from the live `AppState`.
///
/// `ReloadController` holds only the fields needed to perform a reload, so
/// it can be moved into `spawn_blocking` without dragging the entire
/// `AppState` (which is not `Send + 'static` in all contexts) along with it.
pub fn reload_controller_from_state(state: &AppState) -> ReloadController {
    ReloadController {
        scenes_enabled: state.scenes_enabled,
        automations_enabled: state.automations_enabled,
        scripts_enabled: state.scripts_enabled,
        scenes_directory: state.scenes_directory.clone(),
        automations_directory: state.automations_directory.clone(),
        scripts_directory: state.scripts_directory.clone(),
        scenes: state.scenes.clone(),
        automations: state.automations.clone(),
        automation_control: state.automation_control.clone(),
        automation_runner_tx: state.automation_runner_tx.clone(),
        automation_observer: state.automation_observer.clone(),
        store: state.store.clone(),
        trigger_context: state.trigger_context,
        runtime: state.runtime.clone(),
        backstop_timeout: state.backstop_timeout,
        person_registry: state.person_registry.clone(),
    }
}

/// Counts how many `.lua` files currently exist in the scripts directory.
///
/// Returns 0 if the directory cannot be read (e.g. it does not exist yet).
/// This is used to populate the `loaded_count` field of a scripts reload
/// response — scripts are not parsed at reload time, only counted.
pub fn scripts_loaded_count(controller: &ReloadController) -> usize {
    match std::fs::read_dir(&controller.scripts_directory) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.path().is_file()
                    && entry.path().extension().and_then(|ext| ext.to_str()) == Some("lua")
            })
            .count(),
        Err(_) => 0,
    }
}

/// Converts a reload result into the JSON-serialisable `ReloadResponse` DTO.
///
/// On success the response carries the loaded count and elapsed time.  On
/// failure it carries the list of per-file errors and zeroes out the numeric
/// fields so callers don't need to pattern-match.
pub fn reload_response_from_result(
    target: &'static str,
    result: std::result::Result<ReloadOutcome, Vec<ReloadErrorDetail>>,
) -> ReloadResponse {
    match result {
        Ok(outcome) => ReloadResponse {
            status: "ok",
            target,
            loaded_count: outcome.loaded_count,
            errors: Vec::new(),
            duration_ms: outcome.duration_ms,
        },
        Err(errors) => ReloadResponse {
            status: "error",
            target,
            loaded_count: 0,
            errors,
            duration_ms: 0,
        },
    }
}

/// Re-reads all scene files from disk and atomically swaps the in-memory
/// `SceneRunner`.
///
/// Returns an error if scenes are disabled in config, or if the catalog
/// fails to load (individual file parse errors are collected into the
/// error vec rather than aborting early).  Bus events are published
/// regardless of outcome so the dashboard can update its status indicator.
pub fn reload_scenes_internal(
    controller: &ReloadController,
) -> std::result::Result<ReloadOutcome, Vec<ReloadErrorDetail>> {
    if !controller.scenes_enabled {
        return Err(vec![ReloadErrorDetail {
            file: controller.scenes_directory.clone(),
            message: "scene reload is not supported when scenes are disabled".to_string(),
        }]);
    }

    let started = Instant::now();
    controller
        .runtime
        .bus()
        .publish(Event::SceneCatalogReloadStarted);
    match SceneCatalog::reload_from_directory(
        &controller.scenes_directory,
        scripts_root_from_controller(controller),
    ) {
        Ok(catalog) => {
            let loaded_count = catalog.summaries().len();
            let duration_ms = started.elapsed().as_millis() as u64;
            let mut runner = write_lock(&controller.scenes);
            *runner = SceneRunner::new(catalog);
            controller
                .runtime
                .bus()
                .publish(Event::SceneCatalogReloaded {
                    loaded_count,
                    duration_ms,
                });
            Ok(ReloadOutcome {
                loaded_count,
                duration_ms: duration_ms as u128,
            })
        }
        Err(errors) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            controller
                .runtime
                .bus()
                .publish(Event::SceneCatalogReloadFailed {
                    duration_ms,
                    errors: errors
                        .iter()
                        .map(|error| homecmdr_core::event::ReloadError {
                            file: error.file.clone(),
                            message: error.message.clone(),
                        })
                        .collect(),
                });
            Err(errors
                .into_iter()
                .map(|error| ReloadErrorDetail {
                    file: error.file,
                    message: error.message,
                })
                .collect())
        }
    }
}

/// Re-reads all automation files from disk and atomically swaps the in-memory
/// `AutomationCatalog` and its runner.
///
/// Enabled/disabled state is preserved across reloads: any automation that
/// existed before and still exists after the reload keeps its previous
/// enabled flag.  Automations that are new default to enabled.
///
/// After a successful reload the new runner is sent to the supervisor task
/// via `automation_runner_tx` so the currently-running runner is replaced
/// without restarting the server.
pub fn reload_automations_internal(
    controller: &ReloadController,
) -> std::result::Result<ReloadOutcome, Vec<ReloadErrorDetail>> {
    if !controller.automations_enabled {
        return Err(vec![ReloadErrorDetail {
            file: controller.automations_directory.clone(),
            message: "automation reload is not supported when automations are disabled".to_string(),
        }]);
    }

    let started = Instant::now();
    controller
        .runtime
        .bus()
        .publish(Event::AutomationCatalogReloadStarted);

    let previous_controller = {
        let guard = read_lock(&controller.automation_control);
        guard.clone()
    };
    let previous_enabled = previous_controller
        .summaries()
        .into_iter()
        .filter_map(|summary| {
            previous_controller
                .is_enabled(&summary.id)
                .map(|enabled| (summary.id, enabled))
        })
        .collect::<Vec<_>>();

    match AutomationCatalog::reload_from_directory(
        &controller.automations_directory,
        scripts_root_from_controller(controller),
    ) {
        Ok(catalog) => {
            for (id, enabled) in previous_enabled {
                if catalog.get(&id).is_some() {
                    let _ = catalog.set_enabled(&id, enabled);
                }
            }

            let loaded_count = catalog.summaries().len();
            let duration_ms = started.elapsed().as_millis() as u64;
            let runner = build_automation_runner(
                catalog.clone(),
                controller.automation_observer.clone(),
                controller.store.clone(),
                controller.trigger_context,
                controller.backstop_timeout,
                controller.person_registry.clone(),
            );
            let next_controller = Arc::new(runner.controller());

            {
                let mut catalog_guard = write_lock(&controller.automations);
                *catalog_guard = Arc::new(catalog);
            }
            {
                let mut controller_guard = write_lock(&controller.automation_control);
                *controller_guard = next_controller;
            }

            if controller.automation_runner_tx.send(runner).is_err() {
                tracing::warn!("automation reload completed but no active runner supervisor was available to receive the updated runner");
            }
            controller
                .runtime
                .bus()
                .publish(Event::AutomationCatalogReloaded {
                    loaded_count,
                    duration_ms,
                });

            Ok(ReloadOutcome {
                loaded_count,
                duration_ms: duration_ms as u128,
            })
        }
        Err(errors) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            controller
                .runtime
                .bus()
                .publish(Event::AutomationCatalogReloadFailed {
                    duration_ms,
                    errors: errors
                        .iter()
                        .map(|error| homecmdr_core::event::ReloadError {
                            file: error.file.clone(),
                            message: error.message.clone(),
                        })
                        .collect(),
                });
            Err(errors
                .into_iter()
                .map(|error| ReloadErrorDetail {
                    file: error.file,
                    message: error.message,
                })
                .collect())
        }
    }
}

/// Acknowledges a scripts directory reload.
///
/// Unlike scenes and automations, scripts are not pre-parsed into a catalog.
/// They are loaded on demand by `require(...)` inside scene/automation Lua
/// files.  This function therefore only verifies the directory is readable
/// and counts the `.lua` files — the actual re-loading happens transparently
/// the next time a scene or automation calls `require`.
pub fn reload_scripts_internal(
    controller: &ReloadController,
) -> std::result::Result<ReloadOutcome, Vec<ReloadErrorDetail>> {
    if !controller.scripts_enabled {
        return Err(vec![ReloadErrorDetail {
            file: controller.scripts_directory.clone(),
            message: "scripts reload is not supported when scripts are disabled".to_string(),
        }]);
    }

    let started = Instant::now();
    controller
        .runtime
        .bus()
        .publish(Event::ScriptsReloadStarted);
    if let Err(error) = std::fs::read_dir(&controller.scripts_directory) {
        let duration_ms = started.elapsed().as_millis() as u64;
        let errors = vec![ReloadErrorDetail {
            file: controller.scripts_directory.clone(),
            message: format!("failed to read scripts directory: {error}"),
        }];
        controller
            .runtime
            .bus()
            .publish(Event::ScriptsReloadFailed {
                duration_ms,
                errors: errors
                    .iter()
                    .map(|error| homecmdr_core::event::ReloadError {
                        file: error.file.clone(),
                        message: error.message.clone(),
                    })
                    .collect(),
            });
        return Err(errors);
    }

    let loaded_count = scripts_loaded_count(controller);
    let duration_ms = started.elapsed().as_millis() as u64;
    controller.runtime.bus().publish(Event::ScriptsReloaded {
        loaded_count,
        duration_ms,
    });
    Ok(ReloadOutcome {
        loaded_count,
        duration_ms: duration_ms as u128,
    })
}

/// Dispatches a reload for the given `target` on a blocking thread and
/// awaits its completion.
///
/// The actual reload functions do synchronous disk I/O, so they must run
/// outside the async executor via `spawn_blocking`.
pub async fn run_reload_target(controller: ReloadController, target: ReloadTarget) {
    let result = tokio::task::spawn_blocking(move || match target {
        ReloadTarget::Scenes => reload_scenes_internal(&controller),
        ReloadTarget::Automations => reload_automations_internal(&controller),
        ReloadTarget::Scripts => reload_scripts_internal(&controller),
    })
    .await;

    if let Err(error) = result {
        tracing::warn!("reload task join error: {error}");
    }
}

/// Starts a filesystem watcher task if any of the watch flags are enabled in
/// config.
///
/// The watcher listens for `Create`, `Modify`, and `Remove` events on
/// `.lua` files inside the configured directories.  It debounces rapid
/// bursts of events (e.g. an editor writing a file in multiple chunks) by
/// ignoring subsequent events for the same target within a 400 ms window.
///
/// Returns `None` if no watch targets are enabled (no background task is
/// started).  The caller should store the returned `JoinHandle` and abort
/// it on shutdown.
pub fn spawn_reload_watchers_if_enabled(
    config: &Config,
    controller: ReloadController,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.scenes.watch && !config.automations.watch && !config.scripts.watch {
        return None;
    }

    let mut watched = Vec::<(String, ReloadTarget)>::new();
    if config.scenes.watch {
        watched.push((config.scenes.directory.clone(), ReloadTarget::Scenes));
    }
    if config.automations.watch {
        watched.push((
            config.automations.directory.clone(),
            ReloadTarget::Automations,
        ));
    }
    if config.scripts.watch {
        watched.push((config.scripts.directory.clone(), ReloadTarget::Scripts));
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<ReloadTarget>();
    let watched_map = watched.clone();
    let mut watcher =
        match notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else {
                return;
            };
            let trigger = matches!(
                event.kind,
                NotifyEventKind::Create(_)
                    | NotifyEventKind::Modify(_)
                    | NotifyEventKind::Remove(_)
            );
            if !trigger {
                return;
            }
            for path in event.paths {
                if path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
                    continue;
                }
                for (dir, target) in &watched_map {
                    if path.starts_with(std::path::Path::new(dir)) {
                        let _ = tx.send(*target);
                        return;
                    }
                }
            }
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!("failed to start reload watcher: {error}");
                return None;
            }
        };

    for (dir, _) in &watched {
        if let Err(error) = watcher.watch(std::path::Path::new(dir), RecursiveMode::Recursive) {
            tracing::warn!("failed to watch directory '{}': {error}", dir);
        }
    }

    Some(tokio::spawn(async move {
        let _watcher = watcher;
        let mut last = HashMap::<ReloadTarget, Instant>::new();
        while let Some(target) = rx.recv().await {
            let now = Instant::now();
            if let Some(previous) = last.get(&target) {
                if now.duration_since(*previous) < std::time::Duration::from_millis(400) {
                    continue;
                }
            }
            last.insert(target, now);
            run_reload_target(controller.clone(), target).await;
        }
    }))
}
