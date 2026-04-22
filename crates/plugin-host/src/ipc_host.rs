/// Manages IPC adapter child processes.
///
/// Each IPC adapter is a native binary that lives in the plugins directory
/// alongside its `.plugin.toml` manifest.  At startup, `IpcAdapterHost` spawns
/// one child process per registered IPC adapter, passing the HomeCmdr API URL,
/// a bearer token, and the adapter's config block as environment variables:
///
/// - `HOMECMDR_API_URL`       — base URL of the running API (e.g. `http://127.0.0.1:3000`)
/// - `HOMECMDR_API_TOKEN`     — bearer token (master key) used for `/ingest/devices` and `/events`
/// - `HOMECMDR_ADAPTER_CONFIG` — JSON-encoded adapter config from `config/default.toml`
///
/// If a child process exits it is automatically restarted with exponential
/// back-off (starting at 1 s, doubling up to 60 s).  When `shutdown()` is
/// called all children are killed immediately.
use std::path::PathBuf;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::watch;

/// One registered IPC adapter.
#[derive(Debug, Clone)]
pub struct IpcAdapterEntry {
    /// The adapter name (must match the key in `[adapters]` config).
    pub name: String,
    /// Absolute path to the adapter binary.
    pub binary_path: PathBuf,
    /// JSON-encoded adapter-specific config (the value of `config.adapters[name]`).
    pub config_json: String,
    /// Base URL the child uses to reach the HomeCmdr API.
    pub api_url: String,
    /// Bearer token the child uses (master key).
    pub api_token: String,
}

/// Manages spawned IPC adapter child processes.
pub struct IpcAdapterHost {
    entries: Vec<IpcAdapterEntry>,
    /// Shutdown sender — dropping or sending signals all restart loops to stop.
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl IpcAdapterHost {
    pub fn new(entries: Vec<IpcAdapterEntry>) -> Self {
        Self {
            entries,
            shutdown_tx: None,
        }
    }

    /// Returns true when there are no IPC adapters to manage.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Spawn all IPC adapter processes.  Each adapter gets its own restart
    /// loop in a detached `tokio::spawn` task.  Call `shutdown()` to stop them.
    pub fn spawn_all(&mut self) {
        if self.entries.is_empty() {
            return;
        }

        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx.clone());

        for entry in &self.entries {
            let entry = entry.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();

            tokio::spawn(async move {
                let mut backoff = Duration::from_secs(1);

                loop {
                    if *shutdown_rx.borrow() {
                        break;
                    }

                    match spawn_child(&entry) {
                        Err(e) => {
                            tracing::error!(
                                adapter = %entry.name,
                                "failed to spawn IPC adapter '{}': {e}",
                                entry.name
                            );
                        }
                        Ok(mut child) => {
                            tracing::info!(
                                adapter = %entry.name,
                                "IPC adapter '{}' started (pid {:?})",
                                entry.name,
                                child.id()
                            );

                            tokio::select! {
                                status = child.wait() => {
                                    match status {
                                        Ok(s) => tracing::warn!(
                                            adapter = %entry.name,
                                            "IPC adapter '{}' exited with status {s}",
                                            entry.name
                                        ),
                                        Err(e) => tracing::error!(
                                            adapter = %entry.name,
                                            "IPC adapter '{}' wait() error: {e}",
                                            entry.name
                                        ),
                                    }
                                }
                                _ = shutdown_rx.changed() => {
                                    tracing::info!(
                                        adapter = %entry.name,
                                        "shutdown requested; killing IPC adapter '{}'",
                                        entry.name
                                    );
                                    let _ = child.kill().await;
                                    let _ = child.wait().await;
                                    return;
                                }
                            }
                        }
                    }

                    if *shutdown_rx.borrow() {
                        break;
                    }

                    tracing::info!(
                        adapter = %entry.name,
                        "restarting IPC adapter '{}' in {:.0}s",
                        entry.name,
                        backoff.as_secs_f32()
                    );

                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = shutdown_rx.changed() => { break; }
                    }

                    // Exponential back-off, capped at 60 s.
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            });
        }
    }

    /// Signal all adapter processes to stop.  The restart loops will kill their
    /// child processes and exit.
    pub fn shutdown(&self) {
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(true);
        }
    }
}

fn spawn_child(entry: &IpcAdapterEntry) -> std::io::Result<Child> {
    Command::new(&entry.binary_path)
        .env("HOMECMDR_API_URL", &entry.api_url)
        .env("HOMECMDR_API_TOKEN", &entry.api_token)
        .env("HOMECMDR_ADAPTER_CONFIG", &entry.config_json)
        .kill_on_drop(true)
        .spawn()
}
