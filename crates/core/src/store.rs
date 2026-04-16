use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::command::DeviceCommand;
use crate::model::{AttributeValue, Device, DeviceId, Room, RoomId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceHistoryEntry {
    pub observed_at: DateTime<Utc>,
    pub device: Device,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributeHistoryEntry {
    pub observed_at: DateTime<Utc>,
    pub device_id: DeviceId,
    pub attribute: String,
    pub value: AttributeValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandAuditEntry {
    pub recorded_at: DateTime<Utc>,
    pub source: String,
    pub room_id: Option<RoomId>,
    pub device_id: DeviceId,
    pub command: DeviceCommand,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneStepResult {
    pub target: String,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneExecutionHistoryEntry {
    pub executed_at: DateTime<Utc>,
    pub scene_id: String,
    pub status: String,
    pub error: Option<String>,
    pub results: Vec<SceneStepResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationExecutionHistoryEntry {
    pub executed_at: DateTime<Utc>,
    pub automation_id: String,
    pub trigger_payload: AttributeValue,
    pub status: String,
    pub duration_ms: i64,
    pub error: Option<String>,
    pub results: Vec<SceneStepResult>,
}

#[async_trait::async_trait]
pub trait DeviceStore: Send + Sync + 'static {
    async fn load_all_devices(&self) -> anyhow::Result<Vec<Device>>;
    async fn load_all_rooms(&self) -> anyhow::Result<Vec<Room>>;
    async fn save_device(&self, device: &Device) -> anyhow::Result<()>;
    async fn save_room(&self, room: &Room) -> anyhow::Result<()>;
    async fn delete_device(&self, id: &DeviceId) -> anyhow::Result<()>;
    async fn delete_room(&self, id: &RoomId) -> anyhow::Result<()>;
    async fn load_device_history(
        &self,
        id: &DeviceId,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> anyhow::Result<Vec<DeviceHistoryEntry>>;
    async fn load_attribute_history(
        &self,
        id: &DeviceId,
        attribute: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> anyhow::Result<Vec<AttributeHistoryEntry>>;
    async fn save_command_audit(&self, entry: &CommandAuditEntry) -> anyhow::Result<()>;
    async fn load_command_audit(
        &self,
        device_id: Option<&DeviceId>,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> anyhow::Result<Vec<CommandAuditEntry>>;
    async fn save_scene_execution(&self, entry: &SceneExecutionHistoryEntry) -> anyhow::Result<()>;
    async fn load_scene_history(
        &self,
        scene_id: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> anyhow::Result<Vec<SceneExecutionHistoryEntry>>;
    async fn save_automation_execution(
        &self,
        entry: &AutomationExecutionHistoryEntry,
    ) -> anyhow::Result<()>;
    async fn load_automation_history(
        &self,
        automation_id: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> anyhow::Result<Vec<AutomationExecutionHistoryEntry>>;
}
