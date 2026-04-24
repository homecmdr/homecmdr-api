use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Person & zone identity types
// ---------------------------------------------------------------------------

/// Unique identifier for a person.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PersonId(pub String);

/// Unique identifier for a geographic zone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ZoneId(pub String);

/// The derived location state of a person.
///
/// State is never stored directly — it is computed by [`crate::person_registry::PersonRegistry`]
/// from the current attributes of that person's linked tracker devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersonState {
    /// The person is inside the home zone.
    Home,
    /// The person is outside the home zone and not in any other named zone.
    Away,
    /// The person is inside a named zone (e.g. "work", "gym").
    Zone { zone_id: ZoneId },
    /// The person is in a specific room (future BLE indoor tracking — pre-modelled).
    Room { room_id: RoomId },
    /// Location state is not yet known (no tracker has reported).
    Unknown,
}

impl Default for PersonState {
    fn default() -> Self {
        Self::Unknown
    }
}

/// A person tracked by the system.
///
/// A person's `state` is derived from one or more linked tracker devices.
/// Multiple trackers (phone GPS, BLE tag, watch) can be linked to one person;
/// the registry applies priority rules to compute the authoritative state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    /// Stable unique person identifier.
    pub id: PersonId,
    /// Display name.
    pub name: String,
    /// Optional URL or file path to a profile picture.
    pub picture: Option<String>,
    /// Ordered list of tracker device IDs linked to this person.
    ///
    /// The registry evaluates all linked trackers and applies HA-style priority:
    /// stationary-home wins over GPS, which wins over stationary-not-home.
    pub trackers: Vec<DeviceId>,
    /// Current derived location state.
    pub state: PersonState,
    /// The tracker device that is currently the authoritative source for `state`.
    pub state_source: Option<DeviceId>,
    /// Last known GPS latitude (from a GPS tracker).
    pub latitude: Option<f64>,
    /// Last known GPS longitude (from a GPS tracker).
    pub longitude: Option<f64>,
    /// Timestamp of the most recent state derivation.
    pub updated_at: DateTime<Utc>,
}

/// A named geographic zone used for presence detection.
///
/// Zones are circles defined by a centre point and radius in metres.
/// The `home` zone (ID = `"home"`) is auto-created from `[locale]` config
/// at startup and cannot be deleted via the API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    /// Stable unique zone identifier.
    pub id: ZoneId,
    /// Human-readable zone name (e.g. "Home", "Work", "Gym").
    pub name: String,
    /// Latitude of the zone centre.
    pub latitude: f64,
    /// Longitude of the zone centre.
    pub longitude: f64,
    /// Radius of the zone circle in metres (default 100).
    pub radius_meters: f64,
    /// Optional icon name (e.g. `"mdi:home"`, `"mdi:briefcase"`).
    pub icon: Option<String>,
    /// When `true` the zone is hidden from the map/UI but still usable in automations.
    pub passive: bool,
}

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
