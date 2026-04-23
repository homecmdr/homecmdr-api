use serde::Deserialize;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceBackend {
    Sqlite,
    Postgres,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: PersistenceBackend::Sqlite,
            database_url: Some("sqlite://data/homecmdr.db".to_string()),
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

pub(super) fn default_history_query_limit() -> usize {
    200
}

pub(super) fn default_history_max_query_limit() -> usize {
    1000
}
