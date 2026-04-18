# Smart Home Agent Notes

- Trust executable sources over prose when they disagree. Read `Cargo.toml`, `config/default.toml`, `crates/api/src/main.rs`, and the relevant crate before trusting `README.md`.
- Highest-value repo docs are in `config/docs/`: `agent_workflows.md`, `adapter_authoring_guide.md`, `lua_runtime_guide.md`, and `api_reference.md`.

## Commands

- Format: `cargo fmt --all`
- Fast safety check: `cargo check --workspace`
- Full test suite: `cargo test --workspace`
- Run the API: `cargo run -p api -- --config config/default.toml`
- `api` also defaults to `config/default.toml` when `--config` is omitted.
- `SMART_HOME_CONFIG` env var overrides the `--config` default entirely.
- `SMART_HOME_DATA_DIR` env var prefixes relative `database_url` paths (e.g. set to `/var/lib/smart-home`).
- `SMART_HOME_MASTER_KEY` env var overrides `auth.master_key` in config.
- API bind address is configured via `api.bind_address` in `config/default.toml`; it is no longer hard-coded in `main.rs`.
- Focused tests: `cargo test -p api`, `cargo test -p smart-home-core`, `cargo test -p smart-home-scenes`, `cargo test -p smart-home-automations`, `cargo test -p store-sql`, or `cargo test -p adapter-open-meteo` / `adapter-elgato-lights` / `adapter-ollama` / `adapter-roku-tv` / `adapter-zigbee2mqtt`.

## Workspace Map

- `crates/core`: runtime contracts, config model, registry, command/capability model, event bus.
- `crates/api`: only binary; starts runtime, loads Lua assets, exposes HTTP + WebSocket API, and wires persistence/history.
- `crates/adapters`: link crate that pulls adapter crates into the final binary. Adapter discovery is compile-time via `inventory`.
- `crates/adapter-*`: each adapter owns its own config parsing/validation, protocol client, state mapping, command handling, and tests.
- `crates/scenes`, `crates/automations`, `crates/lua-host`: Lua asset loading/execution. `mlua` is vendored Lua 5.4, so no system Lua dependency should be needed.
- `crates/store-sql`: SQLite store plus in-code schema initialization/migrations and history storage.

## Adapter Rules

- New adapters must be linked in three places: root `Cargo.toml`, `crates/adapters/Cargo.toml`, and `crates/adapters/src/lib.rs`.
- Register factories with `inventory::submit!`; `crates/api` discovers factories through `registered_adapter_factories()`.
- Do not add adapter-specific config structs to `crates/core/src/config.rs`; adapter config is loaded as generic JSON and parsed inside the adapter crate.
- Do not add adapter-specific startup wiring to `crates/api/src/main.rs`.
- Device IDs must stay namespaced as `"{adapter_name}:{vendor_id}"`.
- Preserve existing `room_id` when adapters refresh device state.

## Runtime And Asset Gotchas

- Default asset directories come from `config/default.toml`: `config/scenes`, `config/automations`, `config/scripts`.
- Scenes and automations are loaded at API startup, and can be manually reloaded with `POST /scenes/reload` and `POST /automations/reload`.
- Scripts can be acknowledged via `POST /scripts/reload`, and optional file-watch hot reload can be enabled with `[scenes].watch`, `[automations].watch`, and `[scripts].watch` in `config/default.toml`.
- `config/scripts` modules are loadable from scenes/automations with `require(...)`.
- Current automation trigger types are implemented in code, not just docs: `device_state_change`, `weather_state`, `adapter_lifecycle`, `system_error`, `wall_clock`, `cron`, `sunrise`, `sunset`, `interval`.
- Automation runner limits are hard-coded in `crates/automations/src/lib.rs`: max 8 concurrent runs, 10s execution timeout.

## Persistence And API

- Default persistence is SQLite with `database_url = "sqlite://data/smart-home.db"` and `auto_create = true`.
- SQLite schema creation and migrations are handled inside `crates/store-sql/src/sqlite.rs`; there is no separate migration tool or `build.rs`.
- `postgres` is listed in config/types but `crates/api` currently rejects it as unimplemented.
- Useful live inspection endpoints while developing: `/health`, `/ready`, `/diagnostics`, `/adapters`, `/devices`, `/rooms`, `/capabilities`, and WebSocket `/events`.
- History and audit endpoints are implemented; this repo is not current-state-only anymore.

## Authentication

- All routes except `GET /health` and `GET /ready` require a `Bearer` token in the `Authorization` header.
- Roles: `read`, `write`, `admin`, `automation`. Role satisfaction rules: `read` satisfies Read; `write` satisfies Write; `admin` satisfies Admin only; `automation` satisfies Automation and Admin.
- Route tiers: **Read** (all GET endpoints + WebSocket), **Write** (mutation endpoints), **Admin** (diagnostics, reload, key management).
- Master key is configured via `auth.master_key` in `config/default.toml`, overridable with `SMART_HOME_MASTER_KEY` env var. It is stored and compared as a SHA-256 hex digest and always grants the `Admin` role.
- API keys are stored in the `api_keys` SQLite table (schema V4) as SHA-256 hashes. Managed via `POST /auth/keys`, `GET /auth/keys`, `DELETE /auth/keys/{id}`.
- `ApiKeyRole` enum and `ApiKeyStore` trait live in `crates/core/src/store.rs`; the SQLite implementation is in `crates/store-sql/src/sqlite.rs`.
- Auth middleware uses `middleware::from_fn` with closure capture of `AppState` (not `from_fn_with_state`), because `axum::body::Body: !Sync` makes `&Request: !Send`. The `check_auth` helper extracts the bearer token as an owned `String` before any `.await` for the same reason.
- Tests: `test_client()` returns a `reqwest::Client` with `Authorization: Bearer <TEST_MASTER_KEY>` as a default header. WebSocket tests use `authed_ws_request(&url)` which calls `into_client_request()` on the URL (to generate `Sec-WebSocket-Key`) then injects the auth header.
