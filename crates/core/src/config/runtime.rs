use serde::Deserialize;

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
    #[serde(default)]
    pub runner: AutomationRunnerConfig,
}

/// Limits applied to the automation runner at startup.
#[derive(Debug, Clone, Deserialize)]
pub struct AutomationRunnerConfig {
    /// Maximum number of concurrent executions per automation when `mode =
    /// "parallel"` is specified without an explicit `max` in the Lua script.
    /// Defaults to 8.
    #[serde(default = "default_automation_default_max_concurrent")]
    pub default_max_concurrent: usize,
    /// Hard ceiling (in seconds) on how long any single automation execution
    /// may run before it is forcibly cancelled.  Defaults to 3600 (1 hour).
    #[serde(default = "default_automation_backstop_timeout_secs")]
    pub backstop_timeout_secs: u64,
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

impl Default for AutomationRunnerConfig {
    fn default() -> Self {
        Self {
            default_max_concurrent: default_automation_default_max_concurrent(),
            backstop_timeout_secs: default_automation_backstop_timeout_secs(),
        }
    }
}

impl Default for AutomationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "config/automations".to_string(),
            watch: false,
            runner: AutomationRunnerConfig::default(),
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

fn default_automation_default_max_concurrent() -> usize {
    8
}

fn default_automation_backstop_timeout_secs() -> u64 {
    3600
}
