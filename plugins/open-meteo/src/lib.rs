//! Open-Meteo WASM plugin — weather adapter using the HomeCmdr WASM plugin API.
//!
//! Implements the `adapter` world defined in `wit/homecmdr-plugin.wit`.
//! All HTTP is done via the host-provided `host-http` import; no networking is
//! performed directly by the WASM module.
//!
//! Build with:
//!   cargo build --target wasm32-wasip2 --release
//! then copy `target/wasm32-wasip2/release/adapter_open_meteo_wasm.wasm` (and
//! rename it to `open_meteo.wasm`) into `config/plugins/`.

wit_bindgen::generate!({
    world: "adapter",
    path: "wit/homecmdr-plugin.wit",
});

use exports::homecmdr::plugin::plugin::{CommandResult, DeviceUpdate, Guest};
use homecmdr::plugin::host_http;
use homecmdr::plugin::host_log;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Plugin config (parsed from config-json on init)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    latitude: f64,
    longitude: f64,
    #[serde(default = "default_base_url")]
    base_url: String,
}

fn default_true() -> bool {
    true
}

fn default_base_url() -> String {
    "https://api.open-meteo.com".to_string()
}

// ---------------------------------------------------------------------------
// Open-Meteo API response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    current: CurrentWeather,
}

#[derive(Debug, Deserialize)]
struct CurrentWeather {
    temperature_2m: f64,
    apparent_temperature: f64,
    relative_humidity_2m: f64,
    precipitation: f64,
    cloud_cover: u8,
    uv_index: f64,
    surface_pressure: f64,
    wind_speed_10m: f64,
    wind_gusts_10m: f64,
    wind_direction_10m: f64,
    weather_code: u8,
    is_day: u8,
}

// ---------------------------------------------------------------------------
// Global plugin state (single-threaded WASM — no Mutex needed)
// ---------------------------------------------------------------------------

// SAFETY: WASM is single-threaded; Cell<Option<Config>> is sufficient.
thread_local! {
    static STATE: std::cell::RefCell<Option<Config>> = std::cell::RefCell::new(None);
}

// ---------------------------------------------------------------------------
// Guest trait implementation
// ---------------------------------------------------------------------------

struct OpenMeteoPlugin;

impl Guest for OpenMeteoPlugin {
    fn name() -> String {
        "open_meteo".to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("failed to parse open_meteo config: {e}"))?;

        if !config.enabled {
            host_log::log("info", "open_meteo plugin is disabled by config");
            return Ok(());
        }

        host_log::log(
            "info",
            &format!(
                "open_meteo initialised for lat={} lon={}",
                config.latitude, config.longitude
            ),
        );

        STATE.with(|s| {
            *s.borrow_mut() = Some(config);
        });

        Ok(())
    }

    fn poll() -> Result<Vec<DeviceUpdate>, String> {
        let (base_url, latitude, longitude) = STATE.with(|s| {
            let borrowed = s.borrow();
            match &*borrowed {
                Some(cfg) => Ok((cfg.base_url.clone(), cfg.latitude, cfg.longitude)),
                None => Err("open_meteo not initialised or disabled".to_string()),
            }
        })?;

        let url = format!(
            "{}/v1/forecast\
             ?latitude={latitude}\
             &longitude={longitude}\
             &current=temperature_2m,apparent_temperature,relative_humidity_2m,\
             precipitation,cloud_cover,uv_index,surface_pressure,\
             wind_speed_10m,wind_gusts_10m,wind_direction_10m,weather_code,is_day",
            base_url.trim_end_matches('/')
        );

        let body = host_http::get(&url)
            .map_err(|e| format!("open_meteo HTTP GET failed: {e}"))?;

        let forecast: ForecastResponse =
            serde_json::from_str(&body).map_err(|e| format!("open_meteo parse error: {e}"))?;

        let w = forecast.current;
        let condition = wmo_code_to_description(w.weather_code);
        let is_day = w.is_day != 0;

        let updates = vec![
            device_update(
                "temperature_outdoor",
                "sensor",
                measurement_json(w.temperature_2m, "celsius"),
            ),
            device_update(
                "wind_speed",
                "sensor",
                measurement_json(w.wind_speed_10m, "km/h"),
            ),
            device_update(
                "wind_direction",
                "sensor",
                integer_attr_json("wind_direction", w.wind_direction_10m as i64),
            ),
            device_update(
                "temperature_apparent",
                "sensor",
                measurement_json(w.apparent_temperature, "celsius"),
            ),
            device_update(
                "humidity",
                "sensor",
                measurement_json(w.relative_humidity_2m, "percent"),
            ),
            device_update(
                "rainfall",
                "sensor",
                accumulation_json(w.precipitation, "mm", "hour"),
            ),
            device_update(
                "cloud_coverage",
                "sensor",
                integer_attr_json("cloud_coverage", w.cloud_cover as i64),
            ),
            device_update(
                "uv_index",
                "sensor",
                float_attr_json("uv_index", w.uv_index),
            ),
            device_update(
                "pressure",
                "sensor",
                measurement_json(w.surface_pressure, "hPa"),
            ),
            device_update(
                "wind_gust",
                "sensor",
                measurement_json(w.wind_gusts_10m, "km/h"),
            ),
            device_update(
                "weather_condition",
                "sensor",
                text_attr_json("weather_condition", &condition),
            ),
            device_update(
                "is_day",
                "sensor",
                bool_attr_json("custom.open_meteo.is_day", is_day),
            ),
        ];

        Ok(updates)
    }

    fn command(_device_id: String, _command_json: String) -> Result<CommandResult, String> {
        // Weather sensors are read-only.
        Ok(CommandResult {
            handled: false,
            error: None,
        })
    }
}

export!(OpenMeteoPlugin);

// ---------------------------------------------------------------------------
// Attribute JSON helpers
// ---------------------------------------------------------------------------

/// Build a `DeviceUpdate` with the given vendor_id, kind, and attributes JSON.
fn device_update(vendor_id: &str, kind: &str, attributes_json: String) -> DeviceUpdate {
    DeviceUpdate {
        vendor_id: vendor_id.to_string(),
        kind: kind.to_string(),
        attributes_json,
    }
}

/// `{"<key>": {"value": <f>, "unit": "<unit>"}}`
fn measurement_json(value: f64, unit: &str) -> String {
    let key = unit_to_key(unit);
    serde_json::json!({
        key: { "value": value, "unit": unit }
    })
    .to_string()
}

/// `{"<key>": {"value": <f>, "unit": "<unit>", "period": "<period>"}}`
fn accumulation_json(value: f64, unit: &str, period: &str) -> String {
    let key = "rainfall";
    serde_json::json!({
        key: { "value": value, "unit": unit, "period": period }
    })
    .to_string()
}

fn integer_attr_json(key: &str, value: i64) -> String {
    serde_json::json!({ key: value }).to_string()
}

fn float_attr_json(key: &str, value: f64) -> String {
    serde_json::json!({ key: value }).to_string()
}

fn text_attr_json(key: &str, value: &str) -> String {
    serde_json::json!({ key: value }).to_string()
}

fn bool_attr_json(key: &str, value: bool) -> String {
    serde_json::json!({ key: value }).to_string()
}

/// Map a unit string to the attribute key used by the core capability model.
fn unit_to_key(unit: &str) -> &'static str {
    match unit {
        "celsius" => "temperature_outdoor",
        "km/h" => "wind_speed",
        "percent" => "humidity",
        "hPa" => "pressure",
        _ => "value",
    }
}

// ---------------------------------------------------------------------------
// WMO weather code descriptions (mirrors native adapter)
// ---------------------------------------------------------------------------

fn wmo_code_to_description(code: u8) -> String {
    let description = match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51 => "Light drizzle",
        53 => "Moderate drizzle",
        55 => "Dense drizzle",
        56 | 57 => "Freezing drizzle",
        61 => "Slight rain",
        63 => "Moderate rain",
        65 => "Heavy rain",
        66 | 67 => "Freezing rain",
        71 => "Slight snow",
        73 => "Moderate snow",
        75 => "Heavy snow",
        77 => "Snow grains",
        80 => "Slight showers",
        81 => "Moderate showers",
        82 => "Violent showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown",
    };
    description.to_string()
}
