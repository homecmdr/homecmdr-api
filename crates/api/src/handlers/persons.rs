//! Handlers for person and zone management, plus the location-ingest endpoint.
//!
//! - `GET  /persons`                       — list all persons
//! - `POST /persons`                       — create a person
//! - `GET  /persons/{id}`                  — get a person
//! - `PUT  /persons/{id}`                  — update a person's display fields
//! - `DELETE /persons/{id}`                — remove a person
//! - `POST /persons/{id}/picture`          — upload a profile picture (multipart)
//! - `GET  /persons/{id}/picture`          — serve the uploaded profile picture
//! - `DELETE /persons/{id}/picture`        — delete the profile picture
//! - `POST /persons/{id}/trackers`         — link a tracker device
//! - `DELETE /persons/{id}/trackers/{did}` — unlink a tracker device
//! - `GET  /persons/{id}/history`          — location state history
//! - `GET  /zones`                         — list all zones
//! - `POST /zones`                         — create a zone
//! - `GET  /zones/{id}`                    — get a zone
//! - `PUT  /zones/{id}`                    — update a zone
//! - `DELETE /zones/{id}`                  — remove a zone
//! - `POST /ingest/location`               — companion-app GPS ingest

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::body::Body;
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

/// Returns all persons currently in the registry.
/// Returns 503 when persistence is disabled (the registry is not available).
pub async fn list_persons(State(state): State<AppState>) -> Result<Json<Vec<Person>>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;
    Ok(Json(registry.list_persons().await))
}

/// Returns a single person by ID, or 404 if they do not exist.
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

/// Returns paginated location-state history for a person.
/// Supports optional `start`, `end`, and `limit` query parameters.
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

/// Creates a new person in the registry.
/// The `id` field must be a stable slug-style identifier (e.g. `"alice"`).
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

/// Updates a person's display name and/or profile picture.
/// Fields that are omitted in the request body are left unchanged.
/// Returns 404 if the person does not exist.
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

/// Permanently removes a person and all their tracker links.
/// Returns 204 No Content on success, or 404 if not found.
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

// ── Person picture endpoints ──────────────────────────────────────────────────

/// Returns the image file extension corresponding to a MIME content type.
fn ext_for_content_type(content_type: &str) -> &'static str {
    // Strip any parameters (e.g. "image/jpeg; charset=…") before matching.
    let base = content_type.split(';').next().unwrap_or("").trim();
    match base {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg", // covers image/jpeg, image/jpg, and any unknown image type
    }
}

/// Returns the MIME content type for a file extension.
fn content_type_for_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
}

/// The set of extensions we look for when reading or deleting a stored picture.
const PICTURE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

/// Uploads or replaces the profile picture for a person.
///
/// Accepts `multipart/form-data` with a single image field (any name).
/// The file is saved under `pictures_directory/<id>.<ext>` where the
/// extension is derived from the field's `Content-Type`.  Only `image/*`
/// content types are accepted.
///
/// The person's `picture` field is updated to `/persons/{id}/picture` so
/// that callers can retrieve it via `GET /persons/{id}/picture`.
///
/// Returns the updated person, or 404 if the person does not exist.
pub async fn upload_person_picture(
    State(state): State<AppState>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<Person>, ApiError> {
    // Basic safety: reject IDs that could escape the pictures directory.
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid person id"));
    }

    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let mut person = registry
        .get_person(&PersonId(id.clone()))
        .await
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))?;

    // Read the first field from the multipart body.
    let (image_bytes, content_type) = loop {
        let field = multipart
            .next_field()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "no image field in multipart upload"))?;

        let ct = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();

        if !ct.starts_with("image/") {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unsupported content type '{ct}'; only image/* is accepted"),
            ));
        }

        let data = field
            .bytes()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;

        break (data, ct);
    };

    let ext = ext_for_content_type(&content_type);
    let dir = std::path::Path::new(&state.pictures_directory);

    tokio::fs::create_dir_all(dir).await.map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create pictures directory: {e}"),
        )
    })?;

    // Remove any previously stored picture for this person regardless of extension.
    for old_ext in PICTURE_EXTENSIONS {
        let _ = tokio::fs::remove_file(dir.join(format!("{id}.{old_ext}"))).await;
    }

    tokio::fs::write(dir.join(format!("{id}.{ext}")), &image_bytes)
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to save picture: {e}"),
            )
        })?;

    person.picture = Some(format!("/persons/{id}/picture"));
    registry
        .update_person(person.clone())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(person))
}

/// Serves the stored profile picture for a person.
///
/// Returns the raw image bytes with the appropriate `Content-Type` header,
/// or 404 if the person does not exist or has no uploaded picture.
pub async fn get_person_picture(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response<Body>, ApiError> {
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid person id"));
    }

    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    registry
        .get_person(&PersonId(id.clone()))
        .await
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))?;

    let dir = std::path::Path::new(&state.pictures_directory);

    // Find whichever extension was stored.
    let mut found: Option<(std::path::PathBuf, &'static str)> = None;
    for ext in PICTURE_EXTENSIONS {
        let path = dir.join(format!("{id}.{ext}"));
        if path.exists() {
            found = Some((path, content_type_for_ext(ext)));
            break;
        }
    }

    let (path, content_type) = found
        .ok_or_else(|| ApiError::not_found(format!("no picture found for person '{id}'")))?;

    let bytes = tokio::fs::read(&path).await.map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read picture: {e}"),
        )
    })?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(bytes))
        .expect("response builds"))
}

/// Deletes the stored profile picture for a person and clears the `picture` field.
///
/// Returns 204 No Content on success, or 404 if the person does not exist or
/// has no uploaded picture.
pub async fn delete_person_picture(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid person id"));
    }

    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;

    let mut person = registry
        .get_person(&PersonId(id.clone()))
        .await
        .ok_or_else(|| ApiError::not_found(format!("person '{id}' not found")))?;

    let dir = std::path::Path::new(&state.pictures_directory);
    let mut deleted = false;

    for ext in PICTURE_EXTENSIONS {
        let path = dir.join(format!("{id}.{ext}"));
        if path.exists() {
            tokio::fs::remove_file(&path).await.map_err(|e| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to delete picture: {e}"),
                )
            })?;
            deleted = true;
        }
    }

    if !deleted {
        return Err(ApiError::not_found(format!(
            "no picture found for person '{id}'"
        )));
    }

    person.picture = None;
    registry
        .update_person(person)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Links a tracker device to a person.
///
/// The tracker device ID is added to the person's `trackers` list.  Duplicate
/// links are silently ignored.  Returns the updated person.
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

/// Removes a tracker device link from a person.
/// Returns the updated person.  Returns 404 if the person does not exist.
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

/// Returns all zones in the registry (including the auto-managed `home` zone).
pub async fn list_zones(State(state): State<AppState>) -> Result<Json<Vec<Zone>>, ApiError> {
    let registry = state
        .person_registry
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "person registry not available"))?;
    Ok(Json(registry.list_zones().await))
}

/// Returns a single zone by ID, or 404 if it does not exist.
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

/// Creates a new zone.
/// The `home` zone ID is reserved and cannot be created via this endpoint.
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

/// Updates a zone's fields.  Only fields included in the request body are changed.
/// Returns the updated zone, or 404 if the zone does not exist.
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

/// Permanently removes a zone.
/// Returns 204 No Content on success, or 404 if not found.
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
