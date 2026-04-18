use serde::{Deserialize, Serialize};

use crate::model::{Attributes, Device, DeviceGroup, DeviceId, GroupId, Room, RoomId};

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
    SystemError {
        message: String,
    },
}
