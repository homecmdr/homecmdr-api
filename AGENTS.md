# Smart Home Agent Notes

- Trust executable sources over prose when they disagree. Read `Cargo.toml`, `config/default.toml`, `crates/api/src/main.rs`, and the relevant crate before trusting `README.md`.
- Highest-value repo docs are in `config/docs/`: `agent_workflows.md`, `adapter_authoring_guide.md`, `lua_runtime_guide.md`, and `api_reference.md`.

## Commands

- Format: `cargo fmt --all`
- Fast safety check: `cargo check --workspace`
- Full test suite: `cargo test --workspace`
- Run the API: `cargo run -p api -- --config config/default.toml`
- `api` also defaults to `config/default.toml` when `--config` is omitted.
- API bind address is configured via `api.bind_address` in `config/default.toml`; it is no longer hard-coded in `main.rs`.
- Focused tests: `cargo test -p api`, `cargo test -p smart-home-core`, `cargo test -p smart-home-scenes`, `cargo test -p smart-home-automations`, `cargo test -p store-sql`, or `cargo test -p adapter-open-meteo` / `adapter-elgato-lights` / `adapter-ollama` / `adapter-roku-tv`.

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
- Scenes and automations are loaded at API startup. `/scenes/reload` and `/automations/reload` intentionally return not implemented; restart the API after editing Lua assets.
- `config/scripts` modules are loadable from scenes/automations with `require(...)`.
- Current automation trigger types are implemented in code, not just docs: `device_state_change`, `weather_state`, `adapter_lifecycle`, `system_error`, `wall_clock`, `cron`, `sunrise`, `sunset`, `interval`.
- Automation runner limits are hard-coded in `crates/automations/src/lib.rs`: max 8 concurrent runs, 10s execution timeout.

## Persistence And API

- Default persistence is SQLite with `database_url = "sqlite://data/smart-home.db"` and `auto_create = true`.
- SQLite schema creation and migrations are handled inside `crates/store-sql/src/sqlite.rs`; there is no separate migration tool or `build.rs`.
- `postgres` is listed in config/types but `crates/api` currently rejects it as unimplemented.
- Useful live inspection endpoints while developing: `/health`, `/ready`, `/diagnostics`, `/adapters`, `/devices`, `/rooms`, `/capabilities`, and WebSocket `/events`.
- History and audit endpoints are implemented; this repo is not current-state-only anymore.
