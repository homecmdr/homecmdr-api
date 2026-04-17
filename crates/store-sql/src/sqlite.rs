use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use smart_home_core::model::{
    AttributeValue, Attributes, Device, DeviceId, DeviceKind, Metadata, Room, RoomId,
};
use smart_home_core::store::{
    AttributeHistoryEntry, AutomationExecutionHistoryEntry, AutomationRuntimeState,
    CommandAuditEntry, DeviceHistoryEntry, DeviceStore, SceneExecutionHistoryEntry,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

const CREATE_SCHEMA_METADATA_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
)
"#;

const CREATE_DEVICES_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS devices (
    device_id TEXT PRIMARY KEY,
    room_id TEXT REFERENCES rooms(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    attributes_json TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_seen TEXT NOT NULL
)
"#;

const CREATE_ROOMS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS rooms (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
)
"#;

const CREATE_DEVICE_HISTORY_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS device_history (
    device_id TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    device_json TEXT NOT NULL,
    PRIMARY KEY (device_id, observed_at)
)
"#;

const CREATE_ATTRIBUTE_HISTORY_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS attribute_history (
    device_id TEXT NOT NULL,
    attribute TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    value_json TEXT NOT NULL,
    PRIMARY KEY (device_id, attribute, observed_at)
)
"#;

const CREATE_DEVICE_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_device_history_device_time
ON device_history(device_id, observed_at DESC)
"#;

const CREATE_ATTRIBUTE_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_attribute_history_device_attr_time
ON attribute_history(device_id, attribute, observed_at DESC)
"#;

const CREATE_COMMAND_AUDIT_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS command_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    recorded_at TEXT NOT NULL,
    source TEXT NOT NULL,
    room_id TEXT,
    device_id TEXT NOT NULL,
    command_json TEXT NOT NULL,
    status TEXT NOT NULL,
    message TEXT
)
"#;

const CREATE_COMMAND_AUDIT_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_command_audit_device_time
ON command_audit(device_id, recorded_at DESC)
"#;

const CREATE_SCENE_HISTORY_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS scene_execution_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    executed_at TEXT NOT NULL,
    scene_id TEXT NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    results_json TEXT NOT NULL
)
"#;

const CREATE_SCENE_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_scene_history_scene_time
ON scene_execution_history(scene_id, executed_at DESC)
"#;

const CREATE_AUTOMATION_HISTORY_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS automation_execution_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    executed_at TEXT NOT NULL,
    automation_id TEXT NOT NULL,
    trigger_payload_json TEXT NOT NULL,
    status TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    error TEXT,
    results_json TEXT NOT NULL
)
"#;

const CREATE_AUTOMATION_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_automation_history_automation_time
ON automation_execution_history(automation_id, executed_at DESC)
"#;

const CREATE_AUTOMATION_RUNTIME_STATE_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS automation_runtime_state (
    automation_id TEXT PRIMARY KEY,
    updated_at TEXT NOT NULL,
    last_triggered_at TEXT,
    last_trigger_fingerprint TEXT,
    last_scheduled_at TEXT
)
"#;

const SCHEMA_VERSION_KEY: &str = "schema_version";
const SCHEMA_VERSION_V1: i64 = 1;
const SCHEMA_VERSION_V2: i64 = 2;

#[derive(Debug, Clone)]
pub struct SqliteHistoryConfig {
    pub enabled: bool,
    pub retention: Option<Duration>,
    pub selection: HistorySelection,
}

#[derive(Debug, Clone, Default)]
pub struct HistorySelection {
    pub device_ids: Vec<String>,
    pub capabilities: Vec<String>,
    pub adapter_names: Vec<String>,
}

impl Default for SqliteHistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention: None,
            selection: HistorySelection::default(),
        }
    }
}

#[derive(Clone)]
pub struct SqliteDeviceStore {
    pool: SqlitePool,
    history: SqliteHistoryConfig,
}

impl SqliteDeviceStore {
    pub async fn new(database_url: &str, auto_create: bool) -> Result<Self> {
        Self::new_with_history(database_url, auto_create, SqliteHistoryConfig::default()).await
    }

    pub async fn new_with_history(
        database_url: &str,
        auto_create: bool,
        history: SqliteHistoryConfig,
    ) -> Result<Self> {
        let options = sqlite_connect_options(database_url, auto_create)?;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .with_context(|| format!("failed to connect to SQLite database '{database_url}'"))?;

        let store = Self { pool, history };

        if auto_create {
            store.initialize().await?;
        }

        Ok(store)
    }

    pub async fn initialize(&self) -> Result<()> {
        sqlx::query(CREATE_SCHEMA_METADATA_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite schema metadata table")?;

        let schema_version = sqlx::query("SELECT value FROM schema_metadata WHERE key = ?1")
            .bind(SCHEMA_VERSION_KEY)
            .fetch_optional(&self.pool)
            .await
            .context("failed to read SQLite schema version")?
            .map(|row| row.get::<String, _>("value"))
            .map(|value| {
                value
                    .parse::<i64>()
                    .with_context(|| format!("invalid SQLite schema version value '{value}'"))
            })
            .transpose()?
            .unwrap_or(0);

        if schema_version < 1 {
            self.migrate_to_v1().await?;
        }
        if schema_version < 2 {
            self.migrate_to_v2().await?;
        }

        Ok(())
    }

    async fn migrate_to_v1(&self) -> Result<()> {
        sqlx::query(CREATE_ROOMS_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite rooms table")?;

        sqlx::query(CREATE_DEVICES_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite devices table")?;

        sqlx::query(CREATE_DEVICE_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite device history table")?;

        sqlx::query(CREATE_ATTRIBUTE_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite attribute history table")?;

        sqlx::query(CREATE_DEVICE_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite device history index")?;

        sqlx::query(CREATE_ATTRIBUTE_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite attribute history index")?;

        sqlx::query(CREATE_COMMAND_AUDIT_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite command audit table")?;

        sqlx::query(CREATE_COMMAND_AUDIT_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite command audit index")?;

        sqlx::query(CREATE_SCENE_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite scene history table")?;

        sqlx::query(CREATE_SCENE_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite scene history index")?;

        sqlx::query(CREATE_AUTOMATION_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite automation history table")?;

        sqlx::query(CREATE_AUTOMATION_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite automation history index")?;

        sqlx::query(
            r#"
            INSERT INTO schema_metadata (key, value)
            VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
        )
        .bind(SCHEMA_VERSION_KEY)
        .bind(SCHEMA_VERSION_V1.to_string())
        .execute(&self.pool)
        .await
        .context("failed to write SQLite schema version")?;

        Ok(())
    }

    async fn migrate_to_v2(&self) -> Result<()> {
        sqlx::query(CREATE_AUTOMATION_RUNTIME_STATE_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite automation runtime state table")?;

        sqlx::query(
            r#"
            INSERT INTO schema_metadata (key, value)
            VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
        )
        .bind(SCHEMA_VERSION_KEY)
        .bind(SCHEMA_VERSION_V2.to_string())
        .execute(&self.pool)
        .await
        .context("failed to write SQLite schema version")?;

        Ok(())
    }

    async fn load_persisted_device(&self, id: &DeviceId) -> Result<Option<Device>> {
        sqlx::query(
            r#"
            SELECT device_id, room_id, kind, attributes_json, metadata_json, updated_at, last_seen
            FROM devices
            WHERE device_id = ?1
            "#,
        )
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("failed to load persisted device '{}' from SQLite", id.0))?
        .map(device_from_row)
        .transpose()
    }

    async fn persist_history_if_needed(
        &self,
        device: &Device,
        previous: Option<&Device>,
    ) -> Result<()> {
        if !self.history.enabled {
            return Ok(());
        }

        let observed_at = device.updated_at;
        let should_record_device = previous
            .map(|existing| {
                existing.room_id != device.room_id
                    || existing.kind != device.kind
                    || existing.attributes != device.attributes
                    || existing.metadata != device.metadata
                    || existing.updated_at != device.updated_at
            })
            .unwrap_or(true);

        if should_record_device && self.should_record_device(device) {
            let device_json = serde_json::to_string(device).with_context(|| {
                format!("failed to serialize device history for '{}'", device.id.0)
            })?;
            sqlx::query(
                r#"
                INSERT OR REPLACE INTO device_history (device_id, observed_at, device_json)
                VALUES (?1, ?2, ?3)
                "#,
            )
            .bind(&device.id.0)
            .bind(observed_at.to_rfc3339())
            .bind(device_json)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to save device history for '{}'", device.id.0))?;
        }

        for (attribute, value) in &device.attributes {
            if !self.should_record_attribute(device, attribute) {
                continue;
            }

            let changed = previous
                .and_then(|existing| existing.attributes.get(attribute))
                .map(|previous| previous != value)
                .unwrap_or(true);

            if !changed {
                continue;
            }

            let value_json = serde_json::to_string(value).with_context(|| {
                format!(
                    "failed to serialize attribute history for '{}' attribute '{}'",
                    device.id.0, attribute
                )
            })?;
            sqlx::query(
                r#"
                INSERT OR REPLACE INTO attribute_history (device_id, attribute, observed_at, value_json)
                VALUES (?1, ?2, ?3, ?4)
                "#,
            )
            .bind(&device.id.0)
            .bind(attribute)
            .bind(observed_at.to_rfc3339())
            .bind(value_json)
            .execute(&self.pool)
            .await
            .with_context(|| {
                format!(
                    "failed to save attribute history for '{}' attribute '{}'",
                    device.id.0, attribute
                )
            })?;
        }

        self.prune_history().await?;
        Ok(())
    }

    async fn prune_history(&self) -> Result<()> {
        let Some(retention) = self.history.retention else {
            return Ok(());
        };

        let cutoff = chrono::Duration::from_std(retention)
            .context("invalid SQLite history retention duration")?;
        let cutoff = (Utc::now() - cutoff).to_rfc3339();

        sqlx::query("DELETE FROM device_history WHERE observed_at < ?1")
            .bind(&cutoff)
            .execute(&self.pool)
            .await
            .context("failed to prune SQLite device history")?;

        sqlx::query("DELETE FROM attribute_history WHERE observed_at < ?1")
            .bind(&cutoff)
            .execute(&self.pool)
            .await
            .context("failed to prune SQLite attribute history")?;

        Ok(())
    }

    fn should_record_device(&self, device: &Device) -> bool {
        self.selection_allows_device(&device.id.0, &device.metadata.source)
    }

    fn should_record_attribute(&self, device: &Device, attribute: &str) -> bool {
        self.selection_allows_device(&device.id.0, &device.metadata.source)
            && self.selection_allows_capability(attribute)
    }

    fn should_record_command_audit(&self, entry: &CommandAuditEntry) -> bool {
        self.selection_allows_device(&entry.device_id.0, device_adapter_name(&entry.device_id.0))
            && self.selection_allows_capability(&entry.command.capability)
    }

    fn should_record_scene_execution(&self, entry: &SceneExecutionHistoryEntry) -> bool {
        if self.history.selection.adapter_names.is_empty()
            && self.history.selection.capabilities.is_empty()
        {
            return true;
        }

        entry.results.iter().any(|result| {
            self.selection_allows_device(&result.target, device_adapter_name(&result.target))
        })
    }

    fn should_record_automation_execution(&self, entry: &AutomationExecutionHistoryEntry) -> bool {
        if !self.selection_allows_trigger_payload(&entry.trigger_payload) {
            return false;
        }

        if self.history.selection.capabilities.is_empty()
            && self.history.selection.adapter_names.is_empty()
        {
            return true;
        }

        entry.results.iter().any(|result| {
            self.selection_allows_device(&result.target, device_adapter_name(&result.target))
        }) || entry.results.is_empty()
    }

    fn selection_allows_device(&self, device_id: &str, adapter_name: &str) -> bool {
        let device_match = self.history.selection.device_ids.is_empty()
            || self
                .history
                .selection
                .device_ids
                .iter()
                .any(|candidate| candidate == device_id);
        let adapter_match = self.history.selection.adapter_names.is_empty()
            || self
                .history
                .selection
                .adapter_names
                .iter()
                .any(|candidate| candidate == adapter_name);

        device_match && adapter_match
    }

    fn selection_allows_capability(&self, capability: &str) -> bool {
        self.history.selection.capabilities.is_empty()
            || self
                .history
                .selection
                .capabilities
                .iter()
                .any(|candidate| candidate == capability)
    }

    fn selection_allows_trigger_payload(&self, payload: &AttributeValue) -> bool {
        let AttributeValue::Object(fields) = payload else {
            return self.history.selection.device_ids.is_empty()
                && self.history.selection.adapter_names.is_empty();
        };

        let device_id = fields.get("device_id").and_then(attribute_text);
        let attribute = fields.get("attribute").and_then(attribute_text);

        let device_match = match device_id {
            Some(device_id) => {
                self.selection_allows_device(device_id, device_adapter_name(device_id))
            }
            None => {
                self.history.selection.device_ids.is_empty()
                    && self.history.selection.adapter_names.is_empty()
            }
        };
        let capability_match = match attribute {
            Some(attribute) => self.selection_allows_capability(attribute),
            None => self.history.selection.capabilities.is_empty(),
        };

        device_match && capability_match
    }
}

#[async_trait::async_trait]
impl DeviceStore for SqliteDeviceStore {
    async fn load_all_devices(&self) -> Result<Vec<Device>> {
        let rows = sqlx::query(
            r#"
            SELECT device_id, room_id, kind, attributes_json, metadata_json, updated_at, last_seen
            FROM devices
            ORDER BY device_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load devices from SQLite")?;

        rows.into_iter().map(device_from_row).collect()
    }

    async fn load_all_rooms(&self) -> Result<Vec<Room>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name
            FROM rooms
            ORDER BY id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load rooms from SQLite")?;

        rows.into_iter().map(room_from_row).collect()
    }

    async fn save_device(&self, device: &Device) -> Result<()> {
        let previous = self.load_persisted_device(&device.id).await?;
        let attributes_json = serde_json::to_string(&device.attributes)
            .with_context(|| format!("failed to serialize attributes for '{}'", device.id.0))?;
        let metadata_json = serde_json::to_string(&device.metadata)
            .with_context(|| format!("failed to serialize metadata for '{}'", device.id.0))?;

        sqlx::query(
            r#"
            INSERT INTO devices (device_id, room_id, kind, attributes_json, metadata_json, updated_at, last_seen)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(device_id) DO UPDATE SET
                room_id = excluded.room_id,
                kind = excluded.kind,
                attributes_json = excluded.attributes_json,
                metadata_json = excluded.metadata_json,
                updated_at = excluded.updated_at,
                last_seen = excluded.last_seen
            "#,
        )
        .bind(&device.id.0)
        .bind(device.room_id.as_ref().map(|id| id.0.as_str()))
        .bind(device_kind_to_str(&device.kind))
        .bind(attributes_json)
        .bind(metadata_json)
        .bind(device.updated_at.to_rfc3339())
        .bind(device.last_seen.to_rfc3339())
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to save device '{}' to SQLite", device.id.0))?;

        self.persist_history_if_needed(device, previous.as_ref())
            .await?;

        Ok(())
    }

    async fn save_room(&self, room: &Room) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO rooms (id, name)
            VALUES (?1, ?2)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name
            "#,
        )
        .bind(&room.id.0)
        .bind(&room.name)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to save room '{}' to SQLite", room.id.0))?;

        Ok(())
    }

    async fn delete_device(&self, id: &DeviceId) -> Result<()> {
        sqlx::query("DELETE FROM devices WHERE device_id = ?1")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete device '{}' from SQLite", id.0))?;

        Ok(())
    }

    async fn delete_room(&self, id: &RoomId) -> Result<()> {
        sqlx::query("DELETE FROM rooms WHERE id = ?1")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete room '{}' from SQLite", id.0))?;

        Ok(())
    }

    async fn load_device_history(
        &self,
        id: &DeviceId,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<DeviceHistoryEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT observed_at, device_json
            FROM device_history
            WHERE device_id = ?1
              AND (?2 IS NULL OR observed_at >= ?2)
              AND (?3 IS NULL OR observed_at <= ?3)
            ORDER BY observed_at DESC
            LIMIT ?4
            "#,
        )
        .bind(&id.0)
        .bind(start.map(|value| value.to_rfc3339()))
        .bind(end.map(|value| value.to_rfc3339()))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("failed to load device history for '{}'", id.0))?;

        rows.into_iter().map(device_history_from_row).collect()
    }

    async fn load_attribute_history(
        &self,
        id: &DeviceId,
        attribute: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<AttributeHistoryEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT device_id, attribute, observed_at, value_json
            FROM attribute_history
            WHERE device_id = ?1
              AND attribute = ?2
              AND (?3 IS NULL OR observed_at >= ?3)
              AND (?4 IS NULL OR observed_at <= ?4)
            ORDER BY observed_at DESC
            LIMIT ?5
            "#,
        )
        .bind(&id.0)
        .bind(attribute)
        .bind(start.map(|value| value.to_rfc3339()))
        .bind(end.map(|value| value.to_rfc3339()))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .with_context(|| {
            format!(
                "failed to load attribute history for '{}' attribute '{}'",
                id.0, attribute
            )
        })?;

        rows.into_iter().map(attribute_history_from_row).collect()
    }

    async fn save_command_audit(&self, entry: &CommandAuditEntry) -> Result<()> {
        if !self.history.enabled || !self.should_record_command_audit(entry) {
            return Ok(());
        }

        let command_json = serde_json::to_string(&entry.command)
            .context("failed to serialize command audit entry")?;

        sqlx::query(
            r#"
            INSERT INTO command_audit (recorded_at, source, room_id, device_id, command_json, status, message)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
        )
        .bind(entry.recorded_at.to_rfc3339())
        .bind(&entry.source)
        .bind(entry.room_id.as_ref().map(|id| id.0.as_str()))
        .bind(&entry.device_id.0)
        .bind(command_json)
        .bind(&entry.status)
        .bind(&entry.message)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to save command audit for '{}'", entry.device_id.0))?;

        Ok(())
    }

    async fn load_command_audit(
        &self,
        device_id: Option<&DeviceId>,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<CommandAuditEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT recorded_at, source, room_id, device_id, command_json, status, message
            FROM command_audit
            WHERE (?1 IS NULL OR device_id = ?1)
              AND (?2 IS NULL OR recorded_at >= ?2)
              AND (?3 IS NULL OR recorded_at <= ?3)
            ORDER BY recorded_at DESC, id DESC
            LIMIT ?4
            "#,
        )
        .bind(device_id.map(|value| value.0.as_str()))
        .bind(start.map(|value| value.to_rfc3339()))
        .bind(end.map(|value| value.to_rfc3339()))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load command audit from SQLite")?;

        rows.into_iter().map(command_audit_from_row).collect()
    }

    async fn save_scene_execution(&self, entry: &SceneExecutionHistoryEntry) -> Result<()> {
        if !self.history.enabled || !self.should_record_scene_execution(entry) {
            return Ok(());
        }

        let results_json = serde_json::to_string(&entry.results)
            .context("failed to serialize scene execution history entry")?;

        sqlx::query(
            r#"
            INSERT INTO scene_execution_history (executed_at, scene_id, status, error, results_json)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
        )
        .bind(entry.executed_at.to_rfc3339())
        .bind(&entry.scene_id)
        .bind(&entry.status)
        .bind(&entry.error)
        .bind(results_json)
        .execute(&self.pool)
        .await
        .with_context(|| {
            format!(
                "failed to save scene execution history for '{}'",
                entry.scene_id
            )
        })?;

        Ok(())
    }

    async fn load_scene_history(
        &self,
        scene_id: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<SceneExecutionHistoryEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT executed_at, scene_id, status, error, results_json
            FROM scene_execution_history
            WHERE scene_id = ?1
              AND (?2 IS NULL OR executed_at >= ?2)
              AND (?3 IS NULL OR executed_at <= ?3)
            ORDER BY executed_at DESC, id DESC
            LIMIT ?4
            "#,
        )
        .bind(scene_id)
        .bind(start.map(|value| value.to_rfc3339()))
        .bind(end.map(|value| value.to_rfc3339()))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("failed to load scene history for '{scene_id}'"))?;

        rows.into_iter().map(scene_history_from_row).collect()
    }

    async fn save_automation_execution(
        &self,
        entry: &AutomationExecutionHistoryEntry,
    ) -> Result<()> {
        if !self.history.enabled || !self.should_record_automation_execution(entry) {
            return Ok(());
        }

        let trigger_payload_json = serde_json::to_string(&entry.trigger_payload)
            .context("failed to serialize automation trigger payload")?;
        let results_json = serde_json::to_string(&entry.results)
            .context("failed to serialize automation execution history entry")?;

        sqlx::query(
            r#"
            INSERT INTO automation_execution_history (executed_at, automation_id, trigger_payload_json, status, duration_ms, error, results_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
        )
        .bind(entry.executed_at.to_rfc3339())
        .bind(&entry.automation_id)
        .bind(trigger_payload_json)
        .bind(&entry.status)
        .bind(entry.duration_ms)
        .bind(&entry.error)
        .bind(results_json)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to save automation execution history for '{}'", entry.automation_id))?;

        Ok(())
    }

    async fn load_automation_history(
        &self,
        automation_id: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<AutomationExecutionHistoryEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT executed_at, automation_id, trigger_payload_json, status, duration_ms, error, results_json
            FROM automation_execution_history
            WHERE automation_id = ?1
              AND (?2 IS NULL OR executed_at >= ?2)
              AND (?3 IS NULL OR executed_at <= ?3)
            ORDER BY executed_at DESC, id DESC
            LIMIT ?4
            "#,
        )
        .bind(automation_id)
        .bind(start.map(|value| value.to_rfc3339()))
        .bind(end.map(|value| value.to_rfc3339()))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("failed to load automation history for '{automation_id}'"))?;

        rows.into_iter().map(automation_history_from_row).collect()
    }

    async fn load_automation_runtime_state(
        &self,
        automation_id: &str,
    ) -> Result<Option<AutomationRuntimeState>> {
        sqlx::query(
            r#"
            SELECT automation_id, updated_at, last_triggered_at, last_trigger_fingerprint, last_scheduled_at
            FROM automation_runtime_state
            WHERE automation_id = ?1
            "#,
        )
        .bind(automation_id)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("failed to load automation runtime state for '{automation_id}'"))?
        .map(automation_runtime_state_from_row)
        .transpose()
    }

    async fn save_automation_runtime_state(&self, state: &AutomationRuntimeState) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO automation_runtime_state (
                automation_id,
                updated_at,
                last_triggered_at,
                last_trigger_fingerprint,
                last_scheduled_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(automation_id) DO UPDATE SET
                updated_at = excluded.updated_at,
                last_triggered_at = excluded.last_triggered_at,
                last_trigger_fingerprint = excluded.last_trigger_fingerprint,
                last_scheduled_at = excluded.last_scheduled_at
            "#,
        )
        .bind(&state.automation_id)
        .bind(state.updated_at.to_rfc3339())
        .bind(state.last_triggered_at.map(|value| value.to_rfc3339()))
        .bind(&state.last_trigger_fingerprint)
        .bind(state.last_scheduled_at.map(|value| value.to_rfc3339()))
        .execute(&self.pool)
        .await
        .with_context(|| {
            format!(
                "failed to save automation runtime state for '{}'",
                state.automation_id
            )
        })?;

        Ok(())
    }
}

fn sqlite_connect_options(database_url: &str, auto_create: bool) -> Result<SqliteConnectOptions> {
    let mut options = SqliteConnectOptions::from_str(database_url)
        .with_context(|| format!("invalid SQLite database URL '{database_url}'"))?
        .create_if_missing(auto_create)
        .foreign_keys(true);

    if let Some(path) = sqlite_path(database_url) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create SQLite database directory '{}'",
                    parent.display()
                )
            })?;
        }

        options = options.filename(path);
    }

    Ok(options)
}

fn sqlite_path(database_url: &str) -> Option<&Path> {
    database_url
        .strip_prefix("sqlite://")
        .filter(|path| !path.is_empty() && *path != ":memory:")
        .map(Path::new)
}

fn device_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Device> {
    let id = row.get::<String, _>("device_id");
    let room_id = row.get::<Option<String>, _>("room_id").map(RoomId);
    let kind = device_kind_from_str(&row.get::<String, _>("kind"))
        .with_context(|| format!("invalid device kind for '{id}'"))?;
    let attributes: Attributes = serde_json::from_str(&row.get::<String, _>("attributes_json"))
        .with_context(|| format!("invalid attributes JSON for '{id}'"))?;
    let metadata: Metadata = serde_json::from_str(&row.get::<String, _>("metadata_json"))
        .with_context(|| format!("invalid metadata JSON for '{id}'"))?;
    let updated_at = DateTime::parse_from_rfc3339(&row.get::<String, _>("updated_at"))
        .with_context(|| format!("invalid updated_at for '{id}'"))?
        .with_timezone(&Utc);
    let last_seen = DateTime::parse_from_rfc3339(&row.get::<String, _>("last_seen"))
        .with_context(|| format!("invalid last_seen for '{id}'"))?
        .with_timezone(&Utc);

    Ok(Device {
        id: DeviceId(id),
        room_id,
        kind,
        attributes,
        metadata,
        updated_at,
        last_seen,
    })
}

fn device_history_from_row(row: sqlx::sqlite::SqliteRow) -> Result<DeviceHistoryEntry> {
    let observed_at = DateTime::parse_from_rfc3339(&row.get::<String, _>("observed_at"))
        .context("invalid device history observed_at")?
        .with_timezone(&Utc);
    let device: Device = serde_json::from_str(&row.get::<String, _>("device_json"))
        .context("invalid device history JSON")?;

    Ok(DeviceHistoryEntry {
        observed_at,
        device,
    })
}

fn attribute_history_from_row(row: sqlx::sqlite::SqliteRow) -> Result<AttributeHistoryEntry> {
    let device_id = DeviceId(row.get::<String, _>("device_id"));
    let attribute = row.get::<String, _>("attribute");
    let observed_at = DateTime::parse_from_rfc3339(&row.get::<String, _>("observed_at"))
        .context("invalid attribute history observed_at")?
        .with_timezone(&Utc);
    let value: AttributeValue = serde_json::from_str(&row.get::<String, _>("value_json"))
        .context("invalid attribute history JSON")?;

    Ok(AttributeHistoryEntry {
        observed_at,
        device_id,
        attribute,
        value,
    })
}

fn command_audit_from_row(row: sqlx::sqlite::SqliteRow) -> Result<CommandAuditEntry> {
    let recorded_at = DateTime::parse_from_rfc3339(&row.get::<String, _>("recorded_at"))
        .context("invalid command audit recorded_at")?
        .with_timezone(&Utc);
    let command = serde_json::from_str(&row.get::<String, _>("command_json"))
        .context("invalid command audit JSON")?;

    Ok(CommandAuditEntry {
        recorded_at,
        source: row.get::<String, _>("source"),
        room_id: row.get::<Option<String>, _>("room_id").map(RoomId),
        device_id: DeviceId(row.get::<String, _>("device_id")),
        command,
        status: row.get::<String, _>("status"),
        message: row.get::<Option<String>, _>("message"),
    })
}

fn scene_history_from_row(row: sqlx::sqlite::SqliteRow) -> Result<SceneExecutionHistoryEntry> {
    let executed_at = DateTime::parse_from_rfc3339(&row.get::<String, _>("executed_at"))
        .context("invalid scene execution history executed_at")?
        .with_timezone(&Utc);
    let results = serde_json::from_str(&row.get::<String, _>("results_json"))
        .context("invalid scene execution history JSON")?;

    Ok(SceneExecutionHistoryEntry {
        executed_at,
        scene_id: row.get::<String, _>("scene_id"),
        status: row.get::<String, _>("status"),
        error: row.get::<Option<String>, _>("error"),
        results,
    })
}

fn automation_history_from_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<AutomationExecutionHistoryEntry> {
    let executed_at = DateTime::parse_from_rfc3339(&row.get::<String, _>("executed_at"))
        .context("invalid automation execution history executed_at")?
        .with_timezone(&Utc);
    let trigger_payload = serde_json::from_str(&row.get::<String, _>("trigger_payload_json"))
        .context("invalid automation trigger payload JSON")?;
    let results = serde_json::from_str(&row.get::<String, _>("results_json"))
        .context("invalid automation execution history JSON")?;

    Ok(AutomationExecutionHistoryEntry {
        executed_at,
        automation_id: row.get::<String, _>("automation_id"),
        trigger_payload,
        status: row.get::<String, _>("status"),
        duration_ms: row.get::<i64, _>("duration_ms"),
        error: row.get::<Option<String>, _>("error"),
        results,
    })
}

fn automation_runtime_state_from_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<AutomationRuntimeState> {
    Ok(AutomationRuntimeState {
        automation_id: row.get::<String, _>("automation_id"),
        updated_at: DateTime::parse_from_rfc3339(&row.get::<String, _>("updated_at"))
            .context("invalid automation runtime state updated_at")?
            .with_timezone(&Utc),
        last_triggered_at: row
            .get::<Option<String>, _>("last_triggered_at")
            .map(|value| {
                DateTime::parse_from_rfc3339(&value)
                    .context("invalid automation runtime state last_triggered_at")
                    .map(|value| value.with_timezone(&Utc))
            })
            .transpose()?,
        last_trigger_fingerprint: row.get::<Option<String>, _>("last_trigger_fingerprint"),
        last_scheduled_at: row
            .get::<Option<String>, _>("last_scheduled_at")
            .map(|value| {
                DateTime::parse_from_rfc3339(&value)
                    .context("invalid automation runtime state last_scheduled_at")
                    .map(|value| value.with_timezone(&Utc))
            })
            .transpose()?,
    })
}

fn room_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Room> {
    Ok(Room {
        id: RoomId(row.get::<String, _>("id")),
        name: row.get::<String, _>("name"),
    })
}

fn device_kind_to_str(kind: &DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Sensor => "sensor",
        DeviceKind::Light => "light",
        DeviceKind::Switch => "switch",
        DeviceKind::Virtual => "virtual",
    }
}

fn device_kind_from_str(value: &str) -> Result<DeviceKind> {
    match value {
        "sensor" => Ok(DeviceKind::Sensor),
        "light" => Ok(DeviceKind::Light),
        "switch" => Ok(DeviceKind::Switch),
        "virtual" => Ok(DeviceKind::Virtual),
        other => anyhow::bail!("unsupported device kind '{other}'"),
    }
}

fn attribute_text(value: &AttributeValue) -> Option<&str> {
    match value {
        AttributeValue::Text(value) => Some(value.as_str()),
        _ => None,
    }
}

fn device_adapter_name(device_id: &str) -> &str {
    device_id
        .split_once(':')
        .map(|(adapter, _)| adapter)
        .unwrap_or(device_id)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::Duration as ChronoDuration;
    use smart_home_core::capability::{measurement_value, TEMPERATURE_OUTDOOR};
    use smart_home_core::command::DeviceCommand;
    use smart_home_core::model::{AttributeValue, DeviceKind};
    use smart_home_core::store::SceneStepResult;

    use super::*;

    fn sample_device(id: &str, value: f64) -> Device {
        sample_device_with_timestamp(id, value, Utc::now())
    }

    fn sample_device_with_timestamp(id: &str, value: f64, updated_at: DateTime<Utc>) -> Device {
        let mut vendor_specific = HashMap::new();
        vendor_specific.insert("provider".to_string(), serde_json::json!("test"));

        Device {
            id: DeviceId(id.to_string()),
            room_id: Some(RoomId("lab".to_string())),
            kind: DeviceKind::Sensor,
            attributes: HashMap::from([
                (
                    TEMPERATURE_OUTDOOR.to_string(),
                    measurement_value(value, "celsius"),
                ),
                ("online".to_string(), AttributeValue::Bool(true)),
            ]),
            metadata: Metadata {
                source: "test".to_string(),
                accuracy: Some(0.9),
                vendor_specific,
            },
            updated_at,
            last_seen: updated_at,
        }
    }

    async fn temp_store() -> SqliteDeviceStore {
        temp_store_with_history(SqliteHistoryConfig::default()).await
    }

    async fn temp_store_with_history(history: SqliteHistoryConfig) -> SqliteDeviceStore {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("smart-home-store-{unique}.db"));
        let url = format!("sqlite://{}", path.display());

        SqliteDeviceStore::new_with_history(&url, true, history)
            .await
            .expect("temporary sqlite store initializes")
    }

    #[tokio::test]
    async fn save_and_load_single_device() {
        let store = temp_store().await;
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");
        let device = sample_device("test:one", 20.0);

        store.save_device(&device).await.expect("save succeeds");

        assert_eq!(
            store.load_all_devices().await.expect("load succeeds"),
            vec![device]
        );
    }

    #[tokio::test]
    async fn save_overwrites_existing_device_by_id() {
        let store = temp_store().await;
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");
        let original = sample_device("test:one", 20.0);
        let updated = sample_device("test:one", 21.5);

        store
            .save_device(&original)
            .await
            .expect("initial save succeeds");
        store
            .save_device(&updated)
            .await
            .expect("update save succeeds");

        assert_eq!(
            store.load_all_devices().await.expect("load succeeds"),
            vec![updated]
        );
    }

    #[tokio::test]
    async fn delete_removes_device_by_id() {
        let store = temp_store().await;
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");
        let device = sample_device("test:one", 20.0);

        store.save_device(&device).await.expect("save succeeds");
        store
            .delete_device(&device.id)
            .await
            .expect("delete succeeds");

        assert!(store
            .load_all_devices()
            .await
            .expect("load succeeds")
            .is_empty());
    }

    #[tokio::test]
    async fn load_multiple_devices() {
        let store = temp_store().await;
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");
        let device_a = sample_device("test:a", 20.0);
        let device_b = sample_device("test:b", 21.0);

        store.save_device(&device_b).await.expect("save b succeeds");
        store.save_device(&device_a).await.expect("save a succeeds");

        assert_eq!(
            store.load_all_devices().await.expect("load succeeds"),
            vec![device_a, device_b]
        );
    }

    #[tokio::test]
    async fn save_and_load_rooms() {
        let store = temp_store().await;
        let room = Room {
            id: RoomId("outside".to_string()),
            name: "Outside".to_string(),
        };

        store.save_room(&room).await.expect("save room succeeds");

        assert_eq!(
            store.load_all_rooms().await.expect("load rooms succeeds"),
            vec![room]
        );
    }

    #[tokio::test]
    async fn deleting_room_clears_device_assignment() {
        let store = temp_store().await;
        let room = Room {
            id: RoomId("lab".to_string()),
            name: "Lab".to_string(),
        };
        store.save_room(&room).await.expect("save room succeeds");
        let device = sample_device("test:one", 20.0);
        store.save_device(&device).await.expect("save succeeds");

        store
            .delete_room(&room.id)
            .await
            .expect("delete room succeeds");

        let devices = store
            .load_all_devices()
            .await
            .expect("load devices succeeds");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].room_id, None);
    }

    #[tokio::test]
    async fn records_device_and_attribute_history_over_time() {
        let store = temp_store().await;
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");
        let first_at = Utc::now() - ChronoDuration::minutes(2);
        let second_at = Utc::now() - ChronoDuration::minutes(1);
        let first = sample_device_with_timestamp("test:one", 20.0, first_at);
        let second = sample_device_with_timestamp("test:one", 21.5, second_at);

        store
            .save_device(&first)
            .await
            .expect("first save succeeds");
        store
            .save_device(&second)
            .await
            .expect("second save succeeds");

        let device_history = store
            .load_device_history(&first.id, None, None, 10)
            .await
            .expect("device history loads");
        assert_eq!(device_history.len(), 2);
        assert_eq!(device_history[0].observed_at, second_at);
        assert_eq!(device_history[0].device.attributes, second.attributes);
        assert_eq!(device_history[1].observed_at, first_at);

        let attribute_history = store
            .load_attribute_history(&first.id, TEMPERATURE_OUTDOOR, None, None, 10)
            .await
            .expect("attribute history loads");
        assert_eq!(attribute_history.len(), 2);
        assert_eq!(attribute_history[0].observed_at, second_at);
        assert_eq!(
            attribute_history[0].value,
            second.attributes[TEMPERATURE_OUTDOOR]
        );
        assert_eq!(attribute_history[1].observed_at, first_at);
    }

    #[tokio::test]
    async fn does_not_record_duplicate_attribute_history_for_last_seen_only_updates() {
        let store = temp_store().await;
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");
        let updated_at = Utc::now() - ChronoDuration::minutes(1);
        let first = sample_device_with_timestamp("test:one", 20.0, updated_at);
        let mut seen_again = first.clone();
        seen_again.last_seen = Utc::now();

        store
            .save_device(&first)
            .await
            .expect("first save succeeds");
        store
            .save_device(&seen_again)
            .await
            .expect("last_seen save succeeds");

        let device_history = store
            .load_device_history(&first.id, None, None, 10)
            .await
            .expect("device history loads");
        assert_eq!(device_history.len(), 1);

        let attribute_history = store
            .load_attribute_history(&first.id, TEMPERATURE_OUTDOOR, None, None, 10)
            .await
            .expect("attribute history loads");
        assert_eq!(attribute_history.len(), 1);
    }

    #[tokio::test]
    async fn prunes_history_older_than_retention_window() {
        let store = temp_store_with_history(SqliteHistoryConfig {
            enabled: true,
            retention: Some(Duration::from_secs(60)),
            selection: HistorySelection::default(),
        })
        .await;
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");

        let old =
            sample_device_with_timestamp("test:one", 20.0, Utc::now() - ChronoDuration::minutes(5));
        let fresh = sample_device_with_timestamp("test:one", 21.0, Utc::now());

        store.save_device(&old).await.expect("old save succeeds");
        store
            .save_device(&fresh)
            .await
            .expect("fresh save succeeds");

        let device_history = store
            .load_device_history(&fresh.id, None, None, 10)
            .await
            .expect("device history loads");
        assert_eq!(device_history.len(), 1);
        assert_eq!(device_history[0].observed_at, fresh.updated_at);
    }

    #[tokio::test]
    async fn saves_and_loads_command_audit_history() {
        let store = temp_store().await;
        let recorded_at = Utc::now();
        let entry = CommandAuditEntry {
            recorded_at,
            source: "device".to_string(),
            room_id: Some(RoomId("lab".to_string())),
            device_id: DeviceId("test:one".to_string()),
            command: smart_home_core::command::DeviceCommand {
                capability: "brightness".to_string(),
                action: "set".to_string(),
                value: Some(AttributeValue::Integer(42)),
            },
            status: "ok".to_string(),
            message: None,
        };

        store
            .save_command_audit(&entry)
            .await
            .expect("save command audit succeeds");

        let entries = store
            .load_command_audit(Some(&entry.device_id), None, None, 10)
            .await
            .expect("load command audit succeeds");
        assert_eq!(entries, vec![entry]);
    }

    #[tokio::test]
    async fn saves_and_loads_scene_execution_history() {
        let store = temp_store().await;
        let executed_at = Utc::now();
        let entry = SceneExecutionHistoryEntry {
            executed_at,
            scene_id: "movie_time".to_string(),
            status: "ok".to_string(),
            error: None,
            results: vec![SceneStepResult {
                target: "test:one".to_string(),
                status: "ok".to_string(),
                message: None,
            }],
        };

        store
            .save_scene_execution(&entry)
            .await
            .expect("save scene history succeeds");

        let entries = store
            .load_scene_history("movie_time", None, None, 10)
            .await
            .expect("load scene history succeeds");
        assert_eq!(entries, vec![entry]);
    }

    #[tokio::test]
    async fn saves_and_loads_automation_execution_history() {
        let store = temp_store().await;
        let executed_at = Utc::now();
        let entry = AutomationExecutionHistoryEntry {
            executed_at,
            automation_id: "rain_check".to_string(),
            trigger_payload: AttributeValue::Object(HashMap::from([(
                "type".to_string(),
                AttributeValue::Text("interval".to_string()),
            )])),
            status: "ok".to_string(),
            duration_ms: 12,
            error: None,
            results: vec![SceneStepResult {
                target: "test:one".to_string(),
                status: "ok".to_string(),
                message: None,
            }],
        };

        store
            .save_automation_execution(&entry)
            .await
            .expect("save automation history succeeds");

        let entries = store
            .load_automation_history("rain_check", None, None, 10)
            .await
            .expect("load automation history succeeds");
        assert_eq!(entries, vec![entry]);
    }

    #[tokio::test]
    async fn saves_and_loads_automation_runtime_state() {
        let store = temp_store().await;
        let updated_at = Utc::now();
        let state = AutomationRuntimeState {
            updated_at,
            automation_id: "rain_check".to_string(),
            last_triggered_at: Some(updated_at),
            last_trigger_fingerprint: Some("{\"type\":\"interval\"}".to_string()),
            last_scheduled_at: Some(updated_at),
        };

        store
            .save_automation_runtime_state(&state)
            .await
            .expect("save runtime state succeeds");

        let loaded = store
            .load_automation_runtime_state("rain_check")
            .await
            .expect("load runtime state succeeds")
            .expect("runtime state exists");
        assert_eq!(loaded, state);
    }

    #[tokio::test]
    async fn telemetry_selection_filters_device_and_attribute_history() {
        let store = temp_store_with_history(SqliteHistoryConfig {
            enabled: true,
            retention: None,
            selection: HistorySelection {
                device_ids: vec!["test:allowed".to_string()],
                capabilities: vec![TEMPERATURE_OUTDOOR.to_string()],
                adapter_names: Vec::new(),
            },
        })
        .await;
        store
            .save_room(&Room {
                id: RoomId("lab".to_string()),
                name: "Lab".to_string(),
            })
            .await
            .expect("save room succeeds");

        let allowed = sample_device_with_timestamp("test:allowed", 20.0, Utc::now());
        let blocked = sample_device_with_timestamp("test:blocked", 21.0, Utc::now());

        store
            .save_device(&allowed)
            .await
            .expect("allowed save succeeds");
        store
            .save_device(&blocked)
            .await
            .expect("blocked save succeeds");

        assert_eq!(
            store
                .load_device_history(&allowed.id, None, None, 10)
                .await
                .expect("allowed history loads")
                .len(),
            1
        );
        assert!(store
            .load_device_history(&blocked.id, None, None, 10)
            .await
            .expect("blocked history loads")
            .is_empty());
        assert_eq!(
            store
                .load_attribute_history(&allowed.id, TEMPERATURE_OUTDOOR, None, None, 10)
                .await
                .expect("allowed attribute history loads")
                .len(),
            1
        );
        assert!(store
            .load_attribute_history(&allowed.id, "online", None, None, 10)
            .await
            .expect("filtered attribute history loads")
            .is_empty());
    }

    #[tokio::test]
    async fn telemetry_selection_filters_command_scene_and_automation_history() {
        let store = temp_store_with_history(SqliteHistoryConfig {
            enabled: true,
            retention: None,
            selection: HistorySelection {
                device_ids: vec!["test:allowed".to_string()],
                capabilities: vec!["brightness".to_string()],
                adapter_names: Vec::new(),
            },
        })
        .await;

        store
            .save_command_audit(&CommandAuditEntry {
                recorded_at: Utc::now(),
                source: "device".to_string(),
                room_id: None,
                device_id: DeviceId("test:allowed".to_string()),
                command: DeviceCommand {
                    capability: "brightness".to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Integer(42)),
                },
                status: "ok".to_string(),
                message: None,
            })
            .await
            .expect("allowed command audit saves");
        store
            .save_command_audit(&CommandAuditEntry {
                recorded_at: Utc::now(),
                source: "device".to_string(),
                room_id: None,
                device_id: DeviceId("test:blocked".to_string()),
                command: DeviceCommand {
                    capability: "brightness".to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Integer(42)),
                },
                status: "ok".to_string(),
                message: None,
            })
            .await
            .expect("blocked command audit save succeeds");
        assert_eq!(
            store
                .load_command_audit(None, None, None, 10)
                .await
                .expect("command audit loads")
                .len(),
            1
        );

        store
            .save_scene_execution(&SceneExecutionHistoryEntry {
                executed_at: Utc::now(),
                scene_id: "scene".to_string(),
                status: "ok".to_string(),
                error: None,
                results: vec![SceneStepResult {
                    target: "test:allowed".to_string(),
                    status: "ok".to_string(),
                    message: None,
                }],
            })
            .await
            .expect("allowed scene save succeeds");
        store
            .save_scene_execution(&SceneExecutionHistoryEntry {
                executed_at: Utc::now(),
                scene_id: "scene".to_string(),
                status: "ok".to_string(),
                error: None,
                results: vec![SceneStepResult {
                    target: "test:blocked".to_string(),
                    status: "ok".to_string(),
                    message: None,
                }],
            })
            .await
            .expect("blocked scene save succeeds");
        assert_eq!(
            store
                .load_scene_history("scene", None, None, 10)
                .await
                .expect("scene history loads")
                .len(),
            1
        );

        store
            .save_automation_execution(&AutomationExecutionHistoryEntry {
                executed_at: Utc::now(),
                automation_id: "rain_check".to_string(),
                trigger_payload: AttributeValue::Object(HashMap::from([
                    (
                        "device_id".to_string(),
                        AttributeValue::Text("test:allowed".to_string()),
                    ),
                    (
                        "attribute".to_string(),
                        AttributeValue::Text("brightness".to_string()),
                    ),
                ])),
                status: "ok".to_string(),
                duration_ms: 10,
                error: None,
                results: vec![SceneStepResult {
                    target: "test:allowed".to_string(),
                    status: "ok".to_string(),
                    message: None,
                }],
            })
            .await
            .expect("allowed automation save succeeds");
        store
            .save_automation_execution(&AutomationExecutionHistoryEntry {
                executed_at: Utc::now(),
                automation_id: "rain_check".to_string(),
                trigger_payload: AttributeValue::Object(HashMap::from([
                    (
                        "device_id".to_string(),
                        AttributeValue::Text("test:blocked".to_string()),
                    ),
                    (
                        "attribute".to_string(),
                        AttributeValue::Text("brightness".to_string()),
                    ),
                ])),
                status: "ok".to_string(),
                duration_ms: 10,
                error: None,
                results: vec![SceneStepResult {
                    target: "test:blocked".to_string(),
                    status: "ok".to_string(),
                    message: None,
                }],
            })
            .await
            .expect("blocked automation save succeeds");
        assert_eq!(
            store
                .load_automation_history("rain_check", None, None, 10)
                .await
                .expect("automation history loads")
                .len(),
            1
        );
    }
}
