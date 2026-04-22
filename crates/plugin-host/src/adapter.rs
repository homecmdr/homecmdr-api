/// `WasmAdapter` and `WasmAdapterFactory` bridge the WASM plugin system to the
/// `homecmdr_core::adapter` traits so the rest of the API layer is unaware of whether
/// an adapter is native or WASM.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use homecmdr_core::adapter::{Adapter, AdapterFactory};
use homecmdr_core::bus::EventBus;
use homecmdr_core::command::DeviceCommand;
use homecmdr_core::config::AdapterConfig;
use homecmdr_core::event::Event;
use homecmdr_core::model::{Attributes, Device, DeviceId, DeviceKind, Metadata};
use homecmdr_core::registry::DeviceRegistry;
use wasmtime::Engine;

use crate::manifest::PluginManifest;
use crate::plugin::{DeviceUpdate, WasmPlugin};

// ---------------------------------------------------------------------------
// WasmAdapter — implements Adapter, drives the poll loop
// ---------------------------------------------------------------------------

pub struct WasmAdapter {
    /// Adapter name (matches the key in config [adapters]).
    name: String,
    /// Shared WASM plugin state protected by a regular (blocking) mutex.
    /// `std::sync::Mutex` is used because `WasmPlugin` is only accessed from
    /// `spawn_blocking` closures, which must not hold a tokio async guard.
    plugin: Arc<Mutex<WasmPlugin>>,
    /// Host-managed poll cadence.
    poll_interval: Duration,
}

#[async_trait::async_trait]
impl Adapter for WasmAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(Event::AdapterStarted {
            adapter: self.name.clone(),
        });

        loop {
            let plugin = Arc::clone(&self.plugin);
            let result = tokio::task::spawn_blocking(move || plugin.lock().unwrap().plugin_poll())
                .await
                .context("WASM poll task panicked")?;

            match result {
                Ok(updates) => {
                    for update in updates {
                        if let Err(e) = apply_device_update(&self.name, &update, &registry).await {
                            tracing::warn!(
                                adapter = self.name.as_str(),
                                "device upsert failed: {e}"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(adapter = self.name.as_str(), "poll failed: {e}");
                    bus.publish(Event::SystemError {
                        message: format!("{} poll failed: {e}", self.name),
                    });
                }
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn command(
        &self,
        device_id: &DeviceId,
        command: DeviceCommand,
        _registry: DeviceRegistry,
    ) -> Result<bool> {
        let plugin = Arc::clone(&self.plugin);
        let device_id_str = device_id.0.clone();
        let command_json =
            serde_json::to_string(&command).context("failed to serialise DeviceCommand")?;

        let result = tokio::task::spawn_blocking(move || {
            plugin
                .lock()
                .unwrap()
                .plugin_command(&device_id_str, &command_json)
        })
        .await
        .context("WASM command task panicked")??;

        if let Some(err) = result.error {
            return Err(anyhow::anyhow!("{err}"));
        }
        Ok(result.handled)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a `DeviceUpdate` from the WASM guest into a `Device` and upsert it.
async fn apply_device_update(
    adapter_name: &str,
    update: &DeviceUpdate,
    registry: &DeviceRegistry,
) -> Result<()> {
    let device_id = DeviceId(format!("{adapter_name}:{}", update.vendor_id));

    let attributes: Attributes =
        serde_json::from_str(&update.attributes_json).with_context(|| {
            format!(
                "failed to parse attributes JSON for device '{}'",
                device_id.0
            )
        })?;

    let kind = parse_device_kind(&update.kind);

    // Preserve room_id if the device already exists in the registry.
    let room_id = registry.get(&device_id).and_then(|d| d.room_id.clone());

    let now = Utc::now();
    let device = Device {
        id: device_id,
        room_id,
        kind,
        attributes,
        metadata: Metadata {
            source: adapter_name.to_string(),
            accuracy: None,
            vendor_specific: HashMap::new(),
        },
        updated_at: now,
        last_seen: now,
    };

    registry.upsert(device).await
}

fn parse_device_kind(s: &str) -> DeviceKind {
    match s {
        "light" => DeviceKind::Light,
        "switch" => DeviceKind::Switch,
        "virtual" => DeviceKind::Virtual,
        _ => DeviceKind::Sensor,
    }
}

// ---------------------------------------------------------------------------
// WasmAdapterFactory — implements AdapterFactory
// ---------------------------------------------------------------------------

/// Factory that loads a WASM plugin file, calls `init()`, and produces a
/// `WasmAdapter`.  Lives for the process lifetime; `name` is leaked to satisfy
/// the `&'static str` bound on `AdapterFactory::name()`.
pub struct WasmAdapterFactory {
    /// Leaked once at construction — safe for process-lifetime factories.
    name: &'static str,
    engine: Engine,
    wasm_path: PathBuf,
    manifest: PluginManifest,
}

impl WasmAdapterFactory {
    pub fn new(engine: Engine, wasm_path: PathBuf, manifest: PluginManifest) -> Self {
        // Leak the name once so we can return `&'static str` from AdapterFactory::name().
        let name: &'static str = Box::leak(manifest.plugin.name.clone().into_boxed_str());
        Self {
            name,
            engine,
            wasm_path,
            manifest,
        }
    }
}

impl AdapterFactory for WasmAdapterFactory {
    fn name(&self) -> &'static str {
        self.name
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        // Honour a top-level `enabled` field if present (same convention as native adapters).
        let enabled = config
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !enabled {
            return Ok(None);
        }

        let config_json =
            serde_json::to_string(&config).context("failed to serialise adapter config")?;

        let mut plugin = WasmPlugin::load(&self.engine, &self.wasm_path).with_context(|| {
            format!("failed to load WASM plugin '{}'", self.wasm_path.display())
        })?;
        plugin.plugin_init(&config_json)?;

        // Allow per-adapter config to override the manifest default.
        let poll_interval_secs = config
            .get("poll_interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.manifest.runtime.poll_interval_secs);

        Ok(Some(Box::new(WasmAdapter {
            name: self.name.to_string(),
            plugin: Arc::new(Mutex::new(plugin)),
            poll_interval: Duration::from_secs(poll_interval_secs),
        })))
    }
}
