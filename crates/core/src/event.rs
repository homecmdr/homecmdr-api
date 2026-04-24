use serde::{Deserialize, Serialize};

use crate::command::DeviceCommand;
use crate::model::{
    Attributes, Device, DeviceGroup, DeviceId, GroupId, Person, PersonId, PersonState, Room,
    RoomId, Zone, ZoneId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadError {
    pub file: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    DeviceStateChanged {
        id: DeviceId,
        attributes: Attributes,
        previous_attributes: Attributes,
    },
    DeviceAdded {
        device: Device,
    },
    DeviceRemoved {
        id: DeviceId,
    },
    DeviceSeen {
        id: DeviceId,
        last_seen: chrono::DateTime<chrono::Utc>,
    },
    RoomAdded {
        room: Room,
    },
    RoomUpdated {
        room: Room,
    },
    RoomRemoved {
        id: RoomId,
    },
    GroupAdded {
        group: DeviceGroup,
    },
    GroupUpdated {
        group: DeviceGroup,
    },
    GroupRemoved {
        id: GroupId,
    },
    GroupMembersChanged {
        id: GroupId,
        members: Vec<DeviceId>,
    },
    DeviceRoomChanged {
        id: DeviceId,
        room_id: Option<RoomId>,
    },
    AdapterStarted {
        adapter: String,
    },
    /// A command has been dispatched to a device whose adapter is not managed
    /// in-process (i.e. an IPC adapter).  IPC adapters subscribe to the
    /// WebSocket `/events` stream and handle this event themselves.
    DeviceCommandDispatched {
        id: DeviceId,
        command: DeviceCommand,
    },
    SceneCatalogReloadStarted,
    SceneCatalogReloaded {
        loaded_count: usize,
        duration_ms: u64,
    },
    SceneCatalogReloadFailed {
        duration_ms: u64,
        errors: Vec<ReloadError>,
    },
    AutomationCatalogReloadStarted,
    AutomationCatalogReloaded {
        loaded_count: usize,
        duration_ms: u64,
    },
    AutomationCatalogReloadFailed {
        duration_ms: u64,
        errors: Vec<ReloadError>,
    },
    ScriptsReloadStarted,
    ScriptsReloaded {
        loaded_count: usize,
        duration_ms: u64,
    },
    ScriptsReloadFailed {
        duration_ms: u64,
        errors: Vec<ReloadError>,
    },
    PluginCatalogReloaded {
        loaded_count: usize,
        duration_ms: u64,
    },
    PluginCatalogReloadFailed {
        duration_ms: u64,
        errors: Vec<ReloadError>,
    },
    SystemError {
        message: String,
    },
    // -----------------------------------------------------------------------
    // Person events
    // -----------------------------------------------------------------------
    /// Fired when a person's derived location state changes.
    PersonStateChanged {
        person_id: PersonId,
        person_name: String,
        from: PersonState,
        to: PersonState,
        /// The tracker device that provided the authoritative reading.
        source_device: Option<DeviceId>,
    },
    PersonAdded {
        person: Person,
    },
    PersonUpdated {
        person: Person,
    },
    PersonRemoved {
        person_id: PersonId,
    },
    /// Fired when the last person at home leaves (all persons are now away).
    AllPersonsAway,
    /// Fired when the first person arrives home (at least one person is now home).
    AnyPersonHome {
        person_id: PersonId,
        person_name: String,
    },
    // -----------------------------------------------------------------------
    // Zone events
    // -----------------------------------------------------------------------
    ZoneAdded {
        zone: Zone,
    },
    ZoneUpdated {
        zone: Zone,
    },
    ZoneRemoved {
        zone_id: ZoneId,
    },
}
