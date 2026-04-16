use serde::{Deserialize, Serialize};

use crate::model::{Attributes, Device, DeviceId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    DeviceStateChanged {
        id: DeviceId,
        attributes: Attributes,
    },
    DeviceAdded {
        device: Device,
    },
    DeviceRemoved {
        id: DeviceId,
    },
    AdapterStarted {
        adapter: String,
    },
    SystemError {
        message: String,
    },
}
