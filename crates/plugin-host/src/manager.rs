/// Scans a directory for WASM plugin manifests (`*.plugin.toml`) and produces
/// a `WasmAdapterFactory` for each valid plugin found.
use std::path::Path;

use anyhow::{Context, Result};
use wasmtime::Engine;

use crate::adapter::WasmAdapterFactory;
use crate::manifest::PluginManifest;

pub struct PluginManager;

impl PluginManager {
    /// Scan `directory` for `*.plugin.toml` manifests and return the parsed
    /// manifests only — no WASM loading, no Engine required.
    ///
    /// Missing or unparseable manifests are skipped with a warning.
    pub fn scan_manifests(directory: &Path) -> Vec<PluginManifest> {
        let read_dir = match std::fs::read_dir(directory) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(
                    "failed to read plugin directory '{}': {e}",
                    directory.display()
                );
                return Vec::new();
            }
        };

        let mut manifests = Vec::new();

        for entry in read_dir {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();

            if !path
                .to_str()
                .map(|s| s.ends_with(".plugin.toml"))
                .unwrap_or(false)
            {
                continue;
            }

            match PluginManifest::load(&path) {
                Ok(m) => manifests.push(m),
                Err(e) => {
                    tracing::warn!("skipping plugin manifest '{}': {e}", path.display());
                }
            }
        }

        manifests
    }

    /// Scan `directory` for `*.plugin.toml` manifests.
    ///
    /// For each manifest found:
    /// - Derives the companion `.wasm` path (same stem, same directory).
    /// - Skips with a warning if the `.wasm` file does not exist.
    /// - Skips with a warning if the manifest cannot be parsed.
    ///
    /// Returns one `WasmAdapterFactory` per valid plugin, sharing `engine`.
    pub fn scan(directory: &Path, engine: &Engine) -> Result<Vec<WasmAdapterFactory>> {
        let mut factories = Vec::new();

        let read_dir = std::fs::read_dir(directory).with_context(|| {
            format!(
                "failed to read plugin directory '{}'",
                directory.display()
            )
        })?;

        for entry in read_dir {
            let entry = entry.with_context(|| {
                format!(
                    "failed to read entry in plugin directory '{}'",
                    directory.display()
                )
            })?;

            let path = entry.path();

            // Only process *.plugin.toml files.
            if !path
                .to_str()
                .map(|s| s.ends_with(".plugin.toml"))
                .unwrap_or(false)
            {
                continue;
            }

            let manifest = match PluginManifest::load(&path) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        "skipping plugin manifest '{}': {e}",
                        path.display()
                    );
                    continue;
                }
            };

            // IPC adapters are managed by IpcAdapterHost, not the WASM engine.
            if manifest.is_ipc() {
                tracing::info!(
                    plugin = manifest.plugin.name.as_str(),
                    version = manifest.plugin.version.as_str(),
                    "discovered IPC adapter (skipping WASM load)"
                );
                continue;
            }

            let wasm_path = PluginManifest::wasm_path_for(&path);
            if !wasm_path.exists() {
                tracing::warn!(
                    plugin = manifest.plugin.name.as_str(),
                    "skipping plugin '{}': WASM binary not found at '{}'",
                    manifest.plugin.name,
                    wasm_path.display()
                );
                continue;
            }

            tracing::info!(
                plugin = manifest.plugin.name.as_str(),
                version = manifest.plugin.version.as_str(),
                "discovered WASM plugin"
            );

            factories.push(WasmAdapterFactory::new(
                engine.clone(),
                wasm_path,
                manifest,
            ));
        }

        Ok(factories)
    }
}
