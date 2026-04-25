//! PostgreSQL-backed implementation of the HomeCmdr persistence stores.
//!
//! Mirrors `store-sql/sqlite.rs` but uses `sqlx::PgPool`, native `JSONB`
//! columns for attributes/metadata, and `TIMESTAMPTZ` for timestamps.
//! Schema is applied on startup via `CREATE TABLE IF NOT EXISTS` — no
//! migration files exist.
//!
//! `PostgresDeviceStore` implements:
//! - `DeviceStore`  — devices, rooms, groups, command audit, history
//! - `ApiKeyStore`  — API key CRUD and lookup
//! - `PersonStore`  — persons, trackers, zones, person history

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use homecmdr_core::model::{
    AttributeValue, Attributes, Device, DeviceGroup, DeviceId, DeviceKind, GroupId, Metadata,
    Person, PersonId, PersonState, Room, RoomId, Zone, ZoneId,
};
use homecmdr_core::history_filter::{self as hf, HistorySelection};
use homecmdr_core::store::{
    ApiKeyRecord, ApiKeyRole, ApiKeyStore, AttributeHistoryEntry, AutomationExecutionHistoryEntry,
    AutomationRuntimeState, CommandAuditEntry, DeviceHistoryEntry, DeviceStore, PersonHistoryEntry,
    PersonStore, SceneExecutionHistoryEntry,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

// ── Schema SQL ────────────────────────────────────────────────────────────────

const CREATE_SCHEMA_METADATA_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
)
"#;

const CREATE_ROOMS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS rooms (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
)
"#;

const CREATE_DEVICES_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS devices (
    device_id TEXT PRIMARY KEY,
    room_id TEXT REFERENCES rooms(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    attributes_json JSONB NOT NULL,
    metadata_json JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    last_seen TIMESTAMPTZ NOT NULL
)
"#;

const CREATE_GROUPS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS groups (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
)
"#;

const CREATE_GROUP_MEMBERS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS group_members (
    group_id TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
    member_order INTEGER NOT NULL,
    PRIMARY KEY (group_id, device_id)
)
"#;

const CREATE_GROUP_MEMBERS_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_group_members_group_order
ON group_members(group_id, member_order)
"#;

const CREATE_DEVICE_HISTORY_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS device_history (
    device_id TEXT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL,
    device_json JSONB NOT NULL,
    PRIMARY KEY (device_id, observed_at)
)
"#;

const CREATE_DEVICE_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_device_history_device_time
ON device_history(device_id, observed_at DESC)
"#;

const CREATE_ATTRIBUTE_HISTORY_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS attribute_history (
    device_id TEXT NOT NULL,
    attribute TEXT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL,
    value_json JSONB NOT NULL,
    PRIMARY KEY (device_id, attribute, observed_at)
)
"#;

const CREATE_ATTRIBUTE_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_attribute_history_device_attr_time
ON attribute_history(device_id, attribute, observed_at DESC)
"#;

const CREATE_COMMAND_AUDIT_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS command_audit (
    id BIGSERIAL PRIMARY KEY,
    recorded_at TIMESTAMPTZ NOT NULL,
    source TEXT NOT NULL,
    room_id TEXT,
    device_id TEXT NOT NULL,
    command_json JSONB NOT NULL,
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
    id BIGSERIAL PRIMARY KEY,
    executed_at TIMESTAMPTZ NOT NULL,
    scene_id TEXT NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    results_json JSONB NOT NULL
)
"#;

const CREATE_SCENE_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_scene_history_scene_time
ON scene_execution_history(scene_id, executed_at DESC)
"#;

const CREATE_AUTOMATION_HISTORY_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS automation_execution_history (
    id BIGSERIAL PRIMARY KEY,
    executed_at TIMESTAMPTZ NOT NULL,
    automation_id TEXT NOT NULL,
    trigger_payload_json JSONB NOT NULL,
    status TEXT NOT NULL,
    duration_ms BIGINT NOT NULL,
    error TEXT,
    results_json JSONB NOT NULL
)
"#;

const CREATE_AUTOMATION_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_automation_history_automation_time
ON automation_execution_history(automation_id, executed_at DESC)
"#;

const CREATE_AUTOMATION_RUNTIME_STATE_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS automation_runtime_state (
    automation_id TEXT PRIMARY KEY,
    updated_at TIMESTAMPTZ NOT NULL,
    last_triggered_at TIMESTAMPTZ,
    last_trigger_fingerprint TEXT,
    last_scheduled_at TIMESTAMPTZ
)
"#;

const CREATE_API_KEYS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS api_keys (
    id BIGSERIAL PRIMARY KEY,
    key_hash TEXT NOT NULL UNIQUE,
    label TEXT NOT NULL,
    role TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ
)
"#;

const ALTER_API_KEYS_ADD_PERSON_ID_SQL: &str =
    "ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS person_id TEXT";

const CREATE_PERSONS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS persons (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    picture TEXT,
    state_json JSONB NOT NULL,
    state_source TEXT,
    latitude DOUBLE PRECISION,
    longitude DOUBLE PRECISION,
    updated_at TIMESTAMPTZ NOT NULL
)
"#;

const CREATE_PERSON_TRACKERS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS person_trackers (
    person_id TEXT NOT NULL REFERENCES persons(id) ON DELETE CASCADE,
    device_id TEXT NOT NULL,
    tracker_order INTEGER NOT NULL,
    PRIMARY KEY (person_id, device_id)
)
"#;

const CREATE_PERSON_TRACKERS_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_person_trackers_person_order
ON person_trackers(person_id, tracker_order)
"#;

const CREATE_ZONES_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS zones (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    latitude DOUBLE PRECISION NOT NULL,
    longitude DOUBLE PRECISION NOT NULL,
    radius_meters DOUBLE PRECISION NOT NULL,
    icon TEXT,
    passive BOOLEAN NOT NULL DEFAULT FALSE
)
"#;

const CREATE_PERSON_HISTORY_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS person_history (
    id BIGSERIAL PRIMARY KEY,
    person_id TEXT NOT NULL,
    state TEXT NOT NULL,
    zone_id TEXT,
    source_device_id TEXT,
    latitude DOUBLE PRECISION,
    longitude DOUBLE PRECISION,
    recorded_at TIMESTAMPTZ NOT NULL
)
"#;

const CREATE_PERSON_HISTORY_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_person_history_person_time
ON person_history(person_id, recorded_at DESC)
"#;

const SCHEMA_VERSION_KEY: &str = "schema_version";
const SCHEMA_VERSION_V1: i64 = 1;
const SCHEMA_VERSION_V2: i64 = 2;
const SCHEMA_VERSION_V3: i64 = 3;
const SCHEMA_VERSION_V4: i64 = 4;
const SCHEMA_VERSION_V5: i64 = 5;

// ── History config ─────────────────────────────────────────────────────────────

/// Controls whether history recording is active and for how long rows are kept.
#[derive(Debug, Clone)]
pub struct PostgresHistoryConfig {
    pub enabled: bool,
    pub retention: Option<Duration>,
    pub selection: HistorySelection,
}

impl Default for PostgresHistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention: None,
            selection: HistorySelection::default(),
        }
    }
}

// ── Store struct ───────────────────────────────────────────────────────────────

/// The main Postgres store — holds a connection pool and the history config.
/// Cheaply cloneable (the pool is `Arc`-backed internally by sqlx).
#[derive(Clone)]
pub struct PostgresDeviceStore {
    pool: PgPool,
    history: PostgresHistoryConfig,
}

impl PostgresDeviceStore {
    pub async fn new(database_url: &str, auto_create: bool) -> Result<Self> {
        Self::new_with_history(database_url, auto_create, PostgresHistoryConfig::default()).await
    }

    pub async fn new_with_history(
        database_url: &str,
        auto_create: bool,
        history: PostgresHistoryConfig,
    ) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .with_context(|| {
                format!("failed to connect to PostgreSQL database '{database_url}'")
            })?;

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
            .context("failed to create schema_metadata table")?;

        let schema_version: i64 = sqlx::query("SELECT value FROM schema_metadata WHERE key = $1")
            .bind(SCHEMA_VERSION_KEY)
            .fetch_optional(&self.pool)
            .await
            .context("failed to read schema version")?
            .map(|row| row.get::<String, _>("value"))
            .map(|value| {
                value
                    .parse::<i64>()
                    .with_context(|| format!("invalid schema version value '{value}'"))
            })
            .transpose()?
            .unwrap_or(0);

        if schema_version < 1 {
            self.migrate_to_v1().await?;
        }
        if schema_version < 2 {
            self.migrate_to_v2().await?;
        }
        if schema_version < 3 {
            self.migrate_to_v3().await?;
        }
        if schema_version < 4 {
            self.migrate_to_v4().await?;
        }
        if schema_version < 5 {
            self.migrate_to_v5().await?;
        }

        Ok(())
    }

    async fn set_schema_version(&self, version: i64) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO schema_metadata (key, value)
            VALUES ($1, $2)
            ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value
            "#,
        )
        .bind(SCHEMA_VERSION_KEY)
        .bind(version.to_string())
        .execute(&self.pool)
        .await
        .context("failed to write schema version")?;
        Ok(())
    }

    async fn migrate_to_v1(&self) -> Result<()> {
        sqlx::query(CREATE_ROOMS_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create rooms table")?;

        sqlx::query(CREATE_DEVICES_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create devices table")?;

        sqlx::query(CREATE_DEVICE_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create device_history table")?;

        sqlx::query(CREATE_DEVICE_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create device_history index")?;

        sqlx::query(CREATE_ATTRIBUTE_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create attribute_history table")?;

        sqlx::query(CREATE_ATTRIBUTE_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create attribute_history index")?;

        sqlx::query(CREATE_COMMAND_AUDIT_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create command_audit table")?;

        sqlx::query(CREATE_COMMAND_AUDIT_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create command_audit index")?;

        sqlx::query(CREATE_SCENE_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create scene_execution_history table")?;

        sqlx::query(CREATE_SCENE_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create scene_execution_history index")?;

        sqlx::query(CREATE_AUTOMATION_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create automation_execution_history table")?;

        sqlx::query(CREATE_AUTOMATION_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create automation_execution_history index")?;

        self.set_schema_version(SCHEMA_VERSION_V1).await
    }

    async fn migrate_to_v2(&self) -> Result<()> {
        sqlx::query(CREATE_AUTOMATION_RUNTIME_STATE_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create automation_runtime_state table")?;

        self.set_schema_version(SCHEMA_VERSION_V2).await
    }

    async fn migrate_to_v3(&self) -> Result<()> {
        sqlx::query(CREATE_GROUPS_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create groups table")?;

        sqlx::query(CREATE_GROUP_MEMBERS_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create group_members table")?;

        sqlx::query(CREATE_GROUP_MEMBERS_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create group_members index")?;

        self.set_schema_version(SCHEMA_VERSION_V3).await
    }

    async fn migrate_to_v4(&self) -> Result<()> {
        sqlx::query(CREATE_API_KEYS_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create api_keys table")?;

        self.set_schema_version(SCHEMA_VERSION_V4).await
    }

    async fn migrate_to_v5(&self) -> Result<()> {
        sqlx::query(ALTER_API_KEYS_ADD_PERSON_ID_SQL)
            .execute(&self.pool)
            .await
            .context("failed to add person_id column to api_keys table")?;

        sqlx::query(CREATE_PERSONS_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create persons table")?;

        sqlx::query(CREATE_PERSON_TRACKERS_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create person_trackers table")?;

        sqlx::query(CREATE_PERSON_TRACKERS_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create person_trackers index")?;

        sqlx::query(CREATE_ZONES_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create zones table")?;

        sqlx::query(CREATE_PERSON_HISTORY_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create person_history table")?;

        sqlx::query(CREATE_PERSON_HISTORY_INDEX_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create person_history index")?;

        self.set_schema_version(SCHEMA_VERSION_V5).await
    }

    // ── History helpers ────────────────────────────────────────────────────────

    async fn load_persisted_device(&self, id: &DeviceId) -> Result<Option<Device>> {
        sqlx::query(
            r#"
            SELECT device_id, room_id, kind,
                   attributes_json::text AS attributes_json,
                   metadata_json::text AS metadata_json,
                   updated_at, last_seen
            FROM devices
            WHERE device_id = $1
            "#,
        )
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("failed to load persisted device '{}'", id.0))?
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
            let device_json = serde_json::to_value(device).with_context(|| {
                format!("failed to serialize device history for '{}'", device.id.0)
            })?;
            sqlx::query(
                r#"
                INSERT INTO device_history (device_id, observed_at, device_json)
                VALUES ($1, $2, $3)
                ON CONFLICT (device_id, observed_at) DO UPDATE SET device_json = EXCLUDED.device_json
                "#,
            )
            .bind(&device.id.0)
            .bind(observed_at)
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

            let value_json = serde_json::to_value(value).with_context(|| {
                format!(
                    "failed to serialize attribute history for '{}' attribute '{}'",
                    device.id.0, attribute
                )
            })?;
            sqlx::query(
                r#"
                INSERT INTO attribute_history (device_id, attribute, observed_at, value_json)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (device_id, attribute, observed_at) DO UPDATE SET value_json = EXCLUDED.value_json
                "#,
            )
            .bind(&device.id.0)
            .bind(attribute)
            .bind(observed_at)
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

        Ok(())
    }

    pub async fn prune_history_impl(&self) -> Result<()> {
        let Some(retention) = self.history.retention else {
            return Ok(());
        };

        let cutoff = chrono::Duration::from_std(retention)
            .context("invalid PostgreSQL history retention duration")?;
        let cutoff: DateTime<Utc> = Utc::now() - cutoff;

        sqlx::query("DELETE FROM device_history WHERE observed_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .context("failed to prune PostgreSQL device history")?;

        sqlx::query("DELETE FROM attribute_history WHERE observed_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .context("failed to prune PostgreSQL attribute history")?;

        Ok(())
    }

    fn should_record_device(&self, device: &Device) -> bool {
        hf::should_record_device(&self.history.selection, device)
    }

    fn should_record_attribute(&self, device: &Device, attribute: &str) -> bool {
        hf::should_record_attribute(&self.history.selection, device, attribute)
    }

    fn should_record_command_audit(&self, entry: &CommandAuditEntry) -> bool {
        hf::should_record_command_audit(&self.history.selection, entry)
    }

    fn should_record_scene_execution(&self, entry: &SceneExecutionHistoryEntry) -> bool {
        hf::should_record_scene_execution(&self.history.selection, entry)
    }

    fn should_record_automation_execution(&self, entry: &AutomationExecutionHistoryEntry) -> bool {
        hf::should_record_automation_execution(&self.history.selection, entry)
    }
}

// ── DeviceStore impl ───────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl DeviceStore for PostgresDeviceStore {
    async fn load_all_devices(&self) -> Result<Vec<Device>> {
        let rows = sqlx::query(
            r#"
            SELECT device_id, room_id, kind,
                   attributes_json::text AS attributes_json,
                   metadata_json::text AS metadata_json,
                   updated_at, last_seen
            FROM devices
            ORDER BY device_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load devices from PostgreSQL")?;

        rows.into_iter().map(device_from_row).collect()
    }

    async fn load_all_rooms(&self) -> Result<Vec<Room>> {
        let rows = sqlx::query("SELECT id, name FROM rooms ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .context("failed to load rooms from PostgreSQL")?;

        rows.into_iter().map(room_from_row).collect()
    }

    async fn load_all_groups(&self) -> Result<Vec<DeviceGroup>> {
        let rows = sqlx::query(
            r#"
            SELECT g.id, g.name, gm.device_id, gm.member_order
            FROM groups g
            LEFT JOIN group_members gm ON gm.group_id = g.id
            ORDER BY g.id, gm.member_order ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load groups from PostgreSQL")?;

        Ok(groups_from_rows(rows))
    }

    async fn save_device(&self, device: &Device) -> Result<()> {
        let previous = self.load_persisted_device(&device.id).await?;
        let attributes_json = serde_json::to_value(&device.attributes)
            .with_context(|| format!("failed to serialize attributes for '{}'", device.id.0))?;
        let metadata_json = serde_json::to_value(&device.metadata)
            .with_context(|| format!("failed to serialize metadata for '{}'", device.id.0))?;

        sqlx::query(
            r#"
            INSERT INTO devices (device_id, room_id, kind, attributes_json, metadata_json, updated_at, last_seen)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (device_id) DO UPDATE SET
                room_id = EXCLUDED.room_id,
                kind = EXCLUDED.kind,
                attributes_json = EXCLUDED.attributes_json,
                metadata_json = EXCLUDED.metadata_json,
                updated_at = EXCLUDED.updated_at,
                last_seen = EXCLUDED.last_seen
            "#,
        )
        .bind(&device.id.0)
        .bind(device.room_id.as_ref().map(|id| id.0.as_str()))
        .bind(device_kind_to_str(&device.kind))
        .bind(attributes_json)
        .bind(metadata_json)
        .bind(device.updated_at)
        .bind(device.last_seen)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to save device '{}' to PostgreSQL", device.id.0))?;

        self.persist_history_if_needed(device, previous.as_ref())
            .await?;

        Ok(())
    }

    async fn save_room(&self, room: &Room) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO rooms (id, name)
            VALUES ($1, $2)
            ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name
            "#,
        )
        .bind(&room.id.0)
        .bind(&room.name)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to save room '{}' to PostgreSQL", room.id.0))?;

        Ok(())
    }

    async fn save_group(&self, group: &DeviceGroup) -> Result<()> {
        let mut tx = self.pool.begin().await.with_context(|| {
            format!("failed to start transaction to save group '{}'", group.id.0)
        })?;

        sqlx::query(
            r#"
            INSERT INTO groups (id, name)
            VALUES ($1, $2)
            ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name
            "#,
        )
        .bind(&group.id.0)
        .bind(&group.name)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("failed to save group '{}' to PostgreSQL", group.id.0))?;

        sqlx::query("DELETE FROM group_members WHERE group_id = $1")
            .bind(&group.id.0)
            .execute(&mut *tx)
            .await
            .with_context(|| {
                format!(
                    "failed to clear existing members for group '{}' in PostgreSQL",
                    group.id.0
                )
            })?;

        for (order, member) in group.members.iter().enumerate() {
            sqlx::query(
                r#"
                INSERT INTO group_members (group_id, device_id, member_order)
                VALUES ($1, $2, $3)
                "#,
            )
            .bind(&group.id.0)
            .bind(&member.0)
            .bind(order as i64)
            .execute(&mut *tx)
            .await
            .with_context(|| {
                format!(
                    "failed to save member '{}' for group '{}' to PostgreSQL",
                    member.0, group.id.0
                )
            })?;
        }

        tx.commit()
            .await
            .with_context(|| format!("failed to commit group '{}' transaction", group.id.0))?;

        Ok(())
    }

    async fn delete_device(&self, id: &DeviceId) -> Result<()> {
        sqlx::query("DELETE FROM devices WHERE device_id = $1")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete device '{}' from PostgreSQL", id.0))?;

        Ok(())
    }

    async fn delete_room(&self, id: &RoomId) -> Result<()> {
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete room '{}' from PostgreSQL", id.0))?;

        Ok(())
    }

    async fn delete_group(&self, id: &GroupId) -> Result<()> {
        sqlx::query("DELETE FROM groups WHERE id = $1")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete group '{}' from PostgreSQL", id.0))?;

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
            SELECT observed_at, device_json::text AS device_json
            FROM device_history
            WHERE device_id = $1
              AND ($2::TIMESTAMPTZ IS NULL OR observed_at >= $2)
              AND ($3::TIMESTAMPTZ IS NULL OR observed_at <= $3)
            ORDER BY observed_at DESC
            LIMIT $4
            "#,
        )
        .bind(&id.0)
        .bind(start)
        .bind(end)
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
            SELECT device_id, attribute, observed_at, value_json::text AS value_json
            FROM attribute_history
            WHERE device_id = $1
              AND attribute = $2
              AND ($3::TIMESTAMPTZ IS NULL OR observed_at >= $3)
              AND ($4::TIMESTAMPTZ IS NULL OR observed_at <= $4)
            ORDER BY observed_at DESC
            LIMIT $5
            "#,
        )
        .bind(&id.0)
        .bind(attribute)
        .bind(start)
        .bind(end)
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

        let command_json = serde_json::to_value(&entry.command)
            .context("failed to serialize command audit entry")?;

        sqlx::query(
            r#"
            INSERT INTO command_audit (recorded_at, source, room_id, device_id, command_json, status, message)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(entry.recorded_at)
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
            SELECT recorded_at, source, room_id, device_id, command_json::text AS command_json, status, message
            FROM command_audit
            WHERE ($1::TEXT IS NULL OR device_id = $1)
              AND ($2::TIMESTAMPTZ IS NULL OR recorded_at >= $2)
              AND ($3::TIMESTAMPTZ IS NULL OR recorded_at <= $3)
            ORDER BY recorded_at DESC, id DESC
            LIMIT $4
            "#,
        )
        .bind(device_id.map(|v| v.0.as_str()))
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load command audit from PostgreSQL")?;

        rows.into_iter().map(command_audit_from_row).collect()
    }

    async fn save_scene_execution(&self, entry: &SceneExecutionHistoryEntry) -> Result<()> {
        if !self.history.enabled || !self.should_record_scene_execution(entry) {
            return Ok(());
        }

        let results_json = serde_json::to_value(&entry.results)
            .context("failed to serialize scene execution history entry")?;

        sqlx::query(
            r#"
            INSERT INTO scene_execution_history (executed_at, scene_id, status, error, results_json)
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(entry.executed_at)
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
            SELECT executed_at, scene_id, status, error, results_json::text AS results_json
            FROM scene_execution_history
            WHERE scene_id = $1
              AND ($2::TIMESTAMPTZ IS NULL OR executed_at >= $2)
              AND ($3::TIMESTAMPTZ IS NULL OR executed_at <= $3)
            ORDER BY executed_at DESC, id DESC
            LIMIT $4
            "#,
        )
        .bind(scene_id)
        .bind(start)
        .bind(end)
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

        let trigger_payload_json = serde_json::to_value(&entry.trigger_payload)
            .context("failed to serialize automation trigger payload")?;
        let results_json = serde_json::to_value(&entry.results)
            .context("failed to serialize automation execution history entry")?;

        sqlx::query(
            r#"
            INSERT INTO automation_execution_history
                (executed_at, automation_id, trigger_payload_json, status, duration_ms, error, results_json)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(entry.executed_at)
        .bind(&entry.automation_id)
        .bind(trigger_payload_json)
        .bind(&entry.status)
        .bind(entry.duration_ms)
        .bind(&entry.error)
        .bind(results_json)
        .execute(&self.pool)
        .await
        .with_context(|| {
            format!(
                "failed to save automation execution history for '{}'",
                entry.automation_id
            )
        })?;

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
            SELECT executed_at, automation_id,
                   trigger_payload_json::text AS trigger_payload_json,
                   status, duration_ms, error,
                   results_json::text AS results_json
            FROM automation_execution_history
            WHERE automation_id = $1
              AND ($2::TIMESTAMPTZ IS NULL OR executed_at >= $2)
              AND ($3::TIMESTAMPTZ IS NULL OR executed_at <= $3)
            ORDER BY executed_at DESC, id DESC
            LIMIT $4
            "#,
        )
        .bind(automation_id)
        .bind(start)
        .bind(end)
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
            SELECT automation_id, updated_at, last_triggered_at,
                   last_trigger_fingerprint, last_scheduled_at
            FROM automation_runtime_state
            WHERE automation_id = $1
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
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (automation_id) DO UPDATE SET
                updated_at = EXCLUDED.updated_at,
                last_triggered_at = EXCLUDED.last_triggered_at,
                last_trigger_fingerprint = EXCLUDED.last_trigger_fingerprint,
                last_scheduled_at = EXCLUDED.last_scheduled_at
            "#,
        )
        .bind(&state.automation_id)
        .bind(state.updated_at)
        .bind(state.last_triggered_at)
        .bind(&state.last_trigger_fingerprint)
        .bind(state.last_scheduled_at)
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

    async fn prune_history(&self) -> Result<()> {
        self.prune_history_impl().await
    }
}

// ── ApiKeyStore impl ───────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl ApiKeyStore for PostgresDeviceStore {
    async fn create_api_key(
        &self,
        key_hash: &str,
        label: &str,
        role: ApiKeyRole,
        person_id: Option<&PersonId>,
    ) -> Result<ApiKeyRecord> {
        let now = Utc::now();
        let role_str = api_key_role_to_str(role);
        let person_id_val = person_id.map(|p| p.0.as_str());

        let row = sqlx::query(
            r#"
            INSERT INTO api_keys (key_hash, label, role, created_at, person_id)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id, key_hash, label, role, created_at, last_used_at, person_id
            "#,
        )
        .bind(key_hash)
        .bind(label)
        .bind(role_str)
        .bind(now)
        .bind(person_id_val)
        .fetch_one(&self.pool)
        .await
        .context("failed to insert api key")?;

        api_key_from_row(row)
    }

    async fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>> {
        let rows = sqlx::query(
            "SELECT id, key_hash, label, role, created_at, last_used_at, person_id FROM api_keys ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to list api keys")?;

        rows.into_iter().map(api_key_from_row).collect()
    }

    async fn list_api_keys_for_person(
        &self,
        person_id: &PersonId,
    ) -> Result<Vec<ApiKeyRecord>> {
        let rows = sqlx::query(
            "SELECT id, key_hash, label, role, created_at, last_used_at, person_id FROM api_keys WHERE person_id = $1 ORDER BY id ASC",
        )
        .bind(&person_id.0)
        .fetch_all(&self.pool)
        .await
        .context("failed to list api keys for person")?;

        rows.into_iter().map(api_key_from_row).collect()
    }

    async fn revoke_api_key(&self, id: i64) -> Result<bool> {
        let result = sqlx::query("DELETE FROM api_keys WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("failed to revoke api key")?;

        Ok(result.rows_affected() > 0)
    }

    async fn lookup_api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRecord>> {
        let row = sqlx::query(
            "SELECT id, key_hash, label, role, created_at, last_used_at, person_id FROM api_keys WHERE key_hash = $1",
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await
        .context("failed to lookup api key by hash")?;

        row.map(api_key_from_row).transpose()
    }

    async fn touch_api_key(&self, id: i64) -> Result<()> {
        let now = Utc::now();
        sqlx::query("UPDATE api_keys SET last_used_at = $1 WHERE id = $2")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .context("failed to update api key last_used_at")?;

        Ok(())
    }
}

// ── Row decoders ───────────────────────────────────────────────────────────────

fn device_from_row(row: sqlx::postgres::PgRow) -> Result<Device> {
    let id: String = row.get("device_id");
    let room_id = row.get::<Option<String>, _>("room_id").map(RoomId);
    let kind = device_kind_from_str(&row.get::<String, _>("kind"))
        .with_context(|| format!("invalid device kind for '{id}'"))?;
    let attributes: Attributes = serde_json::from_str(&row.get::<String, _>("attributes_json"))
        .with_context(|| format!("invalid attributes JSON for '{id}'"))?;
    let metadata: Metadata = serde_json::from_str(&row.get::<String, _>("metadata_json"))
        .with_context(|| format!("invalid metadata JSON for '{id}'"))?;
    let updated_at: DateTime<Utc> = row.get("updated_at");
    let last_seen: DateTime<Utc> = row.get("last_seen");

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

fn device_history_from_row(row: sqlx::postgres::PgRow) -> Result<DeviceHistoryEntry> {
    let observed_at: DateTime<Utc> = row.get("observed_at");
    let device: Device = serde_json::from_str(&row.get::<String, _>("device_json"))
        .context("invalid device history JSON")?;

    Ok(DeviceHistoryEntry {
        observed_at,
        device,
    })
}

fn attribute_history_from_row(row: sqlx::postgres::PgRow) -> Result<AttributeHistoryEntry> {
    let device_id = DeviceId(row.get::<String, _>("device_id"));
    let attribute: String = row.get("attribute");
    let observed_at: DateTime<Utc> = row.get("observed_at");
    let value: AttributeValue = serde_json::from_str(&row.get::<String, _>("value_json"))
        .context("invalid attribute history JSON")?;

    Ok(AttributeHistoryEntry {
        observed_at,
        device_id,
        attribute,
        value,
    })
}

fn command_audit_from_row(row: sqlx::postgres::PgRow) -> Result<CommandAuditEntry> {
    let recorded_at: DateTime<Utc> = row.get("recorded_at");
    let command = serde_json::from_str(&row.get::<String, _>("command_json"))
        .context("invalid command audit JSON")?;

    Ok(CommandAuditEntry {
        recorded_at,
        source: row.get("source"),
        room_id: row.get::<Option<String>, _>("room_id").map(RoomId),
        device_id: DeviceId(row.get::<String, _>("device_id")),
        command,
        status: row.get("status"),
        message: row.get("message"),
    })
}

fn scene_history_from_row(row: sqlx::postgres::PgRow) -> Result<SceneExecutionHistoryEntry> {
    let executed_at: DateTime<Utc> = row.get("executed_at");
    let results = serde_json::from_str(&row.get::<String, _>("results_json"))
        .context("invalid scene execution history JSON")?;

    Ok(SceneExecutionHistoryEntry {
        executed_at,
        scene_id: row.get("scene_id"),
        status: row.get("status"),
        error: row.get("error"),
        results,
    })
}

fn automation_history_from_row(
    row: sqlx::postgres::PgRow,
) -> Result<AutomationExecutionHistoryEntry> {
    let executed_at: DateTime<Utc> = row.get("executed_at");
    let trigger_payload = serde_json::from_str(&row.get::<String, _>("trigger_payload_json"))
        .context("invalid automation trigger payload JSON")?;
    let results = serde_json::from_str(&row.get::<String, _>("results_json"))
        .context("invalid automation execution history JSON")?;

    Ok(AutomationExecutionHistoryEntry {
        executed_at,
        automation_id: row.get("automation_id"),
        trigger_payload,
        status: row.get("status"),
        duration_ms: row.get("duration_ms"),
        error: row.get("error"),
        results,
    })
}

fn automation_runtime_state_from_row(row: sqlx::postgres::PgRow) -> Result<AutomationRuntimeState> {
    Ok(AutomationRuntimeState {
        automation_id: row.get("automation_id"),
        updated_at: row.get("updated_at"),
        last_triggered_at: row.get("last_triggered_at"),
        last_trigger_fingerprint: row.get("last_trigger_fingerprint"),
        last_scheduled_at: row.get("last_scheduled_at"),
    })
}

fn room_from_row(row: sqlx::postgres::PgRow) -> Result<Room> {
    Ok(Room {
        id: RoomId(row.get::<String, _>("id")),
        name: row.get("name"),
    })
}

fn groups_from_rows(rows: Vec<sqlx::postgres::PgRow>) -> Vec<DeviceGroup> {
    let mut groups: Vec<DeviceGroup> = Vec::new();

    for row in rows {
        let id: String = row.get("id");
        let name: String = row.get("name");
        let member_device_id: Option<String> = row.get("device_id");

        let group_id = GroupId(id.clone());
        if groups
            .last()
            .map(|group| group.id != group_id)
            .unwrap_or(true)
        {
            groups.push(DeviceGroup {
                id: group_id,
                name,
                members: Vec::new(),
            });
        }

        if let Some(member_device_id) = member_device_id {
            groups
                .last_mut()
                .expect("group list is initialized")
                .members
                .push(DeviceId(member_device_id));
        }
    }

    groups
}

// ── Utilities ──────────────────────────────────────────────────────────────────

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

fn api_key_role_to_str(role: ApiKeyRole) -> &'static str {
    match role {
        ApiKeyRole::Read => "read",
        ApiKeyRole::Write => "write",
        ApiKeyRole::Admin => "admin",
        ApiKeyRole::Automation => "automation",
    }
}

fn api_key_role_from_str(value: &str) -> Result<ApiKeyRole> {
    match value {
        "read" => Ok(ApiKeyRole::Read),
        "write" => Ok(ApiKeyRole::Write),
        "admin" => Ok(ApiKeyRole::Admin),
        "automation" => Ok(ApiKeyRole::Automation),
        other => anyhow::bail!("unsupported api key role '{other}'"),
    }
}

fn api_key_from_row(row: sqlx::postgres::PgRow) -> Result<ApiKeyRecord> {
    let id: i64 = row.get("id");
    let key_hash: String = row.get("key_hash");
    let label: String = row.get("label");
    let role_str: String = row.get("role");
    let role = api_key_role_from_str(&role_str)
        .with_context(|| format!("invalid api key role '{role_str}' for key {id}"))?;
    let created_at: DateTime<Utc> = row.get("created_at");
    let last_used_at: Option<DateTime<Utc>> = row.get("last_used_at");
    let person_id: Option<String> = row.get("person_id");

    Ok(ApiKeyRecord {
        id,
        key_hash,
        label,
        role,
        person_id: person_id.map(PersonId),
        created_at,
        last_used_at,
    })
}

// ---------------------------------------------------------------------------
// PersonStore impl for PostgresDeviceStore
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl PersonStore for PostgresDeviceStore {
    async fn upsert_person(&self, person: &Person) -> anyhow::Result<()> {
        let state_json = serde_json::to_value(&person.state)
            .context("failed to serialize PersonState")?;
        sqlx::query(
            r#"
            INSERT INTO persons (id, name, picture, state_json, state_source, latitude, longitude, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (id) DO UPDATE SET
                name         = EXCLUDED.name,
                picture      = EXCLUDED.picture,
                state_json   = EXCLUDED.state_json,
                state_source = EXCLUDED.state_source,
                latitude     = EXCLUDED.latitude,
                longitude    = EXCLUDED.longitude,
                updated_at   = EXCLUDED.updated_at
            "#,
        )
        .bind(&person.id.0)
        .bind(&person.name)
        .bind(&person.picture)
        .bind(state_json)
        .bind(person.state_source.as_ref().map(|d| &d.0))
        .bind(person.latitude)
        .bind(person.longitude)
        .bind(person.updated_at)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to upsert person '{}'", person.id.0))?;

        Ok(())
    }

    async fn load_person(&self, id: &PersonId) -> anyhow::Result<Option<Person>> {
        let row = sqlx::query(
            "SELECT id, name, picture, state_json::text AS state_json, state_source, latitude, longitude, updated_at FROM persons WHERE id = $1",
        )
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("failed to load person '{}'", id.0))?;

        let Some(row) = row else { return Ok(None) };
        let trackers = self.load_person_trackers_inner(id).await?;
        person_from_row(row, trackers).map(Some)
    }

    async fn load_all_persons(&self) -> anyhow::Result<Vec<Person>> {
        let rows = sqlx::query(
            "SELECT id, name, picture, state_json::text AS state_json, state_source, latitude, longitude, updated_at FROM persons ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load all persons")?;

        let mut persons = Vec::with_capacity(rows.len());
        for row in rows {
            let pid: String = row.get("id");
            let trackers = self.load_person_trackers_inner(&PersonId(pid.clone())).await?;
            persons.push(person_from_row(row, trackers)?);
        }
        Ok(persons)
    }

    async fn delete_person(&self, id: &PersonId) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM persons WHERE id = $1")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete person '{}'", id.0))?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_person_trackers(
        &self,
        person_id: &PersonId,
        trackers: &[DeviceId],
    ) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM person_trackers WHERE person_id = $1")
            .bind(&person_id.0)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete trackers for person '{}'", person_id.0))?;

        for (order, device_id) in trackers.iter().enumerate() {
            sqlx::query(
                "INSERT INTO person_trackers (person_id, device_id, tracker_order) VALUES ($1, $2, $3)",
            )
            .bind(&person_id.0)
            .bind(&device_id.0)
            .bind(order as i64)
            .execute(&self.pool)
            .await
            .with_context(|| {
                format!(
                    "failed to insert tracker '{}' for person '{}'",
                    device_id.0, person_id.0
                )
            })?;
        }
        Ok(())
    }

    async fn upsert_zone(&self, zone: &Zone) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            INSERT INTO zones (id, name, latitude, longitude, radius_meters, icon, passive)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (id) DO UPDATE SET
                name          = EXCLUDED.name,
                latitude      = EXCLUDED.latitude,
                longitude     = EXCLUDED.longitude,
                radius_meters = EXCLUDED.radius_meters,
                icon          = EXCLUDED.icon,
                passive       = EXCLUDED.passive
            "#,
        )
        .bind(&zone.id.0)
        .bind(&zone.name)
        .bind(zone.latitude)
        .bind(zone.longitude)
        .bind(zone.radius_meters)
        .bind(&zone.icon)
        .bind(zone.passive)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to upsert zone '{}'", zone.id.0))?;

        Ok(())
    }

    async fn load_zone(&self, id: &ZoneId) -> anyhow::Result<Option<Zone>> {
        let row = sqlx::query(
            "SELECT id, name, latitude, longitude, radius_meters, icon, passive FROM zones WHERE id = $1",
        )
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("failed to load zone '{}'", id.0))?;

        row.map(zone_from_row).transpose()
    }

    async fn load_all_zones(&self) -> anyhow::Result<Vec<Zone>> {
        let rows = sqlx::query(
            "SELECT id, name, latitude, longitude, radius_meters, icon, passive FROM zones ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load all zones")?;

        rows.into_iter().map(zone_from_row).collect()
    }

    async fn delete_zone(&self, id: &ZoneId) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM zones WHERE id = $1")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete zone '{}'", id.0))?;
        Ok(result.rows_affected() > 0)
    }

    async fn record_person_history(&self, entry: &PersonHistoryEntry) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            INSERT INTO person_history (person_id, state, zone_id, source_device_id, latitude, longitude, recorded_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(&entry.person_id.0)
        .bind(&entry.state)
        .bind(entry.zone_id.as_ref().map(|z| &z.0))
        .bind(entry.source_device_id.as_ref().map(|d| &d.0))
        .bind(entry.latitude)
        .bind(entry.longitude)
        .bind(entry.recorded_at)
        .execute(&self.pool)
        .await
        .with_context(|| {
            format!("failed to record person history for '{}'", entry.person_id.0)
        })?;

        Ok(())
    }

    async fn load_person_history(
        &self,
        person_id: &PersonId,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: usize,
    ) -> anyhow::Result<Vec<PersonHistoryEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT id, person_id, state, zone_id, source_device_id, latitude, longitude, recorded_at
            FROM person_history
            WHERE person_id = $1
              AND ($2::TIMESTAMPTZ IS NULL OR recorded_at >= $2)
              AND ($3::TIMESTAMPTZ IS NULL OR recorded_at <= $3)
            ORDER BY recorded_at DESC
            LIMIT $4
            "#,
        )
        .bind(&person_id.0)
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("failed to load person history for '{}'", person_id.0))?;

        rows.into_iter().map(person_history_from_row).collect()
    }

    async fn prune_person_history(&self) -> anyhow::Result<()> {
        let Some(retention) = self.history.retention else {
            return Ok(());
        };
        let cutoff = chrono::Duration::from_std(retention)
            .context("invalid history retention duration")?;
        let cutoff = Utc::now() - cutoff;
        sqlx::query("DELETE FROM person_history WHERE recorded_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .context("failed to prune person history")?;
        Ok(())
    }
}

// ── Person/zone row helpers ────────────────────────────────────────────────────

impl PostgresDeviceStore {
    async fn load_person_trackers_inner(&self, person_id: &PersonId) -> Result<Vec<DeviceId>> {
        let rows = sqlx::query(
            "SELECT device_id FROM person_trackers WHERE person_id = $1 ORDER BY tracker_order ASC",
        )
        .bind(&person_id.0)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("failed to load trackers for person '{}'", person_id.0))?;

        Ok(rows
            .into_iter()
            .map(|r| DeviceId(r.get::<String, _>("device_id")))
            .collect())
    }
}

fn person_from_row(row: sqlx::postgres::PgRow, trackers: Vec<DeviceId>) -> Result<Person> {
    let id: String = row.get("id");
    let name: String = row.get("name");
    let picture: Option<String> = row.get("picture");
    let state_json: String = row.get("state_json");
    let state: PersonState = serde_json::from_str(&state_json)
        .with_context(|| format!("invalid state_json for person '{id}'"))?;
    let state_source: Option<String> = row.get("state_source");
    let latitude: Option<f64> = row.get("latitude");
    let longitude: Option<f64> = row.get("longitude");
    let updated_at: DateTime<Utc> = row.get("updated_at");

    Ok(Person {
        id: PersonId(id),
        name,
        picture,
        trackers,
        state,
        state_source: state_source.map(DeviceId),
        latitude,
        longitude,
        updated_at,
    })
}

fn zone_from_row(row: sqlx::postgres::PgRow) -> Result<Zone> {
    let id: String = row.get("id");
    let name: String = row.get("name");
    let latitude: f64 = row.get("latitude");
    let longitude: f64 = row.get("longitude");
    let radius_meters: f64 = row.get("radius_meters");
    let icon: Option<String> = row.get("icon");
    let passive: bool = row.get("passive");

    Ok(Zone {
        id: ZoneId(id),
        name,
        latitude,
        longitude,
        radius_meters,
        icon,
        passive,
    })
}

fn person_history_from_row(row: sqlx::postgres::PgRow) -> Result<PersonHistoryEntry> {
    let id: i64 = row.get("id");
    let person_id: String = row.get("person_id");
    let state: String = row.get("state");
    let zone_id: Option<String> = row.get("zone_id");
    let source_device_id: Option<String> = row.get("source_device_id");
    let latitude: Option<f64> = row.get("latitude");
    let longitude: Option<f64> = row.get("longitude");
    let recorded_at: DateTime<Utc> = row.get("recorded_at");

    Ok(PersonHistoryEntry {
        id,
        person_id: PersonId(person_id),
        state,
        zone_id: zone_id.map(ZoneId),
        source_device_id: source_device_id.map(DeviceId),
        latitude,
        longitude,
        recorded_at,
    })
}
