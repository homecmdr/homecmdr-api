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
    pub domain: &'static str,
    pub key: &'static str,
    pub schema: CapabilitySchema,
    pub read_only: bool,
    pub actions: &'static [&'static str],
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityOwnershipPolicy {
    pub canonical_attribute_location: &'static str,
    pub custom_attribute_prefix: &'static str,
    pub vendor_metadata_field: &'static str,
    pub rules: &'static [&'static str],
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
pub const BATTERY: &str = "battery";
pub const TEMPERATURE: &str = "temperature";
pub const HUMIDITY: &str = "humidity";
pub const PRESSURE: &str = "pressure";
pub const CO2: &str = "co2";
pub const AIR_QUALITY: &str = "air_quality";
pub const MOTION: &str = "motion";
pub const CONTACT: &str = "contact";
pub const OCCUPANCY: &str = "occupancy";
pub const SMOKE: &str = "smoke";
pub const WATER_LEAK: &str = "water_leak";
pub const TARGET_TEMPERATURE: &str = "target_temperature";
pub const HVAC_MODE: &str = "hvac_mode";
pub const HVAC_STATE: &str = "hvac_state";
pub const FAN_MODE: &str = "fan_mode";
pub const SWING_MODE: &str = "swing_mode";
pub const PRESET_MODE: &str = "preset_mode";
pub const POWER_CONSUMPTION: &str = "power_consumption";
pub const ENERGY_TOTAL: &str = "energy_total";
pub const ENERGY_TODAY: &str = "energy_today";
pub const ENERGY_YESTERDAY: &str = "energy_yesterday";
pub const ENERGY_MONTH: &str = "energy_month";
pub const VOLTAGE: &str = "voltage";
pub const CURRENT: &str = "current";
pub const VOLUME: &str = "volume";
pub const MUTED: &str = "muted";
pub const MEDIA_SOURCE: &str = "media_source";
pub const MEDIA_TITLE: &str = "media_title";
pub const MEDIA_APP: &str = "media_app";
pub const MEDIA_PLAYBACK: &str = "media_playback";
pub const LOCK: &str = "lock";
pub const DOOR: &str = "door";
pub const GARAGE_DOOR: &str = "garage_door";
pub const COVER_POSITION: &str = "cover_position";
pub const COVER_TILT: &str = "cover_tilt";
// tracker capability keys
pub const TRACKER_TYPE: &str = "tracker.type";
pub const TRACKER_STATE: &str = "tracker.state";
pub const TRACKER_LATITUDE: &str = "tracker.latitude";
pub const TRACKER_LONGITUDE: &str = "tracker.longitude";
pub const TRACKER_ACCURACY: &str = "tracker.accuracy";
pub const TRACKER_CONSIDER_HOME: &str = "tracker.consider_home";
pub const CUSTOM_ATTRIBUTE_PREFIX: &str = "custom.";

pub const COLOR_MODE_VALUES: [&str; 5] = ["color_temp", "rgb", "xy", "hs", "white"];
pub const LIGHT_EFFECT_VALUES: [&str; 5] = ["none", "flash", "strobe", "colorloop", "random"];
pub const POWER_VALUES: [&str; 2] = ["on", "off"];
pub const AVAILABILITY_VALUES: [&str; 4] = ["online", "offline", "unavailable", "unknown"];
pub const AIR_QUALITY_VALUES: [&str; 7] = [
    "unknown",
    "excellent",
    "good",
    "moderate",
    "poor",
    "unhealthy",
    "hazardous",
];
pub const MOTION_VALUES: [&str; 2] = ["detected", "clear"];
pub const CONTACT_VALUES: [&str; 2] = ["open", "closed"];
pub const OCCUPANCY_VALUES: [&str; 2] = ["occupied", "unoccupied"];
pub const HVAC_MODE_VALUES: [&str; 7] = [
    "off",
    "heat",
    "cool",
    "auto",
    "heat_cool",
    "dry",
    "fan_only",
];
pub const HVAC_STATE_VALUES: [&str; 6] = ["off", "idle", "heating", "cooling", "fan_only", "dry"];
pub const MEDIA_PLAYBACK_VALUES: [&str; 5] = ["playing", "paused", "stopped", "buffering", "idle"];
pub const LOCK_VALUES: [&str; 3] = ["locked", "unlocked", "jammed"];
pub const ENTRY_STATE_VALUES: [&str; 6] =
    ["open", "closed", "opening", "closing", "stopped", "jammed"];

pub const ACTION_GET: [&str; 1] = ["get"];
pub const ACTION_SET: [&str; 1] = ["set"];
pub const ACTION_POWER: [&str; 3] = ["on", "off", "toggle"];
pub const ACTION_BRIGHTNESS: [&str; 3] = ["set", "increase", "decrease"];
pub const ACTION_COLOR_TEMPERATURE: [&str; 3] = ["set", "increase", "decrease"];
pub const ACTION_EFFECT: [&str; 2] = ["set", "stop"];
pub const ACTION_LED_INDICATION: [&str; 2] = ["get", "set"];
pub const ACTION_RESET: [&str; 1] = ["reset"];
pub const ACTION_VOLUME: [&str; 3] = ["set", "increase", "decrease"];
pub const ACTION_MUTED: [&str; 3] = ["set", "mute", "unmute"];
pub const ACTION_MEDIA_PLAYBACK: [&str; 5] = ["play", "pause", "stop", "next", "previous"];
pub const ACTION_LOCK: [&str; 2] = ["lock", "unlock"];
pub const ACTION_OPEN_CLOSE_STOP: [&str; 3] = ["open", "close", "stop"];
pub const ACTION_COVER: [&str; 4] = ["open", "close", "stop", "set"];
// tracker type values: gps = phone/satellite, stationary = wifi/router, ble = beacon
pub const TRACKER_TYPE_VALUES: [&str; 3] = ["gps", "stationary", "ble"];
// tracker state values: home, not_home, or a zone id string
pub const TRACKER_STATE_VALUES: [&str; 2] = ["home", "not_home"];

pub const CAPABILITY_OWNERSHIP_RULES: [&str; 4] = [
    "Use device.attributes.<capability_key> whenever a canonical capability already exists for the state or command.",
    "Use custom.<adapter>.<field> only for current-state attributes that do not fit an existing canonical capability.",
    "Use metadata.vendor_specific for adapter metadata, opaque upstream identifiers, and descriptive fields that are not canonical device state.",
    "Do not duplicate the same meaning in both a canonical capability and vendor-specific fields.",
];

pub const CAPABILITY_OWNERSHIP: CapabilityOwnershipPolicy = CapabilityOwnershipPolicy {
    canonical_attribute_location: "device.attributes.<capability_key>",
    custom_attribute_prefix: CUSTOM_ATTRIBUTE_PREFIX,
    vendor_metadata_field: "metadata.vendor_specific",
    rules: &CAPABILITY_OWNERSHIP_RULES,
};

pub const WEATHER_CAPABILITIES: [CapabilityDefinition; 12] = [
    CapabilityDefinition {
        domain: "weather",
        key: TEMPERATURE_OUTDOOR,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Outdoor ambient temperature; unit is celsius or fahrenheit.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: TEMPERATURE_APPARENT,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Outdoor feels-like temperature.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: TEMPERATURE_HIGH,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Forecast or observed daily high temperature.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: TEMPERATURE_LOW,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Forecast or observed daily low temperature.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: WIND_SPEED,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Wind speed; unit is km/h, mph, m/s, or knots.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: WIND_DIRECTION,
        schema: CapabilitySchema::IntegerOrString,
        read_only: true,
        actions: &ACTION_GET,
        description: "Wind direction in degrees or compass point.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: WIND_GUST,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Wind gust speed.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: RAINFALL,
        schema: CapabilitySchema::Accumulation,
        read_only: true,
        actions: &ACTION_GET,
        description: "Rainfall accumulation with period.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: UV_INDEX,
        schema: CapabilitySchema::Number,
        read_only: true,
        actions: &ACTION_GET,
        description: "UV index.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: WEATHER_CONDITION,
        schema: CapabilitySchema::String,
        read_only: true,
        actions: &ACTION_GET,
        description: "Weather condition identifier.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: CLOUD_COVERAGE,
        schema: CapabilitySchema::Integer,
        read_only: true,
        actions: &ACTION_GET,
        description: "Cloud coverage percentage.",
    },
    CapabilityDefinition {
        domain: "weather",
        key: VISIBILITY,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Atmospheric visibility.",
    },
];

pub const LIGHT_CAPABILITIES: [CapabilityDefinition; 13] = [
    CapabilityDefinition {
        domain: "lighting",
        key: POWER,
        schema: CapabilitySchema::Enum(&POWER_VALUES),
        read_only: false,
        actions: &ACTION_POWER,
        description: "On/off power control.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: STATE,
        schema: CapabilitySchema::Enum(&AVAILABILITY_VALUES),
        read_only: true,
        actions: &ACTION_GET,
        description: "Connectivity / availability state.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: BRIGHTNESS,
        schema: CapabilitySchema::Percentage,
        read_only: false,
        actions: &ACTION_BRIGHTNESS,
        description: "Brightness level as a percentage (0-100).",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: COLOR_RGB,
        schema: CapabilitySchema::RgbColor,
        read_only: false,
        actions: &ACTION_SET,
        description: "RGB colour control.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: COLOR_HEX,
        schema: CapabilitySchema::HexColor,
        read_only: false,
        actions: &ACTION_SET,
        description: "Hex colour control (#000000-#ffffff).",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: COLOR_XY,
        schema: CapabilitySchema::XyColor,
        read_only: false,
        actions: &ACTION_SET,
        description: "CIE xy chromaticity colour control.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: COLOR_HS,
        schema: CapabilitySchema::HsColor,
        read_only: false,
        actions: &ACTION_SET,
        description: "Hue/saturation colour control.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: COLOR_TEMPERATURE,
        schema: CapabilitySchema::ColorTemperature,
        read_only: false,
        actions: &ACTION_COLOR_TEMPERATURE,
        description: "Colour temperature in mireds or kelvin.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: COLOR_MODE,
        schema: CapabilitySchema::Enum(&COLOR_MODE_VALUES),
        read_only: false,
        actions: &ACTION_SET,
        description: "Active colour mode of the light.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: EFFECT,
        schema: CapabilitySchema::Enum(&LIGHT_EFFECT_VALUES),
        read_only: false,
        actions: &ACTION_EFFECT,
        description: "Light effect control.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: TRANSITION,
        schema: CapabilitySchema::Number,
        read_only: false,
        actions: &ACTION_SET,
        description: "Transition duration in seconds.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: ILLUMINANCE,
        schema: CapabilitySchema::Integer,
        read_only: true,
        actions: &ACTION_GET,
        description: "Ambient light / illuminance sensor in lux.",
    },
    CapabilityDefinition {
        domain: "lighting",
        key: LED_INDICATION,
        schema: CapabilitySchema::Boolean,
        read_only: false,
        actions: &ACTION_LED_INDICATION,
        description: "LED status indicator on the device.",
    },
];

pub const SENSOR_CAPABILITIES: [CapabilityDefinition; 10] = [
    CapabilityDefinition {
        domain: "sensor",
        key: BATTERY,
        schema: CapabilitySchema::Percentage,
        read_only: true,
        actions: &ACTION_GET,
        description: "Battery level as a percentage (0-100).",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: TEMPERATURE,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Measured ambient temperature.",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: HUMIDITY,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Relative humidity measurement.",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: PRESSURE,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Atmospheric pressure measurement.",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: CO2,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Carbon dioxide concentration, typically in ppm.",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: AIR_QUALITY,
        schema: CapabilitySchema::Enum(&AIR_QUALITY_VALUES),
        read_only: true,
        actions: &ACTION_GET,
        description: "Categorized air quality state.",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: MOTION,
        schema: CapabilitySchema::Enum(&MOTION_VALUES),
        read_only: true,
        actions: &ACTION_GET,
        description: "Motion detection state.",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: CONTACT,
        schema: CapabilitySchema::Enum(&CONTACT_VALUES),
        read_only: true,
        actions: &ACTION_GET,
        description: "Contact sensor state.",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: OCCUPANCY,
        schema: CapabilitySchema::Enum(&OCCUPANCY_VALUES),
        read_only: true,
        actions: &ACTION_GET,
        description: "Occupancy or presence state.",
    },
    CapabilityDefinition {
        domain: "sensor",
        key: SMOKE,
        schema: CapabilitySchema::Boolean,
        read_only: true,
        actions: &ACTION_GET,
        description: "Smoke alarm state.",
    },
];

pub const SAFETY_CAPABILITIES: [CapabilityDefinition; 1] = [CapabilityDefinition {
    domain: "sensor",
    key: WATER_LEAK,
    schema: CapabilitySchema::Boolean,
    read_only: true,
    actions: &ACTION_GET,
    description: "Water leak detection state.",
}];

pub const CLIMATE_CAPABILITIES: [CapabilityDefinition; 6] = [
    CapabilityDefinition {
        domain: "climate",
        key: TARGET_TEMPERATURE,
        schema: CapabilitySchema::Measurement,
        read_only: false,
        actions: &ACTION_SET,
        description: "Desired temperature setpoint.",
    },
    CapabilityDefinition {
        domain: "climate",
        key: HVAC_MODE,
        schema: CapabilitySchema::Enum(&HVAC_MODE_VALUES),
        read_only: false,
        actions: &ACTION_SET,
        description: "Thermostat operating mode.",
    },
    CapabilityDefinition {
        domain: "climate",
        key: HVAC_STATE,
        schema: CapabilitySchema::Enum(&HVAC_STATE_VALUES),
        read_only: true,
        actions: &ACTION_GET,
        description: "Current heating or cooling activity.",
    },
    CapabilityDefinition {
        domain: "climate",
        key: FAN_MODE,
        schema: CapabilitySchema::String,
        read_only: false,
        actions: &ACTION_SET,
        description: "Fan operating mode.",
    },
    CapabilityDefinition {
        domain: "climate",
        key: SWING_MODE,
        schema: CapabilitySchema::String,
        read_only: false,
        actions: &ACTION_SET,
        description: "Swing or louver mode.",
    },
    CapabilityDefinition {
        domain: "climate",
        key: PRESET_MODE,
        schema: CapabilitySchema::String,
        read_only: false,
        actions: &ACTION_SET,
        description: "Thermostat preset mode such as away or sleep.",
    },
];

pub const ENERGY_CAPABILITIES: [CapabilityDefinition; 7] = [
    CapabilityDefinition {
        domain: "energy",
        key: POWER_CONSUMPTION,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Instantaneous power draw.",
    },
    CapabilityDefinition {
        domain: "energy",
        key: ENERGY_TOTAL,
        schema: CapabilitySchema::Accumulation,
        read_only: false,
        actions: &ACTION_RESET,
        description: "Cumulative energy usage with an explicit period such as lifetime.",
    },
    CapabilityDefinition {
        domain: "energy",
        key: ENERGY_TODAY,
        schema: CapabilitySchema::Accumulation,
        read_only: true,
        actions: &ACTION_GET,
        description: "Energy usage accumulated for the current day.",
    },
    CapabilityDefinition {
        domain: "energy",
        key: ENERGY_YESTERDAY,
        schema: CapabilitySchema::Accumulation,
        read_only: true,
        actions: &ACTION_GET,
        description: "Energy usage accumulated for the previous day.",
    },
    CapabilityDefinition {
        domain: "energy",
        key: ENERGY_MONTH,
        schema: CapabilitySchema::Accumulation,
        read_only: true,
        actions: &ACTION_GET,
        description: "Energy usage accumulated for the current month.",
    },
    CapabilityDefinition {
        domain: "energy",
        key: VOLTAGE,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Electrical voltage measurement.",
    },
    CapabilityDefinition {
        domain: "energy",
        key: CURRENT,
        schema: CapabilitySchema::Measurement,
        read_only: true,
        actions: &ACTION_GET,
        description: "Electrical current measurement.",
    },
];

pub const MEDIA_CAPABILITIES: [CapabilityDefinition; 6] = [
    CapabilityDefinition {
        domain: "media",
        key: VOLUME,
        schema: CapabilitySchema::Percentage,
        read_only: false,
        actions: &ACTION_VOLUME,
        description: "Volume level as a percentage (0-100).",
    },
    CapabilityDefinition {
        domain: "media",
        key: MUTED,
        schema: CapabilitySchema::Boolean,
        read_only: false,
        actions: &ACTION_MUTED,
        description: "Muted state for speakers or TVs.",
    },
    CapabilityDefinition {
        domain: "media",
        key: MEDIA_SOURCE,
        schema: CapabilitySchema::String,
        read_only: false,
        actions: &ACTION_SET,
        description: "Current input or source selection.",
    },
    CapabilityDefinition {
        domain: "media",
        key: MEDIA_TITLE,
        schema: CapabilitySchema::String,
        read_only: true,
        actions: &ACTION_GET,
        description: "Currently playing title.",
    },
    CapabilityDefinition {
        domain: "media",
        key: MEDIA_APP,
        schema: CapabilitySchema::String,
        read_only: false,
        actions: &ACTION_SET,
        description: "Selected media app, channel, or launcher target.",
    },
    CapabilityDefinition {
        domain: "media",
        key: MEDIA_PLAYBACK,
        schema: CapabilitySchema::Enum(&MEDIA_PLAYBACK_VALUES),
        read_only: false,
        actions: &ACTION_MEDIA_PLAYBACK,
        description: "Playback transport state and controls.",
    },
];

pub const ACCESS_CAPABILITIES: [CapabilityDefinition; 5] = [
    CapabilityDefinition {
        domain: "access",
        key: LOCK,
        schema: CapabilitySchema::Enum(&LOCK_VALUES),
        read_only: false,
        actions: &ACTION_LOCK,
        description: "Lock state for doors or windows.",
    },
    CapabilityDefinition {
        domain: "access",
        key: DOOR,
        schema: CapabilitySchema::Enum(&ENTRY_STATE_VALUES),
        read_only: false,
        actions: &ACTION_OPEN_CLOSE_STOP,
        description: "Door state and controls.",
    },
    CapabilityDefinition {
        domain: "access",
        key: GARAGE_DOOR,
        schema: CapabilitySchema::Enum(&ENTRY_STATE_VALUES),
        read_only: false,
        actions: &ACTION_OPEN_CLOSE_STOP,
        description: "Garage door state and controls.",
    },
    CapabilityDefinition {
        domain: "access",
        key: COVER_POSITION,
        schema: CapabilitySchema::Percentage,
        read_only: false,
        actions: &ACTION_COVER,
        description: "Cover, blind, or shade position as a percentage.",
    },
    CapabilityDefinition {
        domain: "access",
        key: COVER_TILT,
        schema: CapabilitySchema::Percentage,
        read_only: false,
        actions: &ACTION_COVER,
        description: "Cover slat tilt as a percentage.",
    },
];

/// Tracker capabilities — can be present on any device (phone, BLE tag, watch, router entry).
///
/// A device that carries these attributes is eligible to be linked to a [`crate::model::Person`]
/// as one of their location trackers.
pub const TRACKER_CAPABILITIES: [CapabilityDefinition; 6] = [
    CapabilityDefinition {
        domain: "tracker",
        key: TRACKER_TYPE,
        schema: CapabilitySchema::Enum(&TRACKER_TYPE_VALUES),
        read_only: false,
        actions: &ACTION_SET,
        description: "How the tracker determines location: gps (satellite/phone), stationary (wifi/router), or ble (Bluetooth beacon).",
    },
    CapabilityDefinition {
        domain: "tracker",
        key: TRACKER_STATE,
        schema: CapabilitySchema::String,
        read_only: false,
        actions: &ACTION_SET,
        description: "Current location state: 'home', 'not_home', or a zone id string.",
    },
    CapabilityDefinition {
        domain: "tracker",
        key: TRACKER_LATITUDE,
        schema: CapabilitySchema::Measurement,
        read_only: false,
        actions: &ACTION_SET,
        description: "GPS latitude in decimal degrees (GPS tracker type only).",
    },
    CapabilityDefinition {
        domain: "tracker",
        key: TRACKER_LONGITUDE,
        schema: CapabilitySchema::Measurement,
        read_only: false,
        actions: &ACTION_SET,
        description: "GPS longitude in decimal degrees (GPS tracker type only).",
    },
    CapabilityDefinition {
        domain: "tracker",
        key: TRACKER_ACCURACY,
        schema: CapabilitySchema::Measurement,
        read_only: false,
        actions: &ACTION_SET,
        description: "GPS fix accuracy in metres (GPS tracker type only).",
    },
    CapabilityDefinition {
        domain: "tracker",
        key: TRACKER_CONSIDER_HOME,
        schema: CapabilitySchema::Integer,
        read_only: false,
        actions: &ACTION_SET,
        description: "Seconds to wait before marking a stationary tracker as not_home after it was last seen home. Prevents false departures. Default 180.",
    },
];

pub const ALL_CAPABILITIES: [&[CapabilityDefinition]; 9] = [
    &WEATHER_CAPABILITIES,
    &LIGHT_CAPABILITIES,
    &SENSOR_CAPABILITIES,
    &SAFETY_CAPABILITIES,
    &CLIMATE_CAPABILITIES,
    &ENERGY_CAPABILITIES,
    &MEDIA_CAPABILITIES,
    &ACCESS_CAPABILITIES,
    &TRACKER_CAPABILITIES,
];

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

pub fn is_custom_attribute_key(key: &str) -> bool {
    let Some(suffix) = key.strip_prefix(CUSTOM_ATTRIBUTE_PREFIX) else {
        return false;
    };

    let segments = suffix.split('.').collect::<Vec<_>>();
    if segments.len() < 2 {
        return false;
    }

    segments.iter().all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    })
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
