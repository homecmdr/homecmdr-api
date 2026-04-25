use serde::{Deserialize, Serialize};

use crate::command::DeviceCommand;
use crate::model::{
    Attributes, Device, DeviceGroup, DeviceId, GroupId, Person, PersonId, PersonState, Room,
    RoomId, Zone, ZoneId,
};

// Carries a filename and a human-readable message for a file that failed to
// load during a hot-reload cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadError {
    pub file: String,
    pub message: String,
}

// Every meaningful state change in the system is published as one of these
// variants on the EventBus.  Automations, the SSE stream, the persistence
// workers, and the Lua runtime all subscribe and react to these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    // ── Device lifecycle ────────────────────────────────────────────────────

    /// Fired when a device's attributes, kind, or metadata changes.
    DeviceStateChanged {
        id: DeviceId,
        attributes: Attributes,
        previous_attributes: Attributes,
    },
    /// Fired the first time a device is seen (new registration).
    DeviceAdded {
        device: Device,
    },
    /// Fired when a device is deleted from the registry.
    DeviceRemoved {
        id: DeviceId,
    },
    /// Fired when a device reports a heartbeat without changing its attributes.
    DeviceSeen {
        id: DeviceId,
        last_seen: chrono::DateTime<chrono::Utc>,
    },

    // ── Room lifecycle ───────────────────────────────────────────────────────

    RoomAdded {
        room: Room,
    },
    RoomUpdated {
        room: Room,
    },
    RoomRemoved {
        id: RoomId,
    },

    // ── Group lifecycle ──────────────────────────────────────────────────────

    GroupAdded {
        group: DeviceGroup,
    },
    GroupUpdated {
        group: DeviceGroup,
    },
    GroupRemoved {
        id: GroupId,
    },
    /// Fired whenever the ordered member list of a group changes.
    GroupMembersChanged {
        id: GroupId,
        members: Vec<DeviceId>,
    },
    /// Fired when a device is moved to a different room (or unassigned).
    DeviceRoomChanged {
        id: DeviceId,
        room_id: Option<RoomId>,
    },
    /// Fired once an adapter finishes its startup routine.
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

    // ── Catalog / hot-reload lifecycle ──────────────────────────────────────

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
    /// Fired when an adapter or background task exits with an unrecoverable error.
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
