use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::runtime::RuntimeConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub runtime: RuntimeConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub persistence: PersistenceConfig,
    #[serde(default)]
    pub scenes: ScenesConfig,
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

#[derive(Debug, Deserialize)]
pub struct PersistenceConfig {
    pub enabled: bool,
    pub backend: PersistenceBackend,
    pub database_url: Option<String>,
    pub auto_create: bool,
}

#[derive(Debug, Deserialize)]
pub struct ScenesConfig {
    pub enabled: bool,
    pub directory: String,
}

impl Default for ScenesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "config/scenes".to_string(),
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

        Ok(())
    }
}
