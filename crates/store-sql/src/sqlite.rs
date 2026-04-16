use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use smart_home_core::model::{Attributes, Device, DeviceId, DeviceKind, Metadata};
use smart_home_core::store::DeviceStore;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

const CREATE_DEVICES_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS devices (
    device_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    attributes_json TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_seen TEXT NOT NULL
)
"#;

#[derive(Clone)]
pub struct SqliteDeviceStore {
    pool: SqlitePool,
}

impl SqliteDeviceStore {
    pub async fn new(database_url: &str, auto_create: bool) -> Result<Self> {
        let options = sqlite_connect_options(database_url, auto_create)?;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .with_context(|| format!("failed to connect to SQLite database '{database_url}'"))?;

        let store = Self { pool };

        if auto_create {
            store.initialize().await?;
        }

        Ok(store)
    }

    pub async fn initialize(&self) -> Result<()> {
        sqlx::query(CREATE_DEVICES_TABLE_SQL)
            .execute(&self.pool)
            .await
            .context("failed to create SQLite devices table")?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl DeviceStore for SqliteDeviceStore {
    async fn load_all_devices(&self) -> Result<Vec<Device>> {
        let rows = sqlx::query(
            r#"
            SELECT device_id, kind, attributes_json, metadata_json, updated_at, last_seen
            FROM devices
            ORDER BY device_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load devices from SQLite")?;

        rows.into_iter().map(device_from_row).collect()
    }

    async fn save_device(&self, device: &Device) -> Result<()> {
        let attributes_json = serde_json::to_string(&device.attributes)
            .with_context(|| format!("failed to serialize attributes for '{}'", device.id.0))?;
        let metadata_json = serde_json::to_string(&device.metadata)
            .with_context(|| format!("failed to serialize metadata for '{}'", device.id.0))?;

        sqlx::query(
            r#"
            INSERT INTO devices (device_id, kind, attributes_json, metadata_json, updated_at, last_seen)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(device_id) DO UPDATE SET
                kind = excluded.kind,
                attributes_json = excluded.attributes_json,
                metadata_json = excluded.metadata_json,
                updated_at = excluded.updated_at,
                last_seen = excluded.last_seen
            "#,
        )
        .bind(&device.id.0)
        .bind(device_kind_to_str(&device.kind))
        .bind(attributes_json)
        .bind(metadata_json)
        .bind(device.updated_at.to_rfc3339())
        .bind(device.last_seen.to_rfc3339())
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to save device '{}' to SQLite", device.id.0))?;

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
        kind,
        attributes,
        metadata,
        updated_at,
        last_seen,
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::Utc;
    use smart_home_core::capability::{TEMPERATURE_OUTDOOR, measurement_value};
    use smart_home_core::model::{AttributeValue, DeviceKind};

    use super::*;

    fn sample_device(id: &str, value: f64) -> Device {
        let mut vendor_specific = HashMap::new();
        vendor_specific.insert("provider".to_string(), serde_json::json!("test"));

        Device {
            id: DeviceId(id.to_string()),
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
                location: Some("lab".to_string()),
                accuracy: Some(0.9),
                vendor_specific,
            },
            updated_at: Utc::now(),
            last_seen: Utc::now(),
        }
    }

    async fn temp_store() -> SqliteDeviceStore {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("smart-home-store-{unique}.db"));
        let url = format!("sqlite://{}", path.display());

        SqliteDeviceStore::new(&url, true)
            .await
            .expect("temporary sqlite store initializes")
    }

    #[tokio::test]
    async fn save_and_load_single_device() {
        let store = temp_store().await;
        let device = sample_device("test:one", 20.0);

        store.save_device(&device).await.expect("save succeeds");

        assert_eq!(store.load_all_devices().await.expect("load succeeds"), vec![device]);
    }

    #[tokio::test]
    async fn save_overwrites_existing_device_by_id() {
        let store = temp_store().await;
        let original = sample_device("test:one", 20.0);
        let updated = sample_device("test:one", 21.5);

        store.save_device(&original).await.expect("initial save succeeds");
        store.save_device(&updated).await.expect("update save succeeds");

        assert_eq!(store.load_all_devices().await.expect("load succeeds"), vec![updated]);
    }

    #[tokio::test]
    async fn delete_removes_device_by_id() {
        let store = temp_store().await;
        let device = sample_device("test:one", 20.0);

        store.save_device(&device).await.expect("save succeeds");
        store
            .delete_device(&device.id)
            .await
            .expect("delete succeeds");

        assert!(store.load_all_devices().await.expect("load succeeds").is_empty());
    }

    #[tokio::test]
    async fn load_multiple_devices() {
        let store = temp_store().await;
        let device_a = sample_device("test:a", 20.0);
        let device_b = sample_device("test:b", 21.0);

        store.save_device(&device_b).await.expect("save b succeeds");
        store.save_device(&device_a).await.expect("save a succeeds");

        assert_eq!(
            store.load_all_devices().await.expect("load succeeds"),
            vec![device_a, device_b]
        );
    }
}
