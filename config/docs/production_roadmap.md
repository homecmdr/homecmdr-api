# Production Roadmap

This document tracks all planned work toward a first production release.
Each item is updated in place as work progresses.

Status legend: `[ ]` pending · `[~]` in progress · `[x]` done

---

## Official adapters

| Adapter | Status |
|---|---|
| zigbee2mqtt | official |
| open-meteo | official |
| elgato-lights | official |
| roku-tv | bundled example |
| ollama | bundled example |

---

## Phase 1 — Production foundation

### 1.1 Portable paths and environment variable overrides
- [x] `SMART_HOME_CONFIG` env var overrides the `--config` flag default
- [x] `SMART_HOME_DATA_DIR` env var prefixes relative `database_url` paths
- [x] Document both vars in `config/default.toml` and AGENTS.md

### 1.2 Request logging
- [x] Add `tower_http::trace::TraceLayer` to the axum router

### 1.3 Authentication and authorisation
- [x] Add `[auth]` section to `Config` in `crates/core/src/config.rs`
  - `master_key: String` (required; also readable from `SMART_HOME_MASTER_KEY` env var)
- [x] Add `api_keys` table to SQLite schema in `crates/store-sql`
  - columns: `id`, `key_hash` (SHA-256 hex), `label`, `role`, `created_at`, `last_used_at`
- [x] Add `ApiKeyStore` trait to `crates/core` with `create`, `list`, `revoke`, `lookup` methods
- [x] Implement `ApiKeyStore` in `crates/store-sql`
- [x] Add axum bearer-token middleware extractor in `crates/api`
  - Public routes (no auth required): `GET /health`, `GET /ready`
  - All other routes require a valid bearer token
  - Master key from config is always accepted (bypasses role checks)
- [x] Define roles: `read`, `write`, `admin`, `automation`
  - `read` — GET device/room/history/scene/automation endpoints
  - `write` — POST commands, execute scenes, manage rooms and groups
  - `admin` — reload configs, diagnostics, key management
  - `automation` — write + invoke LLM targets (for MCP agents)
- [x] Add key management endpoints (admin role required)
  - `POST   /auth/keys`       — create a new key (`label`, `role` in body)
  - `GET    /auth/keys`       — list all keys (hashes redacted)
  - `DELETE /auth/keys/{id}`  — revoke a key
- [x] Apply role enforcement to existing endpoints
- [x] Add auth section to OpenAPI spec (`config/docs/openapi.yaml`)
- [x] Update AGENTS.md with auth notes
- [x] Add tests: unauthenticated requests rejected, roles enforced, master key accepted

### 1.4 Deployment packaging
- [x] `Dockerfile` — multi-stage build (rust builder → slim runtime)
  - `/config` and `/data` as volumes (Lua files editable outside container)
  - `SMART_HOME_CONFIG` and `SMART_HOME_DATA_DIR` set via environment
- [x] `docker-compose.yml` — single-service compose for home server deployment
- [x] `deploy/smart-home.service` — systemd unit file for bare-metal install
- [x] `docs/deployment.md` — install guide covering Docker, bare metal, and dev instance
  - Dev instance: separate port, DB file, MQTT client ID — no code changes required

---

## Phase 2 — Official adapter completeness

### 2.1 zigbee2mqtt — device type expansion
- [x] Sensors: motion, contact/door, temperature, humidity, occupancy, smoke, water leak
- [x] Battery level (`battery`) for all battery-powered devices
- [x] Locks: lock/unlock state and commands
- [x] Covers/blinds: position, tilt, open/close commands
- [x] Tests for new device kinds

### 2.2 open-meteo — v2 current API
- [x] Switch from `?current_weather=true` to `?current=` parameter set
- [x] Add: apparent temperature, relative humidity, precipitation, cloud cover,
        UV index, surface pressure, wind gusts, weather code, is_day
- [x] Update tests

### 2.3 elgato-lights — production review
- [x] Fix `state` field (currently always `"online"` regardless of reachability)
- [x] Document single-device-per-config limitation in README

### 2.4 Docs cleanup
- [x] Mark roku-tv and ollama as community/example in README and adapter docs
- [x] Update README trigger type section (currently lists 2 of 9 implemented types)

---

## Phase 3 — Dashboard

- [x] Add `[dashboard]` section to `Config`: `enabled`, `directory`
- [x] Mount `tower_http::services::ServeDir` on `/dashboard` when enabled
- [x] Bundle Alpine.js and htmx locally (remove CDN dependency)
- [x] Expand template: light controls (power, brightness, colour temperature),
        scene execution, room listing, live event feed
- [x] Dashboard requests authenticated via the same bearer token middleware

---

## Phase 4 — MCP tooling

### 4.1 Lua authoring (basic users and AI)
- [x] MCP server wrapping the HTTP API
- [x] Add file read/write endpoints to the API (`admin`/`automation` role)
- [x] Tools: list/read/write Lua files, trigger reload, query device state

### 4.2 Adapter authoring (advanced users and AI)
- [x] MCP tools: scaffold new adapter crate, run cargo check/test,
        inspect live registry output

---

## Phase 5 — Hardening

- [x] Rate limiting on command and scene execution endpoints
- [x] Graceful shutdown: drain in-flight requests with a timeout
- [x] Make automation concurrency limit and execution timeout configurable
- [x] PostgreSQL backend (implement or formally defer with docs)
