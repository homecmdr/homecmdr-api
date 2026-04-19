use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique identifier for a device.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// Unique identifier for a room.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoomId(pub String);

/// Unique identifier for a device group.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupId(pub String);

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
    /// Optional room assignment for this device.
    pub room_id: Option<RoomId>,
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

/// A logical room that devices can be assigned to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    /// Stable unique room identifier.
    pub id: RoomId,
    /// Human-readable room name.
    pub name: String,
}

/// A logical device group with explicit static membership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceGroup {
    /// Stable unique group identifier.
    pub id: GroupId,
    /// Human-readable group name.
    pub name: String,
    /// Explicit member devices by device ID.
    pub members: Vec<DeviceId>,
}

/// Typed attribute values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    Integer(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Array(Vec<AttributeValue>),
    Object(HashMap<String, AttributeValue>),
    Null,
}

/// Device metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// Adapter or subsystem that produced this device.
    pub source: String,
    /// Optional accuracy or confidence value.
    pub accuracy: Option<f64>,
    /// Vendor-defined extra metadata preserved as JSON.
    pub vendor_specific: HashMap<String, serde_json::Value>,
}
