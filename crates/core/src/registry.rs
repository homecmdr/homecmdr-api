use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;

use crate::bus::EventBus;
use crate::capability::{CapabilitySchema, capability_definition};
use crate::event::Event;
use crate::model::{AttributeValue, Device, DeviceId, Room, RoomId};

#[derive(Debug, Clone)]
pub struct DeviceRegistry {
    bus: EventBus,
    devices: Arc<RwLock<HashMap<DeviceId, Device>>>,
    rooms: Arc<RwLock<HashMap<RoomId, Room>>>,
}

impl DeviceRegistry {
    pub fn new(bus: EventBus) -> Self {
        Self {
            bus,
            devices: Arc::new(RwLock::new(HashMap::new())),
            rooms: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn upsert(&self, device: Device) -> Result<()> {
        validate_device(&device)?;
        validate_room_assignment(self, &device)?;

        let event = {
            let mut devices = write_guard(&self.devices);
            let id = device.id.clone();
            match devices.get_mut(&id) {
                None => {
                    devices.insert(id, device.clone());
                    Some(Event::DeviceAdded { device })
                }
                Some(existing) => {
                    let state_changed = device.kind != existing.kind
                        || device.attributes != existing.attributes
                        || device.metadata != existing.metadata;

                    if state_changed {
                        *existing = device.clone();
                        Some(Event::DeviceStateChanged {
                            id,
                            attributes: device.attributes.clone(),
                        })
                    } else if device.last_seen != existing.last_seen {
                        existing.last_seen = device.last_seen;
                        Some(Event::DeviceSeen {
                            id,
                            last_seen: existing.last_seen,
                        })
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(event) = event {
            self.bus.publish(event);
        }
        Ok(())
    }

    pub fn restore(&self, devices: Vec<Device>) -> Result<()> {
        let mut restored = HashMap::with_capacity(devices.len());

        for device in devices {
            validate_device(&device)?;
            validate_room_assignment(self, &device)?;
            restored.insert(device.id.clone(), device);
        }

        let mut current = write_guard(&self.devices);
        *current = restored;

        Ok(())
    }

    pub fn restore_rooms(&self, rooms: Vec<Room>) {
        let mut restored = HashMap::with_capacity(rooms.len());

        for room in rooms {
            restored.insert(room.id.clone(), room);
        }

        let mut current = write_guard(&self.rooms);
        *current = restored;
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

    pub async fn upsert_room(&self, room: Room) {
        let event = {
            let mut rooms = write_guard(&self.rooms);
            match rooms.insert(room.id.clone(), room.clone()) {
                Some(_) => Event::RoomUpdated { room },
                None => Event::RoomAdded { room },
            }
        };

        self.bus.publish(event);
    }

    pub async fn remove_room(&self, id: &RoomId) -> bool {
        let removed = {
            let mut rooms = write_guard(&self.rooms);
            rooms.remove(id).is_some()
        };

        if !removed {
            return false;
        }

        let affected_devices = {
            let mut devices = write_guard(&self.devices);
            let mut changed = Vec::new();

            for device in devices.values_mut() {
                if device.room_id.as_ref() == Some(id) {
                    device.room_id = None;
                    changed.push(device.id.clone());
                }
            }

            changed
        };

        self.bus.publish(Event::RoomRemoved { id: id.clone() });

        for device_id in affected_devices {
            self.bus.publish(Event::DeviceRoomChanged {
                id: device_id,
                room_id: None,
            });
        }

        true
    }

    pub fn get_room(&self, id: &RoomId) -> Option<Room> {
        let rooms = read_guard(&self.rooms);
        rooms.get(id).cloned()
    }

    pub fn list_rooms(&self) -> Vec<Room> {
        let rooms = read_guard(&self.rooms);
        rooms.values().cloned().collect()
    }

    pub fn list_devices_in_room(&self, room_id: &RoomId) -> Vec<Device> {
        let devices = read_guard(&self.devices);
        devices
            .values()
            .filter(|device| device.room_id.as_ref() == Some(room_id))
            .cloned()
            .collect()
    }

    pub async fn assign_device_to_room(&self, device_id: &DeviceId, room_id: Option<RoomId>) -> Result<bool> {
        if let Some(room_id) = &room_id {
            let rooms = read_guard(&self.rooms);
            if !rooms.contains_key(room_id) {
                anyhow::bail!("room '{}' not found", room_id.0);
            }
        }

        let changed = {
            let mut devices = write_guard(&self.devices);
            let Some(device) = devices.get_mut(device_id) else {
                return Ok(false);
            };

            if device.room_id == room_id {
                return Ok(true);
            }

            device.room_id = room_id.clone();
            true
        };

        if changed {
            self.bus.publish(Event::DeviceRoomChanged {
                id: device_id.clone(),
                room_id,
            });
        }

        Ok(true)
    }
}

fn validate_room_assignment(registry: &DeviceRegistry, device: &Device) -> Result<()> {
    let Some(room_id) = &device.room_id else {
        return Ok(());
    };

    let rooms = read_guard(&registry.rooms);
    if rooms.contains_key(room_id) {
        Ok(())
    } else {
        anyhow::bail!("device '{}' references unknown room '{}'", device.id.0, room_id.0)
    }
}

fn validate_attributes(device: &Device) -> Result<()> {
    for (key, value) in &device.attributes {
        if let Some(capability) = capability_definition(key) {
            validate_capability_attribute_value(capability.schema, value)
                .map_err(|message| anyhow::anyhow!("invalid value for capability '{key}' on '{}': {message}", device.id.0))?;
        }
    }

    Ok(())
}

pub fn validate_device(device: &Device) -> Result<()> {
    validate_attributes(device)
}

pub fn validate_capability_attribute_value(
    schema: CapabilitySchema,
    value: &AttributeValue,
) -> std::result::Result<(), &'static str> {
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
        "kelvin" if (2200..=7000).contains(&temp_value) => Ok(()),
        "mireds" => Err("color temperature in mireds must be between 153 and 500"),
        "kelvin" => Err("color temperature in kelvin must be between 2200 and 7000"),
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
