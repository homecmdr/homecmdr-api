use serde::Deserialize;

/// HTTP server configuration (bind address, CORS, rate limits).
#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_api_bind_address")]
    pub bind_address: String,
    #[serde(default)]
    pub cors: ApiCorsConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
}

/// Token-bucket rate limit applied to command and scene-execution endpoints.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    /// Enable rate limiting on write endpoints.  Defaults to false.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of requests allowed per second across all write
    /// endpoints.  Defaults to 100.
    #[serde(default = "default_rate_limit_requests_per_second")]
    pub requests_per_second: u64,
    /// Maximum burst size above the steady-state rate.  Defaults to 20.
    #[serde(default = "default_rate_limit_burst_size")]
    pub burst_size: u64,
}

/// CORS config — required when the front-end is served from a different origin
/// (e.g. a Vite dev server on port 5173 hitting the API on port 3001).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiCorsConfig {
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind_address: default_api_bind_address(),
            cors: ApiCorsConfig::default(),
            rate_limit: RateLimitConfig::default(),
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            requests_per_second: default_rate_limit_requests_per_second(),
            burst_size: default_rate_limit_burst_size(),
        }
    }
}

pub(super) fn default_api_bind_address() -> String {
    "127.0.0.1:3000".to_string()
}

fn default_rate_limit_requests_per_second() -> u64 {
    100
}

fn default_rate_limit_burst_size() -> u64 {
    20
}
