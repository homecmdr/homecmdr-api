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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityDefinition {
    pub key: &'static str,
    pub schema: CapabilitySchema,
    pub read_only: bool,
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

pub const WEATHER_CAPABILITIES: [CapabilityDefinition; 12] = [
    CapabilityDefinition {
        key: TEMPERATURE_OUTDOOR,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        description: "Outdoor ambient temperature; unit is celsius or fahrenheit.",
    },
    CapabilityDefinition {
        key: TEMPERATURE_APPARENT,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        description: "Outdoor feels-like temperature.",
    },
    CapabilityDefinition {
        key: TEMPERATURE_HIGH,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        description: "Forecast or observed daily high temperature.",
    },
    CapabilityDefinition {
        key: TEMPERATURE_LOW,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        description: "Forecast or observed daily low temperature.",
    },
    CapabilityDefinition {
        key: WIND_SPEED,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        description: "Wind speed; unit is km/h, mph, m/s, or knots.",
    },
    CapabilityDefinition {
        key: WIND_DIRECTION,
        schema: CapabilitySchema::IntegerOrString,
        read_only: true,
        description: "Wind direction in degrees or compass point.",
    },
    CapabilityDefinition {
        key: WIND_GUST,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        description: "Wind gust speed.",
    },
    CapabilityDefinition {
        key: RAINFALL,
        schema: CapabilitySchema::Accumulation,
        read_only: true,
        description: "Rainfall accumulation with period.",
    },
    CapabilityDefinition {
        key: UV_INDEX,
        schema: CapabilitySchema::Number,
        read_only: true,
        description: "UV index.",
    },
    CapabilityDefinition {
        key: WEATHER_CONDITION,
        schema: CapabilitySchema::String,
        read_only: true,
        description: "Weather condition identifier.",
    },
    CapabilityDefinition {
        key: CLOUD_COVERAGE,
        schema: CapabilitySchema::Integer,
        read_only: true,
        description: "Cloud coverage percentage.",
    },
    CapabilityDefinition {
        key: VISIBILITY,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        description: "Atmospheric visibility.",
    },
];

pub fn weather_capability(key: &str) -> Option<&'static CapabilityDefinition> {
    WEATHER_CAPABILITIES
        .iter()
        .find(|capability| capability.key == key)
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
