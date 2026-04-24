use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct LocaleConfig {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Radius in metres of the auto-created home zone.  Defaults to 100 m.
    #[serde(default = "default_home_zone_radius")]
    pub home_zone_radius_meters: f64,
}

#[derive(Debug, Default, Deserialize)]
pub struct TelemetryConfig {
    pub enabled: bool,
    #[serde(default)]
    pub selection: TelemetrySelectionConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct TelemetrySelectionConfig {
    #[serde(default)]
    pub device_ids: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub adapter_names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_master_key")]
    pub master_key: String,
}

/// Configuration for the WASM plugin loader.
#[derive(Debug, Deserialize)]
pub struct PluginsConfig {
    /// Whether WASM plugin loading is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Directory to scan for `*.plugin.toml` manifests.
    #[serde(default = "default_plugins_directory")]
    pub directory: String,
}

impl Default for LocaleConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
            latitude: None,
            longitude: None,
            home_zone_radius_meters: default_home_zone_radius(),
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            master_key: default_master_key(),
        }
    }
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: default_plugins_directory(),
        }
    }
}

pub(super) fn default_timezone() -> String {
    "UTC".to_string()
}

fn default_master_key() -> String {
    "change-me-in-production".to_string()
}

fn default_plugins_directory() -> String {
    "config/plugins".to_string()
}

fn default_home_zone_radius() -> f64 {
    100.0
}
