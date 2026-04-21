# HomeCmdr Agent Notes

- Trust executable sources over prose when they disagree. Read `Cargo.toml`, `config/default.toml`, `crates/api/src/main.rs`, and the relevant crate before trusting `README.md`.
- Highest-value repo docs are in `config/docs/`: `agent_workflows.md`, `adapter_authoring_guide.md`, `lua_runtime_guide.md`, `api_reference.md`, and `plugin_authoring_guide.md`.
- The API does not serve a dashboard. Dashboards are external clients. The reference dashboard is at https://github.com/homecmdr/homecmdr-dash (Alpine.js + vanilla CSS, no build step). Use it as the canonical starting point when building or extending a dashboard.

## Commands

- Format: `cargo fmt --all`
- Fast safety check: `cargo check --workspace`
- Full test suite: `cargo test --workspace`
- Run the API: `cargo run -p api -- --config config/default.toml`
- Run the MCP server: `cargo run -p mcp-server -- --token <BEARER_TOKEN>` (the API must already be running; `--api-url` defaults to `http://127.0.0.1:3001`, `--workspace` defaults to `.`; token can also be set via `HOMECMDR_TOKEN` env var)
- `api` also defaults to `config/default.toml` when `--config` is omitted.
- `HOMECMDR_CONFIG` env var overrides the `--config` default entirely.
- `HOMECMDR_DATA_DIR` env var prefixes relative `database_url` paths (e.g. set to `/var/lib/homecmdr`).
- `HOMECMDR_MASTER_KEY` env var overrides `auth.master_key` in config.
- API bind address is configured via `api.bind_address` in `config/default.toml`; it is no longer hard-coded in `main.rs`.
- Focused tests: `cargo test -p api`, `cargo test -p homecmdr-core`, `cargo test -p homecmdr-scenes`, `cargo test -p homecmdr-automations`, `cargo test -p store-sql`, `cargo test -p adapter-open-meteo`, or `cargo test -p homecmdr-plugin-host`.

## Workspace Map

- `crates/core`: runtime contracts, config model, registry, command/capability model, event bus.
- `crates/api`: only binary; starts runtime, loads Lua assets, exposes HTTP + WebSocket API, and wires persistence/history.
- `crates/adapters`: empty shim kept for backwards compatibility; no longer links adapter crates. Do not add new deps here.
- `crates/adapter-open-meteo`: native Rust implementation retained for reference and unit tests. It is **not** registered at runtime anymore — the WASM build in `plugins/open-meteo/` is used instead.
- `crates/plugin-host`: WASM runtime (wasmtime 44, component model). Exposes `WasmAdapter`/`WasmAdapterFactory` that implement the same `Adapter`/`AdapterFactory` traits as native adapters, so `Runtime` and the API layer are unaware of WASM vs native. HTTP inside WASM is provided by `ureq` (pure-sync, no Tokio dependency).
- `plugins/open-meteo/`: the Open Meteo WASM guest crate. Lives **outside** the main workspace (has its own `[workspace]` in `Cargo.toml`). Build target is `wasm32-wasip2`. Compiled binary is committed to `config/plugins/open_meteo.wasm`.
- `crates/scenes`, `crates/automations`, `crates/lua-host`: Lua asset loading/execution. `mlua` is vendored Lua 5.4, so no system Lua dependency should be needed.
- `crates/store-sql`: SQLite store plus in-code schema initialization/migrations and history storage.
- `crates/store-postgres`: PostgreSQL store implementing the same `DeviceStore` + `ApiKeyStore` traits. Wired at startup when `persistence.backend = "postgres"` is set in config.
- `crates/mcp-server`: standalone MCP server binary; JSON-RPC 2.0 over stdio (MCP protocol version `2024-11-05`). Launched by an MCP host as a subprocess — does not start automatically with the API. Proxies most tools to the HTTP API with Bearer auth; runs `cargo check`/`cargo test` as subprocesses; scaffolds new adapter crates on disk.

## Adapter Rules

- Adapters are WASM plugins, not compile-time Rust crates. The plugin system lives in `crates/plugin-host`.
- Official plugins are installed by dropping a `.wasm` binary and a `.plugin.toml` manifest into `config/plugins/`. The directory is scanned at startup when `[plugins] enabled = true` in `config/default.toml`.
- To build a new plugin: create a Rust crate **outside** the main workspace (add an empty `[workspace]` table to its `Cargo.toml`), target `wasm32-wasip2`, and implement the WIT interface at `crates/plugin-host/wit/homecmdr-plugin.wit`. See `config/docs/plugin_authoring_guide.md` for the full workflow.
- The `.plugin.toml` manifest must declare `[plugin] name = "<adapter_name>"` and `[runtime] poll_interval_secs = <n>`. The name must match the key used in `config/default.toml` under `[adapters]`.
- WASM plugins shadow native adapters with the same name (info-logged). WASM takes precedence.
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
- Automation runner limits are configurable via `[automations.runner]` in `config/default.toml`: `default_max_concurrent` (default 8) and `backstop_timeout_secs` (default 3600). They are no longer hard-coded in `crates/automations/src/lib.rs`.
- Rate limiting on write endpoints is configurable via `[api.rate_limit]` in `config/default.toml` (`enabled`, `requests_per_second`, `burst_size`). When enabled, excess requests return HTTP 429.
- Graceful shutdown drains in-flight requests with a 30-second timeout (`SHUTDOWN_DRAIN_SECS`). The server handles both SIGTERM and ctrl-c.

## Persistence And API

- Default persistence is SQLite with `database_url = "sqlite://data/homecmdr.db"` and `auto_create = true`.
- SQLite schema creation and migrations are handled inside `crates/store-sql/src/sqlite.rs`; there is no separate migration tool or `build.rs`.
- PostgreSQL is a fully implemented alternative backend (`crates/store-postgres`). Set `persistence.backend = "postgres"` and supply a `database_url` (e.g. `"postgres://user:pass@localhost/homecmdr"`) to use it. `PgPoolOptions::new().max_connections(5)` is used; no TimescaleDB or other extensions are required.
- Useful live inspection endpoints while developing: `/health`, `/ready`, `/diagnostics`, `/adapters`, `/devices`, `/rooms`, `/capabilities`, and WebSocket `/events`.
- History and audit endpoints are implemented; this repo is not current-state-only anymore.

## Authentication

- All routes except `GET /health` and `GET /ready` require a `Bearer` token in the `Authorization` header.
- Roles: `read`, `write`, `admin`, `automation`. Role satisfaction rules: `read` satisfies Read; `write` satisfies Write; `admin` satisfies Admin only; `automation` satisfies Automation and Admin.
- Route tiers: **Read** (all GET endpoints + WebSocket), **Write** (mutation endpoints), **Admin** (diagnostics, reload, key management).
- Master key is configured via `auth.master_key` in `config/default.toml`, overridable with `HOMECMDR_MASTER_KEY` env var. It is stored and compared as a SHA-256 hex digest and always grants the `Admin` role.
- API keys are stored in the `api_keys` SQLite table (schema V4) as SHA-256 hashes. Managed via `POST /auth/keys`, `GET /auth/keys`, `DELETE /auth/keys/{id}`.
- `ApiKeyRole` enum and `ApiKeyStore` trait live in `crates/core/src/store.rs`; the SQLite implementation is in `crates/store-sql/src/sqlite.rs`.
- Auth middleware uses `middleware::from_fn` with closure capture of `AppState` (not `from_fn_with_state`), because `axum::body::Body: !Sync` makes `&Request: !Send`. The `check_auth` helper extracts the bearer token as an owned `String` before any `.await` for the same reason.
- Tests: `test_client()` returns a `reqwest::Client` with `Authorization: Bearer <TEST_MASTER_KEY>` as a default header. WebSocket tests use `authed_ws_request(&url)` which calls `into_client_request()` on the URL (to generate `Sec-WebSocket-Key`) then injects the auth header.
