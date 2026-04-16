use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::runtime::RuntimeConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub runtime: RuntimeConfig,
    pub logging: LoggingConfig,
    pub adapters: AdaptersConfig,
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
}

#[derive(Debug, Deserialize)]
pub struct AdaptersConfig {
    pub open_meteo: OpenMeteoConfig,
}

#[derive(Debug, Deserialize)]
pub struct OpenMeteoConfig {
    pub enabled: bool,
    pub latitude: f64,
    pub longitude: f64,
    pub poll_interval_secs: u64,
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
        if self.adapters.open_meteo.poll_interval_secs < 60 {
            bail!("adapters.open_meteo.poll_interval_secs must be >= 60");
        }

        Ok(())
    }
}
