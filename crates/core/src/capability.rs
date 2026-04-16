use std::collections::HashMap;

use crate::model::AttributeValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySchema {
    Measurement,
    Accumulation,
    Number,
    Integer,
    String,
    IntegerOrString,
    Boolean,
    Percentage,
    RgbColor,
    HexColor,
    XyColor,
    HsColor,
    ColorTemperature,
    Enum(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityDefinition {
    pub key: &'static str,
    pub schema: CapabilitySchema,
    pub read_only: bool,
    pub actions: &'static [&'static str],
    pub description: &'static str,
}

pub const TEMPERATURE_OUTDOOR: &str = "temperature_outdoor";
pub const TEMPERATURE_APPARENT: &str = "temperature_apparent";
pub const TEMPERATURE_HIGH: &str = "temperature_high";
pub const TEMPERATURE_LOW: &str = "temperature_low";
pub const WIND_SPEED: &str = "wind_speed";
pub const WIND_DIRECTION: &str = "wind_direction";
pub const WIND_GUST: &str = "wind_gust";
pub const RAINFALL: &str = "rainfall";
pub const UV_INDEX: &str = "uv_index";
pub const WEATHER_CONDITION: &str = "weather_condition";
pub const CLOUD_COVERAGE: &str = "cloud_coverage";
pub const VISIBILITY: &str = "visibility";
pub const BRIGHTNESS: &str = "brightness";
pub const COLOR_RGB: &str = "color_rgb";
pub const COLOR_HEX: &str = "color_hex";
pub const COLOR_XY: &str = "color_xy";
pub const COLOR_HS: &str = "color_hs";
pub const COLOR_TEMPERATURE: &str = "color_temperature";
pub const COLOR_MODE: &str = "color_mode";
pub const EFFECT: &str = "effect";
pub const TRANSITION: &str = "transition";
pub const ILLUMINANCE: &str = "illuminance";
pub const LED_INDICATION: &str = "led_indication";
pub const POWER: &str = "power";
pub const STATE: &str = "state";

pub const COLOR_MODE_VALUES: [&str; 5] = ["color_temp", "rgb", "xy", "hs", "white"];
pub const LIGHT_EFFECT_VALUES: [&str; 5] = ["none", "flash", "strobe", "colorloop", "random"];
pub const POWER_VALUES: [&str; 2] = ["on", "off"];
pub const AVAILABILITY_VALUES: [&str; 4] = ["online", "offline", "unavailable", "unknown"];

pub const ACTION_GET: [&str; 1] = ["get"];
pub const ACTION_SET: [&str; 1] = ["set"];
pub const ACTION_POWER: [&str; 3] = ["on", "off", "toggle"];
pub const ACTION_BRIGHTNESS: [&str; 3] = ["set", "increase", "decrease"];
pub const ACTION_COLOR_TEMPERATURE: [&str; 3] = ["set", "increase", "decrease"];
pub const ACTION_EFFECT: [&str; 2] = ["set", "stop"];
pub const ACTION_LED_INDICATION: [&str; 2] = ["get", "set"];

pub const WEATHER_CAPABILITIES: [CapabilityDefinition; 12] = [
    CapabilityDefinition {
        key: TEMPERATURE_OUTDOOR,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Outdoor ambient temperature; unit is celsius or fahrenheit.",
    },
    CapabilityDefinition {
        key: TEMPERATURE_APPARENT,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Outdoor feels-like temperature.",
    },
    CapabilityDefinition {
        key: TEMPERATURE_HIGH,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Forecast or observed daily high temperature.",
    },
    CapabilityDefinition {
        key: TEMPERATURE_LOW,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Forecast or observed daily low temperature.",
    },
    CapabilityDefinition {
        key: WIND_SPEED,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Wind speed; unit is km/h, mph, m/s, or knots.",
    },
    CapabilityDefinition {
        key: WIND_DIRECTION,
        schema: CapabilitySchema::IntegerOrString,
        read_only: true,
        actions: &ACTION_GET,
        description: "Wind direction in degrees or compass point.",
    },
    CapabilityDefinition {
        key: WIND_GUST,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Wind gust speed.",
    },
    CapabilityDefinition {
        key: RAINFALL,
        schema: CapabilitySchema::Accumulation,
        read_only: true,
        actions: &ACTION_GET,
        description: "Rainfall accumulation with period.",
    },
    CapabilityDefinition {
        key: UV_INDEX,
        schema: CapabilitySchema::Number,
        read_only: true,
        actions: &ACTION_GET,
        description: "UV index.",
    },
    CapabilityDefinition {
        key: WEATHER_CONDITION,
        schema: CapabilitySchema::String,
        read_only: true,
        actions: &ACTION_GET,
        description: "Weather condition identifier.",
    },
    CapabilityDefinition {
        key: CLOUD_COVERAGE,
        schema: CapabilitySchema::Integer,
        read_only: true,
        actions: &ACTION_GET,
        description: "Cloud coverage percentage.",
    },
    CapabilityDefinition {
        key: VISIBILITY,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Atmospheric visibility.",
    },
];

pub const LIGHT_CAPABILITIES: [CapabilityDefinition; 13] = [
    CapabilityDefinition {
        key: POWER,
        schema: CapabilitySchema::Enum(&POWER_VALUES),
        read_only: false,
        actions: &ACTION_POWER,
        description: "On/off power control.",
    },
    CapabilityDefinition {
        key: STATE,
        schema: CapabilitySchema::Enum(&AVAILABILITY_VALUES),
        read_only: true,
        actions: &ACTION_GET,
        description: "Connectivity / availability state.",
    },
    CapabilityDefinition {
        key: BRIGHTNESS,
        schema: CapabilitySchema::Percentage,
        read_only: false,
        actions: &ACTION_BRIGHTNESS,
        description: "Brightness level as a percentage (0-100).",
    },
    CapabilityDefinition {
        key: COLOR_RGB,
        schema: CapabilitySchema::RgbColor,
        read_only: false,
        actions: &ACTION_SET,
        description: "RGB colour control.",
    },
    CapabilityDefinition {
        key: COLOR_HEX,
        schema: CapabilitySchema::HexColor,
        read_only: false,
        actions: &ACTION_SET,
        description: "Hex colour control (#000000-#ffffff).",
    },
    CapabilityDefinition {
        key: COLOR_XY,
        schema: CapabilitySchema::XyColor,
        read_only: false,
        actions: &ACTION_SET,
        description: "CIE xy chromaticity colour control.",
    },
    CapabilityDefinition {
        key: COLOR_HS,
        schema: CapabilitySchema::HsColor,
        read_only: false,
        actions: &ACTION_SET,
        description: "Hue/saturation colour control.",
    },
    CapabilityDefinition {
        key: COLOR_TEMPERATURE,
        schema: CapabilitySchema::ColorTemperature,
        read_only: false,
        actions: &ACTION_COLOR_TEMPERATURE,
        description: "Colour temperature in mireds or kelvin.",
    },
    CapabilityDefinition {
        key: COLOR_MODE,
        schema: CapabilitySchema::Enum(&COLOR_MODE_VALUES),
        read_only: false,
        actions: &ACTION_SET,
        description: "Active colour mode of the light.",
    },
    CapabilityDefinition {
        key: EFFECT,
        schema: CapabilitySchema::Enum(&LIGHT_EFFECT_VALUES),
        read_only: false,
        actions: &ACTION_EFFECT,
        description: "Light effect control.",
    },
    CapabilityDefinition {
        key: TRANSITION,
        schema: CapabilitySchema::Number,
        read_only: false,
        actions: &ACTION_SET,
        description: "Transition duration in seconds.",
    },
    CapabilityDefinition {
        key: ILLUMINANCE,
        schema: CapabilitySchema::Integer,
        read_only: true,
        actions: &ACTION_GET,
        description: "Ambient light / illuminance sensor in lux.",
    },
    CapabilityDefinition {
        key: LED_INDICATION,
        schema: CapabilitySchema::Boolean,
        read_only: false,
        actions: &ACTION_LED_INDICATION,
        description: "LED status indicator on the device.",
    },
];

pub const ALL_CAPABILITIES: [&[CapabilityDefinition]; 2] =
    [&WEATHER_CAPABILITIES, &LIGHT_CAPABILITIES];

pub fn weather_capability(key: &str) -> Option<&'static CapabilityDefinition> {
    WEATHER_CAPABILITIES
        .iter()
        .find(|capability| capability.key == key)
}

pub fn light_capability(key: &str) -> Option<&'static CapabilityDefinition> {
    LIGHT_CAPABILITIES
        .iter()
        .find(|capability| capability.key == key)
}

pub fn capability_definition(key: &str) -> Option<&'static CapabilityDefinition> {
    ALL_CAPABILITIES
        .iter()
        .flat_map(|capabilities| capabilities.iter())
        .find(|capability| capability.key == key)
}

pub fn action_requires_value(action: &str) -> bool {
    matches!(action, "set" | "increase" | "decrease")
}

pub fn measurement_value(value: f64, unit: &str) -> AttributeValue {
    let mut fields = HashMap::new();
    fields.insert("value".to_string(), AttributeValue::Float(value));
    fields.insert("unit".to_string(), AttributeValue::Text(unit.to_string()));
    AttributeValue::Object(fields)
}

pub fn accumulation_value(value: f64, unit: &str, period: &str) -> AttributeValue {
    let mut fields = HashMap::new();
    fields.insert("value".to_string(), AttributeValue::Float(value));
    fields.insert("unit".to_string(), AttributeValue::Text(unit.to_string()));
    fields.insert(
        "period".to_string(),
        AttributeValue::Text(period.to_string()),
    );
    AttributeValue::Object(fields)
}
