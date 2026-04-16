use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique identifier for a device.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// The kind of device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Sensor,
    Light,
    Switch,
    Virtual,
}

/// Map of device attribute names to typed values.
pub type Attributes = HashMap<String, AttributeValue>;

/// A single device in the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// Stable unique device identifier.
    pub id: DeviceId,
    /// High-level device category.
    pub kind: DeviceKind,
    /// Current device state values.
    pub attributes: Attributes,
    /// Source and device-specific metadata.
    pub metadata: Metadata,
    /// Timestamp of the most recent update.
    pub updated_at: DateTime<Utc>,
    /// Timestamp of the most recent successful observation.
    pub last_seen: DateTime<Utc>,
}

/// Typed attribute values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    Integer(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Object(HashMap<String, AttributeValue>),
    Null,
}

/// Device metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// Adapter or subsystem that produced this device.
    pub source: String,
    /// Optional physical or logical device location.
    pub location: Option<String>,
    /// Optional accuracy or confidence value.
    pub accuracy: Option<f64>,
    /// Vendor-defined extra metadata preserved as JSON.
    pub vendor_specific: HashMap<String, serde_json::Value>,
}
