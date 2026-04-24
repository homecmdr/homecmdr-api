use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::command::DeviceCommand;
use crate::model::{
    AttributeValue, Device, DeviceGroup, DeviceId, GroupId, Person, PersonId, Room, RoomId, Zone,
    ZoneId,
};

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
    /// Optional link to a [`Person`].  When set, requests authenticated with
    /// this key are attributed to that person (e.g. companion app location
    /// ingestion).
    pub person_id: Option<PersonId>,
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
        person_id: Option<&PersonId>,
    ) -> anyhow::Result<ApiKeyRecord>;
    async fn list_api_keys(&self) -> anyhow::Result<Vec<ApiKeyRecord>>;
    async fn list_api_keys_for_person(
        &self,
        person_id: &PersonId,
    ) -> anyhow::Result<Vec<ApiKeyRecord>>;
    async fn revoke_api_key(&self, id: i64) -> anyhow::Result<bool>;
    async fn lookup_api_key_by_hash(&self, key_hash: &str) -> anyhow::Result<Option<ApiKeyRecord>>;
    async fn touch_api_key(&self, id: i64) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Person history
// ---------------------------------------------------------------------------

/// A single recorded location-state transition for a person.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonHistoryEntry {
    pub id: i64,
    pub person_id: PersonId,
    /// Serialised `PersonState` tag, e.g. `"home"`, `"away"`, `"zone"`, `"room"`, `"unknown"`.
    pub state: String,
    /// Zone id when `state == "zone"`.
    pub zone_id: Option<ZoneId>,
    /// Tracker device that was the authoritative source.
    pub source_device_id: Option<DeviceId>,
    /// GPS latitude at time of transition (if available).
    pub latitude: Option<f64>,
    /// GPS longitude at time of transition (if available).
    pub longitude: Option<f64>,
    pub recorded_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// PersonStore trait
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait PersonStore: Send + Sync + 'static {
    // --- persons ---
    async fn upsert_person(&self, person: &Person) -> anyhow::Result<()>;
    async fn load_person(&self, id: &PersonId) -> anyhow::Result<Option<Person>>;
    async fn load_all_persons(&self) -> anyhow::Result<Vec<Person>>;
    async fn delete_person(&self, id: &PersonId) -> anyhow::Result<bool>;
    /// Replace the full ordered list of tracker device IDs for a person.
    async fn update_person_trackers(
        &self,
        person_id: &PersonId,
        trackers: &[DeviceId],
    ) -> anyhow::Result<()>;

    // --- zones ---
    async fn upsert_zone(&self, zone: &Zone) -> anyhow::Result<()>;
    async fn load_zone(&self, id: &ZoneId) -> anyhow::Result<Option<Zone>>;
    async fn load_all_zones(&self) -> anyhow::Result<Vec<Zone>>;
    async fn delete_zone(&self, id: &ZoneId) -> anyhow::Result<bool>;

    // --- person history ---
    async fn record_person_history(&self, entry: &PersonHistoryEntry) -> anyhow::Result<()>;
    async fn load_person_history(
        &self,
        person_id: &PersonId,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> anyhow::Result<Vec<PersonHistoryEntry>>;
    /// Prune person history rows older than the configured retention window.
    async fn prune_person_history(&self) -> anyhow::Result<()>;
}
