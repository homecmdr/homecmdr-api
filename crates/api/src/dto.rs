//! Data Transfer Objects (DTOs) — the shapes of JSON the API sends and receives.
//!
//! Every request body and every response body is defined here as a plain Rust
//! struct.  Serde handles the conversion to/from JSON automatically.
//!
//! "Request" structs are what the client sends to us (deserialized from JSON).
//! "Response" structs are what we send back to the client (serialized to JSON).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use homecmdr_core::capability::{CapabilityOwnershipPolicy, CapabilitySchema};
use homecmdr_core::model::AttributeValue;
use homecmdr_core::store::{
    ApiKeyRole, AttributeHistoryEntry, AutomationExecutionHistoryEntry, CommandAuditEntry,
    DeviceHistoryEntry, PersonHistoryEntry, SceneExecutionHistoryEntry, SceneStepResult,
};
use homecmdr_plugin_host::PluginManifest;
use homecmdr_scenes::SceneExecutionResult;
use serde::{Deserialize, Serialize};
use serde_json::json;

// ── Adapter / health shapes ───────────────────────────────────────────────────

/// One row in the adapter list returned by `GET /adapters`.
#[derive(Clone, Debug, Serialize)]
pub struct AdapterSummary {
    pub name: String,
    pub status: String,
    pub message: Option<String>,
    pub last_updated: Option<DateTime<Utc>>,
    pub last_success: Option<DateTime<Utc>>,
    pub last_error: Option<AdapterErrorSnapshot>,
}

/// The most recent error recorded for an adapter.
#[derive(Clone, Debug, Serialize)]
pub struct AdapterErrorSnapshot {
    pub message: String,
    pub observed_at: DateTime<Utc>,
}

/// Full detail for a single adapter returned by `GET /adapters/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct AdapterDetailResponse {
    pub name: String,
    pub runtime_status: String,
    pub health: ComponentStatus,
    pub last_success: Option<DateTime<Utc>>,
    pub last_error: Option<AdapterErrorSnapshot>,
}

/// Generic status for any system component (runtime, persistence, an adapter, …).
/// `status` is one of `"ok"`, `"starting"`, or `"error"`.
#[derive(Clone, Debug, Serialize)]
pub struct ComponentStatus {
    pub status: String,
    pub message: Option<String>,
    pub last_updated: Option<DateTime<Utc>>,
}

impl ComponentStatus {
    pub fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            message: None,
            last_updated: Some(Utc::now()),
        }
    }

    pub fn starting(message: impl Into<String>) -> Self {
        Self {
            status: "starting".to_string(),
            message: Some(message.into()),
            last_updated: Some(Utc::now()),
        }
    }

    pub fn set_ok(&mut self) {
        self.status = "ok".to_string();
        self.message = None;
        self.last_updated = Some(Utc::now());
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.status = "error".to_string();
        self.message = Some(message.into());
        self.last_updated = Some(Utc::now());
    }
}

/// Response body for `GET /health`.
#[derive(Clone, Serialize)]
pub struct HealthResponse {
    /// Overall status: `"ok"` when every component is healthy, `"degraded"` otherwise.
    pub status: String,
    /// `true` once startup has finished and all components are reporting `"ok"`.
    pub ready: bool,
    pub runtime: ComponentStatus,
    pub persistence: ComponentStatus,
    pub automations: ComponentStatus,
    pub adapters: Vec<AdapterSummary>,
}

/// Response body for `GET /ready`.
#[derive(Clone, Serialize)]
pub struct ReadyResponse {
    pub status: &'static str,
}

// ── Room / group / device request shapes ─────────────────────────────────────

/// Request body for `POST /rooms`.
#[derive(Debug, Deserialize)]
pub struct CreateRoomRequest {
    pub id: String,
    pub name: String,
}

/// Request body for `POST /devices/{id}/room`.  `room_id: null` unassigns the device.
#[derive(Debug, Deserialize)]
pub struct AssignRoomRequest {
    pub room_id: Option<String>,
}

/// Request body for `POST /groups`.
#[derive(Debug, Deserialize)]
pub struct CreateGroupRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
}

/// Request body for `POST /groups/{id}/members` — replaces the whole member list.
#[derive(Debug, Deserialize)]
pub struct SetGroupMembersRequest {
    #[serde(default)]
    pub members: Vec<String>,
}

// ── Automation request shapes ─────────────────────────────────────────────────

/// Request body for `POST /automations/{id}/enabled`.
#[derive(Debug, Deserialize)]
pub struct AutomationEnabledRequest {
    pub enabled: bool,
}

/// Request body for `POST /automations/{id}/execute`.
/// `trigger_payload` is the pretend trigger data passed to the automation script.
/// Defaults to `{ "type": "manual" }` when omitted.
#[derive(Debug, Deserialize)]
pub struct ManualAutomationRequest {
    pub trigger_payload: Option<AttributeValue>,
}

// ── Command result shapes ─────────────────────────────────────────────────────

/// One entry in the response list from `POST /rooms/{id}/command`.
#[derive(Debug, Serialize)]
pub struct RoomCommandResult {
    pub device_id: String,
    /// `"ok"`, `"unsupported"`, or `"error"`.
    pub status: &'static str,
    pub message: Option<String>,
}

/// One entry in the response list from `POST /groups/{id}/command`.
#[derive(Debug, Serialize)]
pub struct GroupCommandResult {
    pub device_id: String,
    pub status: &'static str,
    pub message: Option<String>,
}

// ── API key shapes ────────────────────────────────────────────────────────────

/// Request body for `POST /auth/keys`.
#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub label: String,
    pub role: ApiKeyRole,
    /// Optional person ID to link to this API key.  When set, requests
    /// authenticated with this key are attributed to that person (e.g. a
    /// companion app submitting location updates).
    pub person_id: Option<String>,
}

/// Response body for `POST /auth/keys`.
/// The raw `token` is only returned once — it is never stored in plaintext.
#[derive(Debug, Serialize)]
pub struct CreateApiKeyResponse {
    pub id: i64,
    pub label: String,
    pub role: ApiKeyRole,
    pub person_id: Option<String>,
    pub token: String,
    pub created_at: DateTime<Utc>,
}

/// One entry in the response list from `GET /auth/keys`.
/// The raw token is never included — only the stored metadata.
#[derive(Debug, Serialize)]
pub struct ApiKeyResponse {
    pub id: i64,
    pub label: String,
    pub role: ApiKeyRole,
    pub person_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

// ── Scene shapes ──────────────────────────────────────────────────────────────

/// Response body for `POST /scenes/{id}/execute`.
#[derive(Debug, Serialize)]
pub struct SceneExecuteResponse {
    pub status: &'static str,
    pub results: Vec<SceneExecutionResult>,
}

// ── Reload shapes ─────────────────────────────────────────────────────────────

/// One file that failed to load during a reload operation.
#[derive(Debug, Serialize)]
pub struct ReloadErrorDetail {
    pub file: String,
    pub message: String,
}

/// Response body for all `POST /*/reload` endpoints.
#[derive(Debug, Serialize)]
pub struct ReloadResponse {
    /// `"ok"` or `"error"`.
    pub status: &'static str,
    /// Which catalog was reloaded: `"scenes"`, `"automations"`, `"scripts"`, or `"plugins"`.
    pub target: &'static str,
    pub loaded_count: usize,
    pub errors: Vec<ReloadErrorDetail>,
    /// How long the reload took, in milliseconds.
    pub duration_ms: u128,
}

/// Response body for `GET /diagnostics/reload_watch`.
#[derive(Debug, Serialize)]
pub struct ReloadWatchResponse {
    pub status: &'static str,
    pub watches: Vec<ReloadWatchItem>,
}

/// One directory that is (or could be) watched for changes.
#[derive(Debug, Serialize)]
pub struct ReloadWatchItem {
    pub target: &'static str,
    pub enabled: bool,
    pub directory: String,
}

// ── Plugin shapes ─────────────────────────────────────────────────────────────

/// Summary of an installed WASM plugin returned by `GET /plugins`.
#[derive(Debug, Serialize)]
pub struct PluginSummary {
    pub name: String,
    pub version: String,
    pub description: String,
    pub api_version: String,
    /// How often the plugin polls its upstream service, in seconds.
    pub poll_interval_secs: u64,
}

impl From<&PluginManifest> for PluginSummary {
    fn from(m: &PluginManifest) -> Self {
        Self {
            name: m.plugin.name.clone(),
            version: m.plugin.version.clone(),
            description: m.plugin.description.clone(),
            api_version: m.plugin.api_version.clone(),
            poll_interval_secs: m.runtime.poll_interval_secs,
        }
    }
}

/// A single Lua file entry returned by `GET /files`.
#[derive(Debug, Serialize)]
pub struct FileEntry {
    /// Relative path in the form `{type}/{filename}`, e.g. `scenes/wakeup.lua`.
    pub path: String,
    /// Top-level category: `scenes`, `automations`, or `scripts`.
    #[serde(rename = "type")]
    pub file_type: &'static str,
}

/// Request body for `PUT /files/{path}`.
#[derive(Debug, Deserialize)]
pub struct PutFileRequest {
    pub content: String,
}

// ── Error type ────────────────────────────────────────────────────────────────

/// An API error that automatically serialises to `{ "error": "<message>" }` JSON
/// with the appropriate HTTP status code.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

// ── History query / response shapes ──────────────────────────────────────────

/// Query-string parameters accepted by all history endpoints.
/// All fields are optional; omitted fields mean "no filter / use the default".
#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
}

/// Response body for `GET /devices/{id}/history`.
#[derive(Debug, Serialize)]
pub struct DeviceHistoryResponse {
    pub device_id: String,
    pub entries: Vec<DeviceHistoryEntry>,
}

/// Response body for `GET /devices/{id}/history/{attribute}`.
#[derive(Debug, Serialize)]
pub struct AttributeHistoryResponse {
    pub device_id: String,
    pub attribute: String,
    pub entries: Vec<AttributeHistoryEntry>,
}

/// Response body for `GET /audit/commands`.
#[derive(Debug, Serialize)]
pub struct CommandAuditResponse {
    pub entries: Vec<CommandAuditEntry>,
}

/// Response body for `GET /scenes/{id}/history`.
#[derive(Debug, Serialize)]
pub struct SceneHistoryResponse {
    pub scene_id: String,
    pub entries: Vec<SceneExecutionHistoryEntry>,
}

/// Response body for `GET /automations/{id}/history`.
#[derive(Debug, Serialize)]
pub struct AutomationHistoryResponse {
    pub automation_id: String,
    pub entries: Vec<AutomationExecutionHistoryEntry>,
}

// ── Automation response shapes ────────────────────────────────────────────────

/// One automation as returned by `GET /automations` and `GET /automations/{id}`.
#[derive(Debug, Serialize)]
pub struct AutomationResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub trigger_type: &'static str,
    pub condition_count: usize,
    /// `"enabled"` or `"disabled"`.
    pub status: &'static str,
    pub last_run: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// Response body for `POST /automations/{id}/validate`.
#[derive(Debug, Serialize)]
pub struct AutomationValidateResponse {
    pub status: &'static str,
    pub automation: AutomationResponse,
}

/// Response body for `POST /automations/{id}/execute`.
#[derive(Debug, Serialize)]
pub struct AutomationExecuteResponse {
    pub status: String,
    pub error: Option<String>,
    /// Wall-clock time the automation took to run, in milliseconds.
    pub duration_ms: i64,
    pub results: Vec<SceneStepResult>,
}

// ── Capability shapes ─────────────────────────────────────────────────────────

/// Response body for `GET /capabilities`.
#[derive(Debug, Serialize)]
pub struct CapabilityCatalogResponse {
    pub capabilities: Vec<CapabilityResponse>,
    pub ownership: CapabilityOwnershipResponse,
}

/// One capability definition (e.g. `light.brightness`, `sensor.temperature`).
#[derive(Debug, Serialize)]
pub struct CapabilityResponse {
    pub domain: &'static str,
    pub key: &'static str,
    pub schema: CapabilitySchemaResponse,
    pub read_only: bool,
    pub actions: Vec<&'static str>,
    pub description: &'static str,
}

/// Rules that describe how capability ownership is determined.
#[derive(Debug, Serialize)]
pub struct CapabilityOwnershipResponse {
    pub canonical_attribute_location: &'static str,
    pub custom_attribute_prefix: &'static str,
    pub vendor_metadata_field: &'static str,
    pub rules: Vec<&'static str>,
}

/// The value type accepted by a capability attribute.
#[derive(Debug, Serialize)]
pub struct CapabilitySchemaResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Non-empty only for enum types — the list of allowed string values.
    pub values: Vec<&'static str>,
}

// ── Diagnostics ───────────────────────────────────────────────────────────────

/// Response body for `GET /diagnostics` — a full system snapshot.
#[derive(Debug, Serialize)]
pub struct DiagnosticsResponse {
    pub status: String,
    pub ready: bool,
    pub devices: usize,
    pub rooms: usize,
    pub groups: usize,
    pub scenes: usize,
    pub automations: usize,
    pub history_enabled: bool,
    pub default_history_limit: usize,
    pub max_history_limit: usize,
    pub runtime: ComponentStatus,
    pub persistence: ComponentStatus,
    pub automations_component: ComponentStatus,
    pub adapters: Vec<AdapterSummary>,
}

// ── Person / zone shapes ──────────────────────────────────────────────────────

/// Request body for `POST /persons`.
#[derive(Debug, Deserialize)]
pub struct CreatePersonRequest {
    /// Stable slug-style identifier, e.g. `"alice"`.
    pub id: String,
    /// Display name shown in the UI.
    pub name: String,
    /// Optional URL or file path to a profile picture.
    pub picture: Option<String>,
}

/// Request body for `PUT /persons/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdatePersonRequest {
    pub name: Option<String>,
    pub picture: Option<String>,
}

/// Request body for `POST /persons/{id}/trackers` — link a tracker device.
#[derive(Debug, Deserialize)]
pub struct LinkTrackerRequest {
    /// The device ID to link (e.g. `"myapp:phone-alice"`).
    pub device_id: String,
}

/// Response body for `GET /persons/{id}/history`.
#[derive(Debug, Serialize)]
pub struct PersonHistoryResponse {
    pub person_id: String,
    pub entries: Vec<PersonHistoryEntry>,
}

/// Request body for `POST /zones`.
#[derive(Debug, Deserialize)]
pub struct CreateZoneRequest {
    /// Stable slug-style identifier, e.g. `"work"`.
    pub id: String,
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Zone radius in metres (default 100).
    pub radius_meters: f64,
    /// Optional icon name (e.g. `"mdi:briefcase"`).
    pub icon: Option<String>,
    /// When `true`, the zone is hidden from the map but usable in automations.
    #[serde(default)]
    pub passive: bool,
}

/// Request body for `PUT /zones/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateZoneRequest {
    pub name: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub radius_meters: Option<f64>,
    pub icon: Option<String>,
    pub passive: Option<bool>,
}

/// Request body for `POST /ingest/location`.
///
/// Called by companion apps to report a GPS fix for the person linked to the
/// authenticating API key.  The key must have a `person_id` set.
#[derive(Debug, Deserialize)]
pub struct IngestLocationRequest {
    pub latitude: f64,
    pub longitude: f64,
    /// Horizontal accuracy in metres (informational — stored but not used for
    /// zone detection).
    pub accuracy_meters: Option<f64>,
    /// Stable device identifier (e.g. a phone UUID).  When omitted the server
    /// generates `location:<person_id>` automatically.
    pub device_id: Option<String>,
}

// ── IPC ingest shapes ─────────────────────────────────────────────────────────

/// Request body for `POST /ingest/devices`.
///
/// IPC adapters call this endpoint to push device state into the registry.
/// Each entry in `devices` must supply a `vendor_id` (without the adapter
/// prefix — the API adds it) plus the standard Device fields.
#[derive(Debug, Deserialize)]
pub struct IngestDevicesRequest {
    /// Must match the adapter name declared in `[adapters]` config.
    pub adapter: String,
    pub devices: Vec<IngestDeviceEntry>,
}

#[derive(Debug, Deserialize)]
pub struct IngestDeviceEntry {
    /// Vendor-assigned device identifier.  Must NOT include the adapter prefix.
    pub vendor_id: String,
    pub kind: String,
    /// JSON object whose keys are attribute names and values are `AttributeValue`
    /// (integer, float, bool, string, or null).
    #[serde(default)]
    pub attributes: serde_json::Value,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

// ── Capability helper functions ───────────────────────────────────────────────
// These convert the internal capability model types into the JSON-friendly
// response types above.

pub fn capability_schema_response(schema: CapabilitySchema) -> CapabilitySchemaResponse {
    match schema {
        CapabilitySchema::Measurement => CapabilitySchemaResponse {
            kind: "measurement",
            values: Vec::new(),
        },
        CapabilitySchema::Accumulation => CapabilitySchemaResponse {
            kind: "accumulation",
            values: Vec::new(),
        },
        CapabilitySchema::Number => CapabilitySchemaResponse {
            kind: "number",
            values: Vec::new(),
        },
        CapabilitySchema::Integer => CapabilitySchemaResponse {
            kind: "integer",
            values: Vec::new(),
        },
        CapabilitySchema::String => CapabilitySchemaResponse {
            kind: "string",
            values: Vec::new(),
        },
        CapabilitySchema::IntegerOrString => CapabilitySchemaResponse {
            kind: "integer_or_string",
            values: Vec::new(),
        },
        CapabilitySchema::Boolean => CapabilitySchemaResponse {
            kind: "boolean",
            values: Vec::new(),
        },
        CapabilitySchema::Percentage => CapabilitySchemaResponse {
            kind: "percentage",
            values: Vec::new(),
        },
        CapabilitySchema::RgbColor => CapabilitySchemaResponse {
            kind: "rgb_color",
            values: Vec::new(),
        },
        CapabilitySchema::HexColor => CapabilitySchemaResponse {
            kind: "hex_color",
            values: Vec::new(),
        },
        CapabilitySchema::XyColor => CapabilitySchemaResponse {
            kind: "xy_color",
            values: Vec::new(),
        },
        CapabilitySchema::HsColor => CapabilitySchemaResponse {
            kind: "hs_color",
            values: Vec::new(),
        },
        CapabilitySchema::ColorTemperature => CapabilitySchemaResponse {
            kind: "color_temperature",
            values: Vec::new(),
        },
        CapabilitySchema::Enum(values) => CapabilitySchemaResponse {
            kind: "enum",
            values: values.to_vec(),
        },
    }
}

pub fn capability_ownership_response(
    policy: CapabilityOwnershipPolicy,
) -> CapabilityOwnershipResponse {
    CapabilityOwnershipResponse {
        canonical_attribute_location: policy.canonical_attribute_location,
        custom_attribute_prefix: policy.custom_attribute_prefix,
        vendor_metadata_field: policy.vendor_metadata_field,
        rules: policy.rules.to_vec(),
    }
}
