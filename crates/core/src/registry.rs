use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;

use crate::bus::EventBus;
use crate::capability::{CapabilitySchema, capability_definition};
use crate::event::Event;
use crate::model::{AttributeValue, Device, DeviceId};

#[derive(Debug, Clone)]
pub struct DeviceRegistry {
    bus: EventBus,
    devices: Arc<RwLock<HashMap<DeviceId, Device>>>,
}

impl DeviceRegistry {
    pub fn new(bus: EventBus) -> Self {
        Self {
            bus,
            devices: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn upsert(&self, device: Device) -> Result<()> {
        validate_device(&device)?;

        let event = {
            let mut devices = write_guard(&self.devices);
            let id = device.id.clone();
            let is_new = devices.insert(id.clone(), device.clone()).is_none();

            if is_new {
                Event::DeviceAdded { device }
            } else {
                Event::DeviceStateChanged {
                    id,
                    attributes: device.attributes.clone(),
                }
            }
        };

        self.bus.publish(event);
        Ok(())
    }

    pub fn restore(&self, devices: Vec<Device>) -> Result<()> {
        let mut restored = HashMap::with_capacity(devices.len());

        for device in devices {
            validate_device(&device)?;
            restored.insert(device.id.clone(), device);
        }

        let mut current = write_guard(&self.devices);
        *current = restored;

        Ok(())
    }

    pub async fn remove(&self, id: &DeviceId) -> bool {
        let removed = {
            let mut devices = write_guard(&self.devices);
            devices.remove(id).is_some()
        };

        if removed {
            self.bus.publish(Event::DeviceRemoved { id: id.clone() });
        }

        removed
    }

    pub fn get(&self, id: &DeviceId) -> Option<Device> {
        let devices = read_guard(&self.devices);
        devices.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Device> {
        let devices = read_guard(&self.devices);
        devices.values().cloned().collect()
    }
}

fn validate_attributes(device: &Device) -> Result<()> {
    for (key, value) in &device.attributes {
        if let Some(capability) = capability_definition(key) {
            validate_capability_value(capability.schema, value)
                .map_err(|message| anyhow::anyhow!("invalid value for capability '{key}' on '{}': {message}", device.id.0))?;
        }
    }

    Ok(())
}

pub fn validate_device(device: &Device) -> Result<()> {
    validate_attributes(device)
}

fn validate_capability_value(schema: CapabilitySchema, value: &AttributeValue) -> std::result::Result<(), &'static str> {
    match schema {
        CapabilitySchema::Measurement => validate_measurement(value),
        CapabilitySchema::Accumulation => validate_accumulation(value),
        CapabilitySchema::Number => match value {
            AttributeValue::Float(_) | AttributeValue::Integer(_) => Ok(()),
            _ => Err("expected number"),
        },
        CapabilitySchema::Integer => match value {
            AttributeValue::Integer(_) => Ok(()),
            _ => Err("expected integer"),
        },
        CapabilitySchema::String => match value {
            AttributeValue::Text(_) => Ok(()),
            _ => Err("expected string"),
        },
        CapabilitySchema::IntegerOrString => match value {
            AttributeValue::Integer(_) | AttributeValue::Text(_) => Ok(()),
            _ => Err("expected integer or string"),
        },
        CapabilitySchema::Boolean => match value {
            AttributeValue::Bool(_) => Ok(()),
            _ => Err("expected boolean"),
        },
        CapabilitySchema::Percentage => validate_percentage(value),
        CapabilitySchema::RgbColor => validate_rgb_color(value),
        CapabilitySchema::HexColor => validate_hex_color(value),
        CapabilitySchema::XyColor => validate_xy_color(value),
        CapabilitySchema::HsColor => validate_hs_color(value),
        CapabilitySchema::ColorTemperature => validate_color_temperature(value),
        CapabilitySchema::Enum(options) => validate_enum(value, options),
    }
}

fn validate_percentage(value: &AttributeValue) -> std::result::Result<(), &'static str> {
    match value {
        AttributeValue::Integer(level) if (0..=100).contains(level) => Ok(()),
        AttributeValue::Integer(_) => Err("expected integer percentage between 0 and 100"),
        _ => Err("expected integer percentage between 0 and 100"),
    }
}

fn validate_rgb_color(value: &AttributeValue) -> std::result::Result<(), &'static str> {
    let fields = expect_object(value, "expected rgb color object")?;
    validate_integer_field_in_range(fields, "r", 0, 255, "rgb color requires integer 'r' between 0 and 255")?;
    validate_integer_field_in_range(fields, "g", 0, 255, "rgb color requires integer 'g' between 0 and 255")?;
    validate_integer_field_in_range(fields, "b", 0, 255, "rgb color requires integer 'b' between 0 and 255")?;
    Ok(())
}

fn validate_hex_color(value: &AttributeValue) -> std::result::Result<(), &'static str> {
    match value {
        AttributeValue::Text(hex)
            if hex.len() == 7
                && hex.starts_with('#')
                && hex.chars().skip(1).all(|ch| ch.is_ascii_hexdigit()) =>
        {
            Ok(())
        }
        AttributeValue::Text(_) => Err("expected hex color string like '#ff8800'"),
        _ => Err("expected hex color string like '#ff8800'"),
    }
}

fn validate_xy_color(value: &AttributeValue) -> std::result::Result<(), &'static str> {
    let fields = expect_object(value, "expected xy color object")?;
    validate_number_field_in_range(fields, "x", 0.0, 1.0, "xy color requires number 'x' between 0 and 1")?;
    validate_number_field_in_range(fields, "y", 0.0, 1.0, "xy color requires number 'y' between 0 and 1")?;
    Ok(())
}

fn validate_hs_color(value: &AttributeValue) -> std::result::Result<(), &'static str> {
    let fields = expect_object(value, "expected hs color object")?;
    validate_integer_field_in_range(fields, "hue", 0, 360, "hs color requires integer 'hue' between 0 and 360")?;
    validate_integer_field_in_range(
        fields,
        "saturation",
        0,
        100,
        "hs color requires integer 'saturation' between 0 and 100",
    )?;
    Ok(())
}

fn validate_color_temperature(value: &AttributeValue) -> std::result::Result<(), &'static str> {
    let fields = expect_object(value, "expected color temperature object")?;

    let temp_value = match fields.get("value") {
        Some(AttributeValue::Integer(value)) if *value >= 0 => *value,
        _ => return Err("color temperature requires integer 'value' >= 0"),
    };

    let unit = match fields.get("unit") {
        Some(AttributeValue::Text(unit)) => unit.as_str(),
        _ => return Err("color temperature requires string 'unit' of 'mireds' or 'kelvin'"),
    };

    match unit {
        "mireds" if (153..=500).contains(&temp_value) => Ok(()),
        "kelvin" if (2200..=6500).contains(&temp_value) => Ok(()),
        "mireds" => Err("color temperature in mireds must be between 153 and 500"),
        "kelvin" => Err("color temperature in kelvin must be between 2200 and 6500"),
        _ => Err("color temperature requires string 'unit' of 'mireds' or 'kelvin'"),
    }
}

fn validate_enum(value: &AttributeValue, options: &'static [&'static str]) -> std::result::Result<(), &'static str> {
    match value {
        AttributeValue::Text(text) if options.contains(&text.as_str()) => Ok(()),
        AttributeValue::Text(_) => Err("expected one of the declared enum values"),
        _ => Err("expected string enum value"),
    }
}

fn expect_object<'a>(
    value: &'a AttributeValue,
    message: &'static str,
) -> std::result::Result<&'a HashMap<String, AttributeValue>, &'static str> {
    match value {
        AttributeValue::Object(fields) => Ok(fields),
        _ => Err(message),
    }
}

fn validate_integer_field_in_range(
    fields: &HashMap<String, AttributeValue>,
    key: &str,
    min: i64,
    max: i64,
    message: &'static str,
) -> std::result::Result<(), &'static str> {
    match fields.get(key) {
        Some(AttributeValue::Integer(value)) if (min..=max).contains(value) => Ok(()),
        _ => Err(message),
    }
}

fn validate_number_field_in_range(
    fields: &HashMap<String, AttributeValue>,
    key: &str,
    min: f64,
    max: f64,
    message: &'static str,
) -> std::result::Result<(), &'static str> {
    let value = match fields.get(key) {
        Some(AttributeValue::Float(value)) => *value,
        Some(AttributeValue::Integer(value)) => *value as f64,
        _ => return Err(message),
    };

    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(message)
    }
}

fn validate_measurement(value: &AttributeValue) -> std::result::Result<(), &'static str> {
    match value {
        AttributeValue::Object(fields) => {
            if !matches!(fields.get("value"), Some(AttributeValue::Float(_) | AttributeValue::Integer(_))) {
                return Err("measurement requires numeric 'value'");
            }
            if !matches!(fields.get("unit"), Some(AttributeValue::Text(_))) {
                return Err("measurement requires string 'unit'");
            }

            Ok(())
        }
        _ => Err("expected measurement object"),
    }
}

fn validate_accumulation(value: &AttributeValue) -> std::result::Result<(), &'static str> {
    match value {
        AttributeValue::Object(fields) => {
            if !matches!(fields.get("value"), Some(AttributeValue::Float(_) | AttributeValue::Integer(_))) {
                return Err("accumulation requires numeric 'value'");
            }
            if !matches!(fields.get("unit"), Some(AttributeValue::Text(_))) {
                return Err("accumulation requires string 'unit'");
            }
            if !matches!(fields.get("period"), Some(AttributeValue::Text(_))) {
                return Err("accumulation requires string 'period'");
            }

            Ok(())
        }
        _ => Err("expected accumulation object"),
    }
}

fn read_guard<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_guard<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
