use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Parsed contents of a `<plugin-name>.plugin.toml` manifest file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    #[serde(default)]
    pub runtime: PluginRuntimeConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginMeta {
    /// Unique plugin name.  Must match the key in config [adapters.<name>].
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// WIT API version this plugin was compiled against.
    #[serde(default = "default_api_version")]
    pub api_version: String,
    /// "wasm" (default) or "ipc".  IPC plugins are standalone native binaries
    /// managed as child processes; they are not loaded into the WASM engine.
    #[serde(default = "default_adapter_type")]
    pub adapter_type: String,
    /// Binary filename for IPC adapters.  Defaults to `"{name}-adapter"`.
    /// The file is expected in the same directory as the `.plugin.toml`.
    #[serde(default)]
    pub binary: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginRuntimeConfig {
    /// Default poll interval in seconds.  May be overridden by the adapter
    /// config in `config/default.toml`.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
}

impl Default for PluginRuntimeConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval_secs(),
        }
    }
}

fn default_api_version() -> String {
    "0.1.0".to_string()
}

fn default_adapter_type() -> String {
    "wasm".to_string()
}

fn default_poll_interval_secs() -> u64 {
    300
}

impl PluginManifest {
    /// Load a manifest from a `.plugin.toml` path.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read plugin manifest '{}': {e}", path.display()))?;
        toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse plugin manifest '{}': {e}", path.display()))
    }

    /// Derive the expected `.wasm` path from a manifest path.
    ///
    /// Given `/path/to/open_meteo.plugin.toml`, returns
    /// `/path/to/open_meteo.wasm`.
    pub fn wasm_path_for(manifest_path: &Path) -> std::path::PathBuf {
        let stem = manifest_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("plugin")
            .trim_end_matches(".plugin.toml");
        manifest_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!("{stem}.wasm"))
    }

    /// Returns true when this manifest describes an IPC adapter (a standalone
    /// native binary) rather than a WASM plugin.
    pub fn is_ipc(&self) -> bool {
        self.plugin.adapter_type == "ipc"
    }

    /// Derive the expected binary path for an IPC adapter from a manifest path.
    ///
    /// The binary filename is taken from `[plugin] binary` when set, otherwise
    /// it defaults to `"{name}-adapter"`.
    pub fn binary_path_for(manifest_path: &Path, manifest: &PluginManifest) -> std::path::PathBuf {
        let binary_name = manifest
            .plugin
            .binary
            .as_deref()
            .map(|b| b.to_string())
            .unwrap_or_else(|| format!("{}-adapter", manifest.plugin.name));
        manifest_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(binary_name)
    }
}
