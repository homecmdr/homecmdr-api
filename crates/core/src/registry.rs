use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;

use crate::bus::EventBus;
use crate::capability::{CapabilitySchema, weather_capability};
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
        validate_attributes(&device)?;

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
        if let Some(capability) = weather_capability(key) {
            validate_capability_value(capability.schema, value)
                .map_err(|message| anyhow::anyhow!("invalid value for capability '{key}' on '{}': {message}", device.id.0))?;
        }
    }

    Ok(())
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
