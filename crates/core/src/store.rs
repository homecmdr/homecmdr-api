use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::command::DeviceCommand;
use crate::model::{AttributeValue, Device, DeviceGroup, DeviceId, GroupId, Room, RoomId};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationRuntimeState {
    pub updated_at: DateTime<Utc>,
    pub automation_id: String,
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub last_trigger_fingerprint: Option<String>,
    pub last_scheduled_at: Option<DateTime<Utc>>,
}

#[async_trait::async_trait]
pub trait DeviceStore: Send + Sync + 'static {
    async fn load_all_devices(&self) -> anyhow::Result<Vec<Device>>;
    async fn load_all_rooms(&self) -> anyhow::Result<Vec<Room>>;
    async fn load_all_groups(&self) -> anyhow::Result<Vec<DeviceGroup>>;
    async fn save_device(&self, device: &Device) -> anyhow::Result<()>;
    async fn save_room(&self, room: &Room) -> anyhow::Result<()>;
    async fn save_group(&self, group: &DeviceGroup) -> anyhow::Result<()>;
    async fn delete_device(&self, id: &DeviceId) -> anyhow::Result<()>;
    async fn delete_room(&self, id: &RoomId) -> anyhow::Result<()>;
    async fn delete_group(&self, id: &GroupId) -> anyhow::Result<()>;
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
    async fn load_automation_runtime_state(
        &self,
        automation_id: &str,
    ) -> anyhow::Result<Option<AutomationRuntimeState>>;
    async fn save_automation_runtime_state(
        &self,
        state: &AutomationRuntimeState,
    ) -> anyhow::Result<()>;
    /// Prune stale history rows according to the store's configured retention
    /// window.  Intended to be called from a background timer task; impls that
    /// already prune inline may choose to make this a no-op.
    async fn prune_history(&self) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyRole {
    Read,
    Write,
    Admin,
    Automation,
}

impl ApiKeyRole {
    /// Returns true when `self` grants at least the privileges of `required`.
    pub fn satisfies(self, required: ApiKeyRole) -> bool {
        use ApiKeyRole::*;
        match required {
            Read => matches!(self, Read | Write | Admin | Automation),
            Write => matches!(self, Write | Admin | Automation),
            Admin => matches!(self, Admin),
            Automation => matches!(self, Automation | Admin),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: i64,
    pub key_hash: String,
    pub label: String,
    pub role: ApiKeyRole,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[async_trait::async_trait]
pub trait ApiKeyStore: Send + Sync + 'static {
    async fn create_api_key(
        &self,
        key_hash: &str,
        label: &str,
        role: ApiKeyRole,
    ) -> anyhow::Result<ApiKeyRecord>;
    async fn list_api_keys(&self) -> anyhow::Result<Vec<ApiKeyRecord>>;
    async fn revoke_api_key(&self, id: i64) -> anyhow::Result<bool>;
    async fn lookup_api_key_by_hash(&self, key_hash: &str) -> anyhow::Result<Option<ApiKeyRecord>>;
    async fn touch_api_key(&self, id: i64) -> anyhow::Result<()>;
}
