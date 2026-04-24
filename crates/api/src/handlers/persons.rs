//! Handlers for person and zone management, plus the location-ingest endpoint.
//!
//! - `GET  /persons`                       — list all persons
//! - `POST /persons`                       — create a person
//! - `GET  /persons/{id}`                  — get a person
//! - `PUT  /persons/{id}`                  — update a person's display fields
//! - `DELETE /persons/{id}`                — remove a person
//! - `POST /persons/{id}/trackers`         — link a tracker device
//! - `DELETE /persons/{id}/trackers/{did}` — unlink a tracker device
//! - `GET  /persons/{id}/history`          — location state history
//! - `GET  /zones`                         — list all zones
//! - `POST /zones`                         — create a zone
//! - `GET  /zones/{id}`                    — get a zone
//! - `PUT  /zones/{id}`                    — update a zone
//! - `DELETE /zones/{id}`                  — remove a zone
//! - `POST /ingest/location`               — companion-app GPS ingest

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use homecmdr_core::capability::TRACKER_TYPE;
use homecmdr_core::model::{AttributeValue, DeviceId, Person, PersonId, Zone, ZoneId};
use homecmdr_core::person_registry::{PersonRegistry, HOME_ZONE_ID};

use crate::dto::{
    ApiError, CreatePersonRequest, CreateZoneRequest, HistoryQuery, IngestLocationRequest,
    LinkTrackerRequest, PersonHistoryResponse, UpdatePersonRequest, UpdateZoneRequest,
};
use crate::helpers::sha256_hex;
use crate::state::AppState;

// ── Person read endpoints ─────────────────────────────────────────────────────

pub async fn list_persons(State(state): State<AppState>) -> Result<Json<Vec<Person>>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;
    Ok(Json(registry.list_persons().await))
}

pub async fn get_person(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Person>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;
    registry
        .get_person(&PersonId(id.clone()))
        .await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))
}

pub async fn get_person_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<PersonHistoryResponse>, ApiError> {
    let person_store = state
        .person_store
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person store not available"))?;

    let limit = query.limit.unwrap_or(state.history.default_limit);
    let limit = limit.min(state.history.max_limit);

    let entries = person_store
        .load_person_history(
            &PersonId(id.clone()),
            query.start,
            query.end,
            limit,
        )
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(PersonHistoryResponse {
        person_id: id,
        entries,
    }))
}

// ── Person write endpoints ────────────────────────────────────────────────────

pub async fn create_person(
    State(state): State<AppState>,
    Json(req): Json<CreatePersonRequest>,
) -> Result<Json<Person>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let person = Person {
        id: PersonId(req.id),
        name: req.name,
        picture: req.picture,
        trackers: Vec::new(),
        state: homecmdr_core::model::PersonState::Unknown,
        state_source: None,
        latitude: None,
        longitude: None,
        updated_at: chrono::Utc::now(),
    };

    registry
        .add_person(person.clone())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(person))
}

pub async fn update_person(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdatePersonRequest>,
) -> Result<Json<Person>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let mut person = registry
        .get_person(&PersonId(id.clone()))
        .await
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))?;

    if let Some(name) = req.name {
        person.name = name;
    }
    // `picture` is an `Option<Option<String>>` in the request so we can distinguish
    // "omitted" from "explicitly set to null".  Here we use a simple Option<String>
    // and only update when present (cannot clear with this approach; acceptable for now).
    if let Some(picture) = req.picture {
        person.picture = Some(picture);
    }

    registry
        .update_person(person.clone())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(person))
}

pub async fn delete_person(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let removed = registry
        .remove_person(&PersonId(id.clone()))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("person '{id}' not found")))
    }
}

pub async fn link_tracker(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<LinkTrackerRequest>,
) -> Result<Json<Person>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let person = registry
        .get_person(&PersonId(id.clone()))
        .await
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))?;

    let device_id = DeviceId(req.device_id);
    if !person.trackers.contains(&device_id) {
        let mut trackers = person.trackers.clone();
        trackers.push(device_id);
        registry
            .set_person_trackers(&PersonId(id.clone()), trackers)
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    registry
        .get_person(&PersonId(id.clone()))
        .await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))
}

pub async fn unlink_tracker(
    State(state): State<AppState>,
    Path((id, device_id)): Path<(String, String)>,
) -> Result<Json<Person>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let person = registry
        .get_person(&PersonId(id.clone()))
        .await
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))?;

    let did = DeviceId(device_id.clone());
    let trackers: Vec<DeviceId> = person.trackers.into_iter().filter(|d| d != &did).collect();

    registry
        .set_person_trackers(&PersonId(id.clone()), trackers)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    registry
        .get_person(&PersonId(id.clone()))
        .await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))
}

// ── Zone read endpoints ───────────────────────────────────────────────────────

pub async fn list_zones(State(state): State<AppState>) -> Result<Json<Vec<Zone>>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;
    Ok(Json(registry.list_zones().await))
}

pub async fn get_zone(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Zone>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;
    registry
        .get_zone(&ZoneId(id.clone()))
        .await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("zone '{id}' not found")))
}

// ── Zone write endpoints ──────────────────────────────────────────────────────

pub async fn create_zone(
    State(state): State<AppState>,
    Json(req): Json<CreateZoneRequest>,
) -> Result<Json<Zone>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    if req.id == HOME_ZONE_ID {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the 'home' zone is managed automatically and cannot be created via the API",
        ));
    }

    let zone = Zone {
        id: ZoneId(req.id),
        name: req.name,
        latitude: req.latitude,
        longitude: req.longitude,
        radius_meters: req.radius_meters,
        icon: req.icon,
        passive: req.passive,
    };

    registry
        .add_zone(zone.clone())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(zone))
}

pub async fn update_zone(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateZoneRequest>,
) -> Result<Json<Zone>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let mut zone = registry
        .get_zone(&ZoneId(id.clone()))
        .await
        .ok_or_else(|| ApiError::not_found(format!("zone '{id}' not found")))?;

    if let Some(name) = req.name {
        zone.name = name;
    }
    if let Some(lat) = req.latitude {
        zone.latitude = lat;
    }
    if let Some(lon) = req.longitude {
        zone.longitude = lon;
    }
    if let Some(r) = req.radius_meters {
        zone.radius_meters = r;
    }
    if let Some(icon) = req.icon {
        zone.icon = Some(icon);
    }
    if let Some(passive) = req.passive {
        zone.passive = passive;
    }

    registry
        .update_zone(zone.clone())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(zone))
}

pub async fn delete_zone(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let removed = registry
        .remove_zone(&ZoneId(id.clone()))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("zone '{id}' not found")))
    }
}

// ── Location ingest ───────────────────────────────────────────────────────────

/// Accept a GPS location fix from a companion app.
///
/// The request must be authenticated with a bearer token whose API key has a
/// `person_id` set.  The handler looks up the full key record to identify
/// which person this location belongs to.  Returns 403 if the key has no
/// linked person.
pub async fn ingest_location(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IngestLocationRequest>,
) -> Result<StatusCode, ApiError> {
    // Re-extract the bearer token to look up the person_id from the key record.
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "missing bearer token"))?;

    let key_store = state
        .auth_key_store
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "key store not available"))?;

    let record = key_store
        .lookup_api_key_by_hash(&sha256_hex(token))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "invalid token"))?;

    let person_id = record.person_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "api key is not linked to a person; set person_id when creating the key",
        )
    })?;

    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    // Ensure the tracker device exists in the main device registry BEFORE
    // calling ingest_location.  person_registry::ingest_location fires a
    // DeviceStateChanged event on the bus; the persistence worker handles that
    // event by looking up the device via `runtime.registry().get(&id)`.
    // If the upsert happens after the event is published, the worker may
    // process the event before the device exists in the registry and silently
    // drop the save.  Upserting first eliminates this race.
    //
    // Only tracker.type is set here — tracker.latitude and tracker.longitude
    // are CapabilitySchema::Measurement (require a {value,unit} object) so
    // setting them as raw floats fails validate_device() with a 500.
    // Coordinate persistence is handled by person_registry::ingest_location()
    // which writes directly to the person record and bypasses device validation.
    let device_id = DeviceId(
        req.device_id
            .as_deref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("location:{}", person_id.0)),
    );
    let mut tracker = PersonRegistry::build_tracker_device(&device_id);
    tracker.attributes.insert(
        TRACKER_TYPE.to_string(),
        AttributeValue::Text("gps".to_string()),
    );
    state
        .runtime
        .registry()
        .upsert(tracker)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    registry
        .ingest_location(
            &person_id,
            req.device_id.as_deref(),
            req.latitude,
            req.longitude,
            req.accuracy_meters,
        )
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}
