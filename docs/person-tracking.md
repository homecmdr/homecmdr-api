# Person & Location Tracking — Implementation Plan

**Branch:** `feat/person-tracking`  
**Repo:** `homecmdr-api`  
**Status:** In Progress

---

## Decisions Log

| Decision | Choice | Rationale |
|---|---|---|
| Architecture | Option 3 — Hybrid (person as named entity, tracked via device capabilities) | Mirrors Home Assistant's proven design. Separates identity (who) from tracking (how). Multiple tracker devices can feed one person's state. |
| Tracker priority | HA-style: stationary-home > GPS > stationary-not-home | Prevents GPS boundary flapping near home. Stationary (wifi/BLE) wins when at home. |
| Home zone | Auto-created from `[locale]` lat/lon + `home_zone_radius_meters` in config | No API management needed for home zone. Radius configurable in `default.toml`. |
| Auto-create tracker devices | Yes — first `/ingest/location` call auto-registers the device | Frictionless companion app onboarding. |
| Person history | Yes — stored in `person_history` table, pruned by `retention_days` | Enables future activity pattern queries and audit. |
| `PersonState::Room` variant | Pre-modelled now, empty for future BLE indoor tracking | Avoids a breaking enum change when indoor room tracking is added later. |
| User-account link | `ApiKeyRecord.person_id: Option<PersonId>` — API key linked to person | Backward-compatible. Companion app key identifies which person is reporting without needing `person_id` in the request body. Full username/password user accounts are a separate future feature. |
| Dashboard | Deferred — implement after API is complete | `homecmdr-dash` changes are out of scope for this branch. |

---

## Architecture Overview

```
Companion App / BLE Tag / Router
         │  POST /ingest/location
         ▼
   Tracker Device (DeviceKind::Sensor + tracker.* capabilities)
         │  DeviceStateChanged event
         ▼
   PersonRegistry (subscribes to device events)
         │  derives state from linked trackers
         │  runs zone detection (haversine)
         │  applies consider_home debounce
         ▼
   Person { state: Home | Away | Zone(id) | Room(id) | Unknown }
         │  PersonStateChanged event
         ▼
   Automation Engine (PersonStateChange trigger / PersonState condition)
```

### Tracker State Priority (HA-style)

1. Any linked **stationary** tracker (`tracker.type = "stationary" | "ble"`) with `tracker.state = "home"` → `PersonState::Home`
2. Else most-recently-updated **GPS** tracker → zone match via haversine or `PersonState::Away`
3. Else any stationary tracker with `tracker.state = "not_home"` → `PersonState::Away`
4. Else → `PersonState::Unknown`

### Home Zone

Auto-created at startup from `[locale]` `latitude`, `longitude`, and `home_zone_radius_meters` (default 100 m).  
ID is always `"home"`. Cannot be deleted via API.

---

## Phases & Progress

### Phase 1 — Core Models & Traits (`crates/core`)

- [x] Create `feat/person-tracking` branch
- [ ] `model.rs` — add `PersonId`, `ZoneId`, `PersonState`, `Person`, `Zone`
- [ ] `capability.rs` — add `TRACKER_CAPABILITIES` (6 caps: `tracker.type`, `tracker.state`, `tracker.latitude`, `tracker.longitude`, `tracker.accuracy`, `tracker.consider_home`)
- [ ] `event.rs` — add `PersonStateChanged`, `PersonAdded`, `PersonUpdated`, `PersonRemoved`, `ZoneAdded`, `ZoneUpdated`, `ZoneRemoved`
- [ ] `store.rs` — add `person_id` to `ApiKeyRecord`; add `person_id` param + `list_api_keys_for_person` to `ApiKeyStore`; add `PersonStore` trait + `PersonHistoryEntry`
- [ ] `config/system.rs` — add `home_zone_radius_meters: f64` (default 100.0) to `LocaleConfig`
- [ ] `person_registry.rs` — new file: `PersonRegistry` with state derivation, zone detection, consider_home debounce, auto-device creation, event emission
- [ ] `lib.rs` — export `person_registry` module

### Phase 2 — Storage (`crates/store-sql`, `crates/store-postgres`)

- [ ] `store-sql/src/sqlite.rs` — schema v5 migration (add `person_id` to `api_keys`; create `persons`, `person_trackers`, `zones`, `person_history` tables); implement `PersonStore`; update `ApiKeyStore` impl
- [ ] `store-postgres/src/postgres.rs` — same for PostgreSQL

### Phase 3 — API State & Startup

- [ ] `crates/api/src/state.rs` — add `PersonRegistry` to `AppState`
- [ ] App startup — wire `PersonRegistry` (load persons + zones from store, subscribe to event bus, spawn background tasks)

### Phase 4 — API Handlers & Routes

- [ ] `crates/api/src/dto.rs` — add `PersonResponse`, `CreatePersonRequest`, `UpdatePersonRequest`, `ZoneResponse`, `CreateZoneRequest`, `UpdateZoneRequest`, `IngestLocationRequest`; update `CreateApiKeyRequest` / `ApiKeyResponse` with optional `person_id`
- [ ] `crates/api/src/handlers/persons.rs` — **new file**: person CRUD + zone CRUD handlers
- [ ] `crates/api/src/handlers/events.rs` — add `POST /ingest/location`
- [ ] `crates/api/src/handlers/admin.rs` — thread `person_id` through `create_api_key`
- [ ] `crates/api/src/middleware.rs` — auth context carries `Option<PersonId>`
- [ ] `crates/api/src/router.rs` — wire all new routes

### Phase 5 — Automation Engine

- [ ] `crates/automations/src/types.rs` — add `PersonStateChange`, `AllPersonsAway`, `AnyPersonArrived` trigger variants; add `PersonState`, `AllPersonsAway`, `AnyPersonHome` condition variants
- [ ] `crates/automations/src/engine.rs` — subscribe to `PersonStateChanged` events; implement trigger matching for person triggers
- [ ] `crates/lua-host/src/context.rs` — add `ctx:person_state(id)`, `ctx:all_persons_away()`, `ctx:any_person_home()`, `ctx:persons_in_zone(zone_id)` Lua helpers

### Phase 6 — Configuration

- [ ] `config/default.toml` — add `home_zone_radius_meters = 100` under `[locale]`

### Phase 7 — Final Verification

- [ ] `cargo build` passes with no errors
- [ ] `cargo clippy` clean
- [ ] Manual API smoke test (create person, create zone, ingest location, verify state derivation, verify automation trigger)

---

## New Routes

| Method | Path | Auth | Description |
|---|---|---|---|
| GET | `/persons` | Read | List all persons with current state |
| POST | `/persons` | Write | Create a person |
| GET | `/persons/{id}` | Read | Get person with state + linked key metadata |
| PATCH | `/persons/{id}` | Write | Update name, picture, trackers |
| DELETE | `/persons/{id}` | Write | Remove person |
| GET | `/persons/{id}/history` | Read | State transition history |
| GET | `/zones` | Read | List zones with person counts |
| POST | `/zones` | Write | Create a zone |
| GET | `/zones/{id}` | Read | Get zone |
| PATCH | `/zones/{id}` | Write | Update zone |
| DELETE | `/zones/{id}` | Write | Remove zone (home zone protected) |
| POST | `/ingest/location` | Write | Companion app GPS push |

### `POST /ingest/location` payload

```json
{
  "device_id": "companion:andy_phone",
  "latitude": 51.5074,
  "longitude": -0.1278,
  "accuracy": 12.0,
  "battery": 82
}
```

If the calling API key has a linked `person_id`, `device_id` defaults to `companion:{person_id}` when omitted.

---

## New Automation Triggers

```lua
-- Fire when a specific person's state changes
{ kind = "PersonStateChange", person_id = "andy", to = "home" }
{ kind = "PersonStateChange", person_id = "andy", from = "home" }

-- Fire when the last person leaves home
{ kind = "AllPersonsAway" }

-- Fire when the first person arrives home
{ kind = "AnyPersonArrived" }
```

## New Automation Conditions

```lua
conditions = {
    { kind = "PersonState", person_id = "andy", state = "home" },
    { kind = "AllPersonsAway" },
    { kind = "AnyPersonHome" },
}
```

## Lua Context Helpers

```lua
ctx:person_state("andy")         -- "home" | "away" | "work" | "unknown"
ctx:all_persons_away()           -- bool
ctx:any_person_home()            -- bool
ctx:persons_in_zone("work")      -- { "andy", "sarah" }
```

---

## File Change Index

| File | Change |
|---|---|
| `crates/core/src/model.rs` | Add `PersonId`, `ZoneId`, `PersonState`, `Person`, `Zone` |
| `crates/core/src/capability.rs` | Add `TRACKER_CAPABILITIES` (6 capabilities) + add to `ALL_CAPABILITIES` |
| `crates/core/src/event.rs` | Add 7 new person/zone event variants |
| `crates/core/src/store.rs` | `person_id` on `ApiKeyRecord`; `ApiKeyStore` updates; new `PersonStore` trait + `PersonHistoryEntry` |
| `crates/core/src/person_registry.rs` | **New** — `PersonRegistry` implementation |
| `crates/core/src/config/system.rs` | `home_zone_radius_meters` on `LocaleConfig` |
| `crates/core/src/lib.rs` | Export `person_registry` module |
| `crates/store-sql/src/sqlite.rs` | Schema v5 migration; `PersonStore` impl; `ApiKeyStore` update |
| `crates/store-postgres/src/postgres.rs` | Schema v5 migration; `PersonStore` impl; `ApiKeyStore` update |
| `crates/api/src/state.rs` | Add `PersonRegistry` to `AppState` |
| `crates/api/src/middleware.rs` | Auth context carries `Option<PersonId>` |
| `crates/api/src/router.rs` | Wire person, zone, `/ingest/location` routes |
| `crates/api/src/handlers/persons.rs` | **New** — person + zone CRUD handlers |
| `crates/api/src/handlers/events.rs` | Add `POST /ingest/location` |
| `crates/api/src/handlers/admin.rs` | Thread `person_id` through `create_api_key` |
| `crates/api/src/dto.rs` | Person/zone DTOs; update `ApiKey` DTOs |
| `crates/automations/src/types.rs` | New person triggers + conditions |
| `crates/automations/src/engine.rs` | Subscribe to `PersonStateChanged`; implement person triggers |
| `crates/lua-host/src/context.rs` | Person state Lua helpers |
| `config/default.toml` | Add `home_zone_radius_meters = 100` under `[locale]` |

---

## Database Schema Changes (v4 → v5)

```sql
-- Extend existing api_keys table (backward-compatible nullable column)
ALTER TABLE api_keys ADD COLUMN person_id TEXT;

-- New: persons
CREATE TABLE persons (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    picture     TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- New: tracker devices linked to each person, priority-ordered
CREATE TABLE person_trackers (
    person_id   TEXT NOT NULL REFERENCES persons(id) ON DELETE CASCADE,
    device_id   TEXT NOT NULL,
    priority    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (person_id, device_id)
);

-- New: geographic zones
CREATE TABLE zones (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    latitude        REAL NOT NULL,
    longitude       REAL NOT NULL,
    radius_meters   REAL NOT NULL DEFAULT 100.0,
    icon            TEXT,
    passive         INTEGER NOT NULL DEFAULT 0
);

-- New: person location state history
CREATE TABLE person_history (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    person_id         TEXT NOT NULL,
    state             TEXT NOT NULL,
    zone_id           TEXT,
    source_device_id  TEXT,
    latitude          REAL,
    longitude         REAL,
    recorded_at       TEXT NOT NULL
);
```

---

## Future Work (out of scope for this branch)

- `homecmdr-dash` People + Zones UI tabs
- Full user accounts (username/password login, `users` table, `Person.user_id`)
- Indoor BLE room-level tracking (`PersonState::Room(RoomId)` — enum variant pre-modelled)
- Companion app (iOS/Android) — contract defined by `POST /ingest/location`
- BLE beacon/scanner plugin
