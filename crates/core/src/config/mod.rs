mod api;
mod persistence;
mod runtime;
mod system;

pub use api::{ApiConfig, ApiCorsConfig, RateLimitConfig};
pub use persistence::{HistoryConfig, PersistenceBackend, PersistenceConfig};
pub use runtime::{AutomationRunnerConfig, AutomationsConfig, ScriptsConfig, ScenesConfig};
pub use system::{AuthConfig, LocaleConfig, PluginsConfig, TelemetryConfig, TelemetrySelectionConfig};

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
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
}

pub type AdapterConfig = serde_json::Value;
pub type AdaptersConfig = HashMap<String, AdapterConfig>;

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
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

        if self.auth.master_key.trim().is_empty() {
            bail!("auth.master_key must not be empty");
        }

        if self.plugins.enabled && self.plugins.directory.trim().is_empty() {
            bail!("plugins.directory is required when plugins are enabled");
        }

        Ok(())
    }
}
