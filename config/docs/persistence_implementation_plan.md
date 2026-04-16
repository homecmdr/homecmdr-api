# Smart Home Persistence Plan

> **Goal:** Add durable current-state persistence while keeping the in-memory registry as the hot cache.
> **Initial backend:** SQLite
> **Future backend:** PostgreSQL
> **Historical telemetry:** deferred until current-state persistence is stable
> **ORM:** no; use `sqlx`

---

## Decisions

- Keep `DeviceRegistry` as the in-memory source used by the runtime, API, and WebSocket path.
- Add a persistence abstraction in `crates/core`, not in `crates/api`.
- Add a new crate for SQL persistence, not API-specific code.
- Start with SQLite for local development and single-node use.
- Design the abstraction so PostgreSQL can be added later with minimal interface churn.
- Persist current device state for all active devices.
- Add historical telemetry later as a separate store/table and make telemetry opt-in per device/capability.

---

## Target Crate Layout

```text
smart-home/
├── config/
│   └── default.toml
├── crates/
│   ├── core/
│   │   └── src/
│   │       ├── store.rs            # trait(s) only
│   │       ├── runtime.rs
│   │       ├── registry.rs
│   │       └── ...
│   ├── api/
│   │   └── src/main.rs             # composition only
│   ├── adapter-open-meteo/
│   └── store-sql/                  # SQLite first, PostgreSQL later
│       └── src/
│           ├── lib.rs
│           ├── sqlite.rs
│           ├── schema.rs or migrations/
│           └── ...
```

---

## Why This Plan

- Keeps `api` clean and focused on HTTP/WebSocket composition.
- Preserves the fast in-memory read path already used by `/devices` and `/events`.
- Makes SQLite easy for development.
- Leaves a clear path to PostgreSQL without rewriting runtime or API boundaries.
- Avoids premature telemetry/storage complexity.

---

## Backend Recommendation

### Short term

- Use SQLite.
- Store current device state in a single table.
- Use `sqlx`, not an ORM.

### Medium term

- Add PostgreSQL support in the same `store-sql` crate.
- Keep trait interface identical.
- Use Docker for local PostgreSQL only when PostgreSQL support is actually implemented and tested.

### Long term

- Add telemetry/history tables separately.
- Add config to select which devices/capabilities are historized.

---

## Non-Goals For This Work

- No direct DB reads in HTTP handlers for normal current-state requests.
- No replacement of the in-memory registry.
- No telemetry/history ingestion yet.
- No lighting or other new capability domains.
- No multi-node clustering work yet.

---

## Phase 10 — Persistence Abstraction

**Goal:** Add persistence-neutral interfaces to `core`.

### Task 10.1 — Add store traits in `crates/core/src/store.rs`

Define a current-state persistence trait.

Suggested interface:

```rust
#[async_trait::async_trait]
pub trait DeviceStore: Send + Sync + 'static {
    async fn load_all_devices(&self) -> anyhow::Result<Vec<Device>>;
    async fn save_device(&self, device: &Device) -> anyhow::Result<()>;
    async fn delete_device(&self, id: &DeviceId) -> anyhow::Result<()>;
}
```

Optional later trait, do not implement yet:

```rust
#[async_trait::async_trait]
pub trait TelemetryStore: Send + Sync + 'static {
    async fn append_telemetry(&self, record: TelemetryRecord) -> anyhow::Result<()>;
}
```

**Interface Contract:**

- `DeviceStore` is for latest state only.
- Telemetry/history is explicitly not part of `DeviceStore`.

**Acceptance Criteria:**

- `core` compiles with the trait added.
- Existing runtime/API code still compiles after any necessary composition changes.
- No SQL or backend-specific code appears in `core`.

---

## Phase 11 — Registry Restore Support

**Goal:** Allow startup hydration from durable storage without removing the in-memory cache.

### Task 11.1 — Add restore path to `DeviceRegistry` or `Runtime`

Requirements:

- Load all persisted devices once during startup.
- Populate the in-memory registry before adapters begin polling.
- Avoid publishing live external events during restore unless explicitly intended.
- Preserve validation of canonical capability shapes during restore.

Possible approaches:

- `DeviceRegistry::restore(devices: Vec<Device>)`
- or `Runtime::new_with_store(...)` plus internal hydration
- or dedicated bootstrap function in composition layer

Preferred approach:

- Keep restore orchestration outside the registry if possible.
- Add a registry method only if it avoids duplication cleanly.

**Acceptance Criteria:**

- Startup hydration restores valid devices into memory.
- Invalid persisted devices fail clearly with context.
- Restore order does not race with adapter startup.

---

## Phase 12 — SQL Persistence Crate

**Goal:** Add a dedicated SQL persistence crate with SQLite support first.

### Task 12.1 — Create `crates/store-sql`

Dependencies:

- `sqlx`
- `tokio`
- `serde_json`
- `anyhow`
- `smart-home-core`

Suggested `sqlx` features for first pass:

- `runtime-tokio-rustls`
- `sqlite`
- `postgres`
- `chrono`
- `json`

Even if PostgreSQL implementation is deferred, enabling both backends now is acceptable if it does not complicate the crate too much.

### Task 12.2 — Implement `SqliteDeviceStore`

Responsibilities:

- open DB connection/pool
- create schema or run migrations
- load all devices
- upsert device state
- delete device state

Suggested table for current state:

```sql
CREATE TABLE devices (
    device_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    attributes_json TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

Notes:

- `attributes_json` and `metadata_json` can be JSON text in SQLite.
- Keep schema simple.
- Do not split capabilities into relational columns yet.

**Acceptance Criteria:**

- Can save a device and load it back losslessly.
- Can overwrite an existing device by ID.
- Can delete by ID.
- Serialization round-trips through DB cleanly.

---

## Phase 13 — Composition In API

**Goal:** Compose runtime + store cleanly while keeping API transport concerns separate.

### Task 13.1 — Wire persistence in `crates/api/src/main.rs`

Requirements:

- Load config.
- Create SQLite store.
- Hydrate registry from store before starting adapters.
- Subscribe to the event bus.
- Persist:
  - `DeviceAdded`
  - `DeviceStateChanged`
  - `DeviceRemoved`
- Do persistence asynchronously so the hot in-memory path remains primary.
- Log persistence failures with context.
- Do not crash the runtime for transient DB write failures.

Suggested flow:

1. load config
2. create store
3. create runtime
4. hydrate registry from store
5. start persistence task subscribed to event bus
6. start runtime
7. start HTTP server

**Acceptance Criteria:**

- App starts with empty DB.
- App restarts and restores previously persisted devices.
- Device updates remain visible immediately through in-memory API responses.
- Persistence failures do not crash API/runtime.

---

## Phase 14 — Configuration

**Goal:** Add configuration for persistence without locking into PostgreSQL too early.

### Task 14.1 — Extend config schema

Suggested config shape:

```toml
[persistence]
enabled = true
backend = "sqlite"            # sqlite | postgres
database_url = "sqlite://data/smart-home.db"
auto_create = true

[telemetry]
enabled = false

[telemetry.selection]
device_ids = []
capabilities = []
adapter_names = []
```

Notes:

- `telemetry` section is for future use.
- It is fine if selection config is parsed now but unused until telemetry exists.

**Acceptance Criteria:**

- Config loads with SQLite defaults.
- Invalid persistence backend fails clearly.
- Missing DB URL fails clearly when persistence is enabled.

---

## Phase 15 — Tests

**Goal:** Prove persistence works without destabilizing runtime/API/WS behavior.

### Task 15.1 — Store tests

Add tests for:

- save/load single device
- overwrite existing device
- delete device
- load multiple devices

### Task 15.2 — Hydration tests

Add tests for:

- startup restores devices from SQLite into runtime registry
- restored devices appear in `/devices`
- restored devices do not break WebSocket operation

### Task 15.3 — End-to-end persistence test

Test flow:

1. create temp SQLite DB
2. start app
3. adapter inserts device(s)
4. verify `/devices`
5. stop app
6. restart app with same DB
7. verify devices are present before new adapter writes arrive

**Acceptance Criteria:**

- `cargo check --workspace` passes
- `cargo clippy --workspace -- -D warnings` passes
- `cargo test --workspace` passes

---

## Phase 16 — PostgreSQL Preparation

**Goal:** Make future PostgreSQL support easy without implementing telemetry yet.

### Task 16.1 — Keep SQL backend-neutral where practical

Requirements:

- Keep SQL operations simple and portable.
- Avoid SQLite-specific assumptions in trait design.
- Keep serialized `attributes` and `metadata` as JSON payloads.
- Isolate backend-specific SQL to backend modules.

### Task 16.2 — Plan PostgreSQL implementation

Future implementation:

- `PostgresDeviceStore` in `crates/store-sql`
- same `DeviceStore` trait
- same current-state semantics

Local dev later:

- use Docker Compose or `docker run` for PostgreSQL only when Postgres backend work begins

Example future dev command:

```bash
docker run --name smart-home-postgres \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_USER=postgres \
  -e POSTGRES_DB=smarthome \
  -p 5432:5432 \
  -d postgres:16
```

---

## Telemetry Plan (Deferred)

**Goal:** Decide later which devices/capabilities produce historical data.

When implemented:

- keep current-state persistence mandatory for all active devices
- make telemetry opt-in
- selection should support:
  - adapter name
  - device ID
  - capability key

Suggested future table:

```sql
CREATE TABLE device_telemetry (
    id INTEGER PRIMARY KEY,
    device_id TEXT NOT NULL,
    capability TEXT NOT NULL,
    value_json TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);
```

For PostgreSQL/TimescaleDB later:

- convert `device_telemetry` into a hypertable
- keep `devices` as a normal current-state table

---

## Tooling Recommendation

Use `sqlx`, not an ORM.

Why:

- explicit SQL fits this domain well
- easier SQLite/PostgreSQL dual support
- JSON payload storage is straightforward
- less framework surface area than an ORM
- cleaner separation from core domain model

---

## Progress Checklist

- [ ] Add `DeviceStore` trait in `core`
- [ ] Add startup hydration support
- [ ] Create `crates/store-sql`
- [ ] Implement `SqliteDeviceStore`
- [ ] Add persistence config
- [ ] Wire store into API composition
- [ ] Persist `DeviceAdded`
- [ ] Persist `DeviceStateChanged`
- [ ] Persist `DeviceRemoved`
- [ ] Add SQLite persistence tests
- [ ] Add restart hydration test
- [ ] Update README with persistence setup
- [ ] Re-run check/clippy/test successfully

---

## Exit Criteria

This plan is complete when:

- current device state survives restart via SQLite
- API and WebSocket still use the in-memory registry/event bus
- persistence code lives outside `api` transport concerns
- PostgreSQL can be added later behind the same trait
- telemetry remains deferred but planned
