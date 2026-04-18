use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use chrono_tz::Tz;
use serde::Deserialize;

use crate::runtime::RuntimeConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub locale: LocaleConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub persistence: PersistenceConfig,
    #[serde(default)]
    pub scenes: ScenesConfig,
    #[serde(default)]
    pub automations: AutomationsConfig,
    #[serde(default)]
    pub scripts: ScriptsConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub adapters: AdaptersConfig,
}

pub type AdapterConfig = serde_json::Value;
pub type AdaptersConfig = HashMap<String, AdapterConfig>;

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_api_bind_address")]
    pub bind_address: String,
    #[serde(default)]
    pub cors: ApiCorsConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiCorsConfig {
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocaleConfig {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct PersistenceConfig {
    pub enabled: bool,
    pub backend: PersistenceBackend,
    pub database_url: Option<String>,
    pub auto_create: bool,
    #[serde(default)]
    pub history: HistoryConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub retention_days: Option<u64>,
    #[serde(default = "default_history_query_limit")]
    pub default_query_limit: usize,
    #[serde(default = "default_history_max_query_limit")]
    pub max_query_limit: usize,
}

#[derive(Debug, Deserialize)]
pub struct ScenesConfig {
    pub enabled: bool,
    pub directory: String,
    #[serde(default)]
    pub watch: bool,
}

#[derive(Debug, Deserialize)]
pub struct AutomationsConfig {
    pub enabled: bool,
    pub directory: String,
    #[serde(default)]
    pub watch: bool,
}

#[derive(Debug, Deserialize)]
pub struct ScriptsConfig {
    pub enabled: bool,
    pub directory: String,
    #[serde(default)]
    pub watch: bool,
}

impl Default for ScenesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "config/scenes".to_string(),
            watch: false,
        }
    }
}

impl Default for LocaleConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
            latitude: None,
            longitude: None,
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind_address: default_api_bind_address(),
            cors: ApiCorsConfig::default(),
        }
    }
}

impl Default for AutomationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "config/automations".to_string(),
            watch: false,
        }
    }
}

impl Default for ScriptsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "config/scripts".to_string(),
            watch: false,
        }
    }
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: PersistenceBackend::Sqlite,
            database_url: Some("sqlite://data/smart-home.db".to_string()),
            auto_create: true,
            history: HistoryConfig::default(),
        }
    }
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: None,
            default_query_limit: default_history_query_limit(),
            max_query_limit: default_history_max_query_limit(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceBackend {
    Sqlite,
    Postgres,
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

impl Config {
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let config = config::Config::builder()
            .add_source(config::File::from(path))
            .build()
            .with_context(|| format!("failed to load config file {}", path.display()))?;

        let config: Self = config.try_deserialize().map_err(|error| {
            anyhow!(
                "failed to deserialize config file {}: {}",
                path.display(),
                error
            )
        })?;

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.api.bind_address.trim().is_empty() {
            bail!("api.bind_address is required");
        }

        if self.api.cors.enabled {
            if self.api.cors.allowed_origins.is_empty() {
                bail!("api.cors.allowed_origins must not be empty when api.cors.enabled is true");
            }

            for origin in &self.api.cors.allowed_origins {
                if origin.trim().is_empty() {
                    bail!("api.cors.allowed_origins must not contain empty origins");
                }

                let parsed = url::Url::parse(origin).map_err(|error| {
                    anyhow!(
                        "api.cors.allowed_origins contains invalid origin '{}': {}",
                        origin,
                        error
                    )
                })?;

                match parsed.scheme() {
                    "http" | "https" => {}
                    scheme => {
                        bail!(
                            "api.cors.allowed_origins contains unsupported scheme '{}' in '{}'",
                            scheme,
                            origin
                        );
                    }
                }

                if parsed.host_str().is_none() {
                    bail!(
                        "api.cors.allowed_origins must include a host component: '{}'",
                        origin
                    );
                }

                if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
                    bail!(
                        "api.cors.allowed_origins must be bare origins without path, query, or fragment: '{}'",
                        origin
                    );
                }
            }
        }

        if self.persistence.enabled
            && self
                .persistence
                .database_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
        {
            bail!("persistence.database_url is required when persistence is enabled");
        }

        if self.scenes.enabled && self.scenes.directory.trim().is_empty() {
            bail!("scenes.directory is required when scenes are enabled");
        }

        self.locale.timezone.parse::<Tz>().map_err(|_| {
            anyhow!(
                "locale.timezone '{}' is not a valid IANA timezone",
                self.locale.timezone
            )
        })?;

        if self.automations.enabled && self.automations.directory.trim().is_empty() {
            bail!("automations.directory is required when automations are enabled");
        }

        if self.scripts.enabled && self.scripts.directory.trim().is_empty() {
            bail!("scripts.directory is required when scripts are enabled");
        }

        if self.persistence.history.default_query_limit == 0 {
            bail!("persistence.history.default_query_limit must be > 0");
        }

        if self.persistence.history.max_query_limit == 0 {
            bail!("persistence.history.max_query_limit must be > 0");
        }

        if self.persistence.history.default_query_limit > self.persistence.history.max_query_limit {
            bail!(
                "persistence.history.default_query_limit must be <= persistence.history.max_query_limit"
            );
        }

        Ok(())
    }
}

fn default_history_query_limit() -> usize {
    200
}

fn default_timezone() -> String {
    "UTC".to_string()
}

fn default_api_bind_address() -> String {
    "127.0.0.1:3000".to_string()
}

fn default_history_max_query_limit() -> usize {
    1000
}
