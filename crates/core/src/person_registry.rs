//! # PersonRegistry
//!
//! Central registry for [`Person`] entities and [`Zone`] geographic fences.
//!
//! ## Responsibilities
//!
//! - Maintains the in-memory state of all persons and zones.
//! - Subscribes to [`Event::DeviceStateChanged`] events on the event bus and
//!   re-derives the state of any person whose tracker device changed.
//! - Derives person state using HA-style tracker priority:
//!   1. Any linked **stationary/BLE** tracker with `tracker.state = "home"` → `Home`
//!   2. Else most-recently-updated **GPS** tracker → zone match or `Away`
//!   3. Else any stationary/BLE tracker with `tracker.state = "not_home"` → `Away`
//!   4. Else `Unknown`
//! - Performs geographic zone detection via the haversine formula.
//! - Applies `consider_home` debounce for stationary trackers.
//! - Emits [`Event::PersonStateChanged`] (and related) events when state transitions.
//! - Auto-creates a tracker device entry on the first `/ingest/location` call for
//!   an unknown device ID.
//! - Auto-creates the `home` zone at startup from `[locale]` config when it is
//!   absent from the store.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::bus::EventBus;
use crate::capability::{TRACKER_CONSIDER_HOME, TRACKER_LATITUDE, TRACKER_LONGITUDE, TRACKER_STATE, TRACKER_TYPE};
use crate::config::LocaleConfig;
use crate::event::Event;
use crate::model::{
    AttributeValue, Device, DeviceId, DeviceKind, Metadata, Person, PersonId, PersonState, RoomId,
    Zone, ZoneId,
};
use crate::store::{PersonHistoryEntry, PersonStore};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The well-known ID for the auto-created home zone.
pub const HOME_ZONE_ID: &str = "home";

/// Default `consider_home` debounce in seconds when the tracker attribute is absent.
const DEFAULT_CONSIDER_HOME_SECS: i64 = 180;

// ---------------------------------------------------------------------------
// Internal tracker snapshot
// ---------------------------------------------------------------------------

/// Cached snapshot of a tracker device's relevant attributes.
#[derive(Debug, Clone)]
struct TrackerSnapshot {
    device_id: DeviceId,
    tracker_type: TrackerType,
    state: String,
    latitude: Option<f64>,
    longitude: Option<f64>,
    consider_home_secs: i64,
    last_updated: DateTime<Utc>,
    /// Timestamp when the tracker last reported "home" (used for debounce).
    last_home_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrackerType {
    Gps,
    Stationary,
    Ble,
}

impl TrackerType {
    fn from_str(s: &str) -> Self {
        match s {
            "gps" => Self::Gps,
            "ble" => Self::Ble,
            _ => Self::Stationary, // default to stationary for unknown values
        }
    }

    fn is_stationary(&self) -> bool {
        matches!(self, Self::Stationary | Self::Ble)
    }
}

fn extract_f64(attributes: &crate::model::Attributes, key: &str) -> Option<f64> {
    match attributes.get(key)? {
        AttributeValue::Float(v) => Some(*v),
        AttributeValue::Integer(v) => Some(*v as f64),
        _ => None,
    }
}

fn extract_str<'a>(attributes: &'a crate::model::Attributes, key: &str) -> Option<&'a str> {
    match attributes.get(key)? {
        AttributeValue::Text(s) => Some(s.as_str()),
        _ => None,
    }
}

fn extract_i64(attributes: &crate::model::Attributes, key: &str) -> Option<i64> {
    match attributes.get(key)? {
        AttributeValue::Integer(v) => Some(*v),
        AttributeValue::Float(v) => Some(*v as i64),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Haversine distance
// ---------------------------------------------------------------------------

/// Returns the great-circle distance between two lat/lon coordinates in metres.
pub fn haversine_distance_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_000.0;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    EARTH_RADIUS_M * c
}

// ---------------------------------------------------------------------------
// PersonRegistry
// ---------------------------------------------------------------------------

/// Shared state inside the registry, behind an `RwLock`.
struct Inner {
    persons: HashMap<PersonId, Person>,
    zones: HashMap<ZoneId, Zone>,
    /// Per-tracker snapshots, keyed by device ID.
    trackers: HashMap<DeviceId, TrackerSnapshot>,
    /// Reverse index: device_id → set of person IDs that have this tracker linked.
    device_to_persons: HashMap<DeviceId, Vec<PersonId>>,
}

impl Inner {
    fn new() -> Self {
        Self {
            persons: HashMap::new(),
            zones: HashMap::new(),
            trackers: HashMap::new(),
            device_to_persons: HashMap::new(),
        }
    }

    /// Rebuild the reverse device→person index from scratch.
    fn rebuild_device_index(&mut self) {
        self.device_to_persons.clear();
        for person in self.persons.values() {
            for device_id in &person.trackers {
                self.device_to_persons
                    .entry(device_id.clone())
                    .or_default()
                    .push(person.id.clone());
            }
        }
    }

    /// Detect which zone (if any) a GPS coordinate falls inside.
    /// Returns the `home` zone first if matched, then other zones in insertion order.
    fn detect_zone(&self, lat: f64, lon: f64) -> Option<&Zone> {
        // Check home zone first (priority)
        if let Some(home) = self.zones.get(&ZoneId(HOME_ZONE_ID.to_string())) {
            if !home.passive {
                let dist = haversine_distance_meters(lat, lon, home.latitude, home.longitude);
                if dist <= home.radius_meters {
                    return Some(home);
                }
            }
        }
        // Check other zones
        for zone in self.zones.values() {
            if zone.id.0 == HOME_ZONE_ID || zone.passive {
                continue;
            }
            let dist = haversine_distance_meters(lat, lon, zone.latitude, zone.longitude);
            if dist <= zone.radius_meters {
                return Some(zone);
            }
        }
        None
    }

    /// Derive the [`PersonState`] for a person given their current tracker snapshots.
    ///
    /// Priority (HA-style):
    /// 1. Any stationary/BLE tracker currently reporting `"home"` (not debounce-expired) → `Home`
    /// 2. Most-recently-updated GPS tracker → zone match or `Away`
    /// 3. Any stationary/BLE tracker reporting `"not_home"` → `Away`
    /// 4. `Unknown`
    fn derive_person_state(
        &self,
        person: &Person,
        now: DateTime<Utc>,
    ) -> (PersonState, Option<DeviceId>, Option<f64>, Option<f64>) {
        let mut stationary_home: Option<&TrackerSnapshot> = None;
        let mut stationary_not_home: Option<&TrackerSnapshot> = None;
        let mut best_gps: Option<&TrackerSnapshot> = None;

        for device_id in &person.trackers {
            let Some(snap) = self.trackers.get(device_id) else {
                continue;
            };

            if snap.tracker_type.is_stationary() {
                if snap.state == "home" {
                    // Check consider_home debounce — if last_home_at is set and the debounce
                    // window has NOT yet expired, we still consider this as "home".
                    let debounce_expired = snap.last_home_at.map_or(false, |last| {
                        (now - last).num_seconds() > snap.consider_home_secs
                    });
                    if !debounce_expired {
                        stationary_home = Some(snap);
                    }
                } else if snap.state == "not_home" {
                    stationary_not_home = Some(snap);
                }
            } else {
                // GPS tracker — keep the most recently updated one
                match best_gps {
                    None => best_gps = Some(snap),
                    Some(current) if snap.last_updated > current.last_updated => {
                        best_gps = Some(snap)
                    }
                    _ => {}
                }
            }
        }

        // Priority 1: stationary home
        if let Some(snap) = stationary_home {
            return (PersonState::Home, Some(snap.device_id.clone()), None, None);
        }

        // Priority 2: GPS
        if let Some(snap) = best_gps {
            if let (Some(lat), Some(lon)) = (snap.latitude, snap.longitude) {
                if let Some(zone) = self.detect_zone(lat, lon) {
                    let state = if zone.id.0 == HOME_ZONE_ID {
                        PersonState::Home
                    } else {
                        PersonState::Zone {
                            zone_id: zone.id.clone(),
                        }
                    };
                    return (state, Some(snap.device_id.clone()), Some(lat), Some(lon));
                }
                return (
                    PersonState::Away,
                    Some(snap.device_id.clone()),
                    Some(lat),
                    Some(lon),
                );
            }
        }

        // Priority 3: stationary not_home
        if let Some(snap) = stationary_not_home {
            return (PersonState::Away, Some(snap.device_id.clone()), None, None);
        }

        (PersonState::Unknown, None, None, None)
    }
}

/// The person registry.
///
/// Cheaply cloneable — all state is behind an `Arc`.
#[derive(Clone)]
pub struct PersonRegistry {
    inner: Arc<RwLock<Inner>>,
    store: Arc<dyn PersonStore>,
    bus: EventBus,
    /// Mutex used to serialise concurrent state-derivation writes for the same person.
    derive_lock: Arc<Mutex<()>>,
}

impl PersonRegistry {
    /// Create a new registry, loading persons and zones from the store.
    ///
    /// Also ensures the `home` zone exists (creates it from locale config if absent).
    pub async fn new(
        store: Arc<dyn PersonStore>,
        bus: EventBus,
        locale: &LocaleConfig,
    ) -> anyhow::Result<Self> {
        let mut inner = Inner::new();

        // Load zones
        let zones = store.load_all_zones().await?;
        for zone in zones {
            inner.zones.insert(zone.id.clone(), zone);
        }

        // Ensure home zone exists
        let home_id = ZoneId(HOME_ZONE_ID.to_string());
        if !inner.zones.contains_key(&home_id) {
            if let (Some(lat), Some(lon)) = (locale.latitude, locale.longitude) {
                let home_zone = Zone {
                    id: home_id.clone(),
                    name: "Home".to_string(),
                    latitude: lat,
                    longitude: lon,
                    radius_meters: locale.home_zone_radius_meters,
                    icon: Some("mdi:home".to_string()),
                    passive: false,
                };
                store.upsert_zone(&home_zone).await?;
                info!(
                    "Auto-created home zone at ({}, {}) radius {}m",
                    lat, lon, locale.home_zone_radius_meters
                );
                inner.zones.insert(home_id, home_zone);
            } else {
                warn!("No locale.latitude/longitude configured — home zone not created");
            }
        }

        // Load persons
        let persons = store.load_all_persons().await?;
        for person in persons {
            inner.persons.insert(person.id.clone(), person);
        }
        inner.rebuild_device_index();

        let registry = Self {
            inner: Arc::new(RwLock::new(inner)),
            store,
            bus,
            derive_lock: Arc::new(Mutex::new(())),
        };

        info!(
            "PersonRegistry initialised ({} persons, {} zones)",
            registry.inner.read().await.persons.len(),
            registry.inner.read().await.zones.len(),
        );

        Ok(registry)
    }

    // -----------------------------------------------------------------------
    // Person CRUD
    // -----------------------------------------------------------------------

    pub async fn add_person(&self, person: Person) -> anyhow::Result<()> {
        self.store.upsert_person(&person).await?;
        let mut inner = self.inner.write().await;
        inner.persons.insert(person.id.clone(), person.clone());
        inner.rebuild_device_index();
        drop(inner);
        self.bus.publish(Event::PersonAdded { person });
        Ok(())
    }

    pub async fn update_person(&self, person: Person) -> anyhow::Result<()> {
        self.store.upsert_person(&person).await?;
        let mut inner = self.inner.write().await;
        inner.persons.insert(person.id.clone(), person.clone());
        inner.rebuild_device_index();
        drop(inner);
        self.bus.publish(Event::PersonUpdated { person });
        Ok(())
    }

    pub async fn remove_person(&self, person_id: &PersonId) -> anyhow::Result<bool> {
        let removed = self.store.delete_person(person_id).await?;
        if removed {
            let mut inner = self.inner.write().await;
            inner.persons.remove(person_id);
            inner.rebuild_device_index();
            drop(inner);
            self.bus
                .publish(Event::PersonRemoved { person_id: person_id.clone() });
        }
        Ok(removed)
    }

    pub async fn get_person(&self, person_id: &PersonId) -> Option<Person> {
        self.inner.read().await.persons.get(person_id).cloned()
    }

    pub async fn list_persons(&self) -> Vec<Person> {
        self.inner.read().await.persons.values().cloned().collect()
    }

    /// Update the ordered list of tracker device IDs linked to a person, then
    /// immediately re-derive the person's state.
    pub async fn set_person_trackers(
        &self,
        person_id: &PersonId,
        trackers: Vec<DeviceId>,
    ) -> anyhow::Result<Option<Person>> {
        self.store
            .update_person_trackers(person_id, &trackers)
            .await?;

        let mut inner = self.inner.write().await;
        let Some(person) = inner.persons.get_mut(person_id) else {
            return Ok(None);
        };
        person.trackers = trackers;
        inner.rebuild_device_index();
        drop(inner);

        // Re-derive state with the new tracker list
        self.re_derive_person(person_id).await?;

        Ok(self.get_person(person_id).await)
    }

    // -----------------------------------------------------------------------
    // Zone CRUD
    // -----------------------------------------------------------------------

    pub async fn add_zone(&self, zone: Zone) -> anyhow::Result<()> {
        self.store.upsert_zone(&zone).await?;
        let mut inner = self.inner.write().await;
        inner.zones.insert(zone.id.clone(), zone.clone());
        drop(inner);
        self.bus.publish(Event::ZoneAdded { zone });
        Ok(())
    }

    pub async fn update_zone(&self, zone: Zone) -> anyhow::Result<()> {
        self.store.upsert_zone(&zone).await?;
        let mut inner = self.inner.write().await;
        inner.zones.insert(zone.id.clone(), zone.clone());
        drop(inner);
        self.bus.publish(Event::ZoneUpdated { zone });
        // Re-derive all persons — zone change may affect GPS-based state
        self.re_derive_all_persons().await?;
        Ok(())
    }

    pub async fn remove_zone(&self, zone_id: &ZoneId) -> anyhow::Result<bool> {
        if zone_id.0 == HOME_ZONE_ID {
            anyhow::bail!("the home zone cannot be deleted");
        }
        let removed = self.store.delete_zone(zone_id).await?;
        if removed {
            let mut inner = self.inner.write().await;
            inner.zones.remove(zone_id);
            drop(inner);
            self.bus
                .publish(Event::ZoneRemoved { zone_id: zone_id.clone() });
            self.re_derive_all_persons().await?;
        }
        Ok(removed)
    }

    pub async fn get_zone(&self, zone_id: &ZoneId) -> Option<Zone> {
        self.inner.read().await.zones.get(zone_id).cloned()
    }

    pub async fn list_zones(&self) -> Vec<Zone> {
        self.inner.read().await.zones.values().cloned().collect()
    }

    /// Return the number of persons currently in each zone.
    pub async fn zone_person_counts(&self) -> HashMap<ZoneId, usize> {
        let inner = self.inner.read().await;
        let mut counts: HashMap<ZoneId, usize> = HashMap::new();
        for zone in inner.zones.values() {
            counts.insert(zone.id.clone(), 0);
        }
        for person in inner.persons.values() {
            match &person.state {
                PersonState::Home => {
                    let home_id = ZoneId(HOME_ZONE_ID.to_string());
                    *counts.entry(home_id).or_insert(0) += 1;
                }
                PersonState::Zone { zone_id } => {
                    *counts.entry(zone_id.clone()).or_insert(0) += 1;
                }
                _ => {}
            }
        }
        counts
    }

    // -----------------------------------------------------------------------
    // Convenience queries for automations
    // -----------------------------------------------------------------------

    pub async fn all_persons_away(&self) -> bool {
        let inner = self.inner.read().await;
        if inner.persons.is_empty() {
            return false;
        }
        inner
            .persons
            .values()
            .all(|p| matches!(p.state, PersonState::Away | PersonState::Unknown))
    }

    pub async fn any_person_home(&self) -> bool {
        let inner = self.inner.read().await;
        inner
            .persons
            .values()
            .any(|p| matches!(p.state, PersonState::Home))
    }

    pub async fn persons_in_zone(&self, zone_id: &ZoneId) -> Vec<Person> {
        let inner = self.inner.read().await;
        inner
            .persons
            .values()
            .filter(|p| {
                if zone_id.0 == HOME_ZONE_ID {
                    matches!(p.state, PersonState::Home)
                } else {
                    matches!(&p.state, PersonState::Zone { zone_id: zid } if zid == zone_id)
                }
            })
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------------
    // Device event handler
    // -----------------------------------------------------------------------

    /// Called when a `DeviceStateChanged` event is received.
    ///
    /// Updates the internal tracker snapshot if the device is a known tracker
    /// for any person, then re-derives those persons' states.
    pub async fn handle_device_state_changed(
        &self,
        device_id: &DeviceId,
        attributes: &crate::model::Attributes,
        updated_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        // Only process if this device is a tracker for at least one person
        let affected_persons: Vec<PersonId> = {
            let inner = self.inner.read().await;
            inner
                .device_to_persons
                .get(device_id)
                .cloned()
                .unwrap_or_default()
        };

        if affected_persons.is_empty() {
            return Ok(());
        }

        // Update tracker snapshot
        {
            let mut inner = self.inner.write().await;
            let existing = inner.trackers.get(device_id).cloned();
            let tracker_type = extract_str(attributes, TRACKER_TYPE)
                .map(TrackerType::from_str)
                .unwrap_or_else(|| {
                    existing
                        .as_ref()
                        .map(|s| s.tracker_type.clone())
                        .unwrap_or(TrackerType::Stationary)
                });
            let state = extract_str(attributes, TRACKER_STATE)
                .unwrap_or_else(|| existing.as_ref().map(|s| s.state.as_str()).unwrap_or(""))
                .to_string();
            let latitude = extract_f64(attributes, TRACKER_LATITUDE)
                .or_else(|| existing.as_ref().and_then(|s| s.latitude));
            let longitude = extract_f64(attributes, TRACKER_LONGITUDE)
                .or_else(|| existing.as_ref().and_then(|s| s.longitude));
            let consider_home_secs = extract_i64(attributes, TRACKER_CONSIDER_HOME)
                .unwrap_or_else(|| {
                    existing
                        .as_ref()
                        .map(|s| s.consider_home_secs)
                        .unwrap_or(DEFAULT_CONSIDER_HOME_SECS)
                });

            // Track last_home_at for consider_home debounce
            let last_home_at = if state == "home" {
                Some(updated_at)
            } else {
                existing.as_ref().and_then(|s| s.last_home_at)
            };

            inner.trackers.insert(
                device_id.clone(),
                TrackerSnapshot {
                    device_id: device_id.clone(),
                    tracker_type,
                    state,
                    latitude,
                    longitude,
                    consider_home_secs,
                    last_updated: updated_at,
                    last_home_at,
                },
            );
        }

        // Re-derive state for each affected person
        for person_id in &affected_persons {
            if let Err(e) = self.re_derive_person(person_id).await {
                error!(
                    person_id = %person_id.0,
                    error = %e,
                    "Failed to re-derive person state"
                );
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Auto-create tracker device
    // -----------------------------------------------------------------------

    /// Create a minimal tracker device in the device registry on first location report.
    ///
    /// Returns a `Device` suitable for upserting via the `DeviceRegistry`.
    pub fn build_tracker_device(device_id: &DeviceId) -> Device {
        use crate::model::Attributes;
        let now = Utc::now();
        Device {
            id: device_id.clone(),
            room_id: None,
            kind: DeviceKind::Sensor,
            attributes: Attributes::new(),
            metadata: Metadata {
                source: "location_ingest".to_string(),
                accuracy: None,
                vendor_specific: HashMap::new(),
            },
            updated_at: now,
            last_seen: now,
        }
    }

    /// Ingest a GPS location update for a person from a companion app.
    ///
    /// Called by `POST /ingest/location`.  If the specified tracker device is
    /// not yet linked to the person, it is auto-linked as a GPS tracker.
    /// Emits `DeviceStateChanged` on the event bus so the device store
    /// persists the fix, then re-derives the person's state immediately.
    pub async fn ingest_location(
        &self,
        person_id: &PersonId,
        device_id_hint: Option<&str>,
        latitude: f64,
        longitude: f64,
        _accuracy_meters: Option<f64>,
    ) -> anyhow::Result<()> {
        let device_id = DeviceId(
            device_id_hint
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("location:{}", person_id.0)),
        );

        // Ensure the person exists.
        {
            let inner = self.inner.read().await;
            anyhow::ensure!(
                inner.persons.contains_key(person_id),
                "person '{}' not found",
                person_id.0
            );
        }

        // Auto-link the device as a GPS tracker if not already linked.
        let is_already_linked = {
            let inner = self.inner.read().await;
            inner
                .persons
                .get(person_id)
                .map(|p| p.trackers.contains(&device_id))
                .unwrap_or(false)
        };

        if !is_already_linked {
            let new_trackers = {
                let inner = self.inner.read().await;
                let mut trackers = inner
                    .persons
                    .get(person_id)
                    .map(|p| p.trackers.clone())
                    .unwrap_or_default();
                trackers.push(device_id.clone());
                trackers
            };
            self.store
                .update_person_trackers(person_id, &new_trackers)
                .await?;
            {
                let mut inner = self.inner.write().await;
                if let Some(p) = inner.persons.get_mut(person_id) {
                    p.trackers = new_trackers;
                }
                inner.rebuild_device_index();
            }
            info!(
                person_id = %person_id.0,
                device_id = %device_id.0,
                "auto-linked GPS tracker device to person"
            );
        }

        // Build GPS attributes.
        let now = Utc::now();
        let mut attributes = crate::model::Attributes::new();
        attributes.insert(
            TRACKER_TYPE.to_string(),
            AttributeValue::Text("gps".to_string()),
        );
        attributes.insert(
            TRACKER_LATITUDE.to_string(),
            AttributeValue::Float(latitude),
        );
        attributes.insert(
            TRACKER_LONGITUDE.to_string(),
            AttributeValue::Float(longitude),
        );

        // Emit DeviceStateChanged so the device store records the fix.
        self.bus.publish(Event::DeviceStateChanged {
            id: device_id.clone(),
            attributes: attributes.clone(),
            previous_attributes: crate::model::Attributes::new(),
        });

        // Update the tracker snapshot and immediately re-derive person state.
        self.handle_device_state_changed(&device_id, &attributes, now)
            .await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Re-derive and persist the state of a single person, emitting events if changed.
    async fn re_derive_person(&self, person_id: &PersonId) -> anyhow::Result<()> {
        let _lock = self.derive_lock.lock().await;
        let now = Utc::now();

        let (old_state, new_state, source, lat, lon, name) = {
            let inner = self.inner.read().await;
            let Some(person) = inner.persons.get(person_id) else {
                return Ok(());
            };
            let (new_state, source, lat, lon) = inner.derive_person_state(person, now);
            (
                person.state.clone(),
                new_state,
                source,
                lat,
                lon,
                person.name.clone(),
            )
        };

        if old_state == new_state {
            // Update coords even if state didn't change (GPS drift)
            if lat.is_some() || lon.is_some() {
                let mut inner = self.inner.write().await;
                if let Some(p) = inner.persons.get_mut(person_id) {
                    p.latitude = lat;
                    p.longitude = lon;
                    p.updated_at = now;
                }
            }
            return Ok(());
        }

        debug!(
            person = %person_id.0,
            from = ?old_state,
            to = ?new_state,
            "Person state changed"
        );

        // Update in-memory state
        {
            let mut inner = self.inner.write().await;
            if let Some(p) = inner.persons.get_mut(person_id) {
                p.state = new_state.clone();
                p.state_source = source.clone();
                p.latitude = lat;
                p.longitude = lon;
                p.updated_at = now;
            }
        }

        // Persist the person
        if let Some(person) = self.get_person(person_id).await {
            self.store.upsert_person(&person).await?;
        }

        // Record history
        let state_tag = person_state_tag(&new_state);
        let zone_id = match &new_state {
            PersonState::Zone { zone_id } => Some(zone_id.clone()),
            _ => None,
        };
        let history_entry = PersonHistoryEntry {
            id: 0, // store assigns the real id
            person_id: person_id.clone(),
            state: state_tag,
            zone_id,
            source_device_id: source.clone(),
            latitude: lat,
            longitude: lon,
            recorded_at: now,
        };
        self.store.record_person_history(&history_entry).await?;

        // Emit event
        self.bus.publish(Event::PersonStateChanged {
            person_id: person_id.clone(),
            person_name: name.clone(),
            from: old_state.clone(),
            to: new_state.clone(),
            source_device: source,
        });

        // Emit aggregate presence events when the home/away boundary is crossed.
        let was_home = matches!(old_state, PersonState::Home);
        let is_home = matches!(new_state, PersonState::Home);

        if is_home && !was_home {
            // Check whether this is the *first* person to arrive home.
            let anyone_was_home_before = {
                let inner = self.inner.read().await;
                inner.persons.values().any(|p| {
                    p.id != *person_id && matches!(p.state, PersonState::Home)
                })
            };
            if !anyone_was_home_before {
                self.bus.publish(Event::AnyPersonHome {
                    person_id: person_id.clone(),
                    person_name: name,
                });
            }
        } else if !is_home && was_home {
            // Check whether this was the *last* person to leave home.
            let anyone_still_home = {
                let inner = self.inner.read().await;
                inner.persons.values().any(|p| {
                    p.id != *person_id && matches!(p.state, PersonState::Home)
                })
            };
            if !anyone_still_home {
                self.bus.publish(Event::AllPersonsAway);
            }
        }

        Ok(())
    }

    /// Re-derive state for every person (called after zone changes).
    async fn re_derive_all_persons(&self) -> anyhow::Result<()> {
        let person_ids: Vec<PersonId> = self
            .inner
            .read()
            .await
            .persons
            .keys()
            .cloned()
            .collect();
        for person_id in &person_ids {
            if let Err(e) = self.re_derive_person(person_id).await {
                error!(person_id = %person_id.0, error = %e, "re-derive failed");
            }
        }
        Ok(())
    }
}

/// Convert a [`PersonState`] to its string tag for storage.
pub fn person_state_tag(state: &PersonState) -> String {
    match state {
        PersonState::Home => "home".to_string(),
        PersonState::Away => "away".to_string(),
        PersonState::Zone { zone_id } => format!("zone:{}", zone_id.0),
        PersonState::Room { room_id } => format!("room:{}", room_id.0),
        PersonState::Unknown => "unknown".to_string(),
    }
}

/// Parse a stored state tag back into a [`PersonState`].
pub fn person_state_from_tag(tag: &str) -> PersonState {
    match tag {
        "home" => PersonState::Home,
        "away" => PersonState::Away,
        "unknown" => PersonState::Unknown,
        s if s.starts_with("zone:") => PersonState::Zone {
            zone_id: ZoneId(s[5..].to_string()),
        },
        s if s.starts_with("room:") => PersonState::Room {
            room_id: RoomId(s[5..].to_string()),
        },
        _ => PersonState::Unknown,
    }
}
