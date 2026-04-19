# Agent Workflows

This document explains how an agentic AI should approach work in this repository.

It is useful both directly and when working through the MCP server (`crates/mcp-server`).

## Primary Agent Goals

An agent working in this repo should be able to:

- inspect the live runtime through the API
- understand the current adapter architecture
- add or modify adapter crates safely
- create or manage rooms through the API
- inspect device state and event flow
- prepare for future automation, scene, and Lua scripting work

## Source Of Truth

Use these sources in order:

1. current Rust implementation
2. focused docs in `config/docs/`
3. adapter-specific READMEs
4. root `README.md`

If documentation and code disagree, prefer the code unless the task is explicitly to repair the docs.

## Files To Read First

When an agent starts work, it should usually inspect:

- `README.md`
- `config/default.toml`
- `config/docs/api_reference.md`
- `config/docs/lua_runtime_guide.md`
- `config/docs/adapter_authoring_guide.md`
- `crates/core/src/adapter.rs`
- `crates/core/src/capability.rs`
- `crates/core/src/command.rs`
- `crates/api/src/main.rs`

Then inspect the most relevant adapter crate.

## Common Tasks

### Inspect the live system

Use:

- `GET /health`
- `GET /adapters`
- `GET /devices`
- `GET /rooms`
- WebSocket `/events`

Recommended sequence:

1. confirm the API is healthy
2. inspect enabled adapters
3. list current devices
4. list rooms
5. subscribe to `/events` if observing behavior changes

### Add a room

Use `POST /rooms`.

Example:

```json
{ "id": "living_room", "name": "Living Room" }
```

### Get the state of all devices

Use `GET /devices`.

This is the main endpoint for discovering the canonical runtime state graph.

### Assign devices to rooms

Use `POST /devices/{id}/room`.

Example:

```json
{ "room_id": "living_room" }
```

### Control a device

Use `POST /devices/{id}/command`.

Example:

```json
{ "capability": "power", "action": "toggle" }
```

Write endpoints (`/command`, `/execute`) are subject to optional rate limiting. When the rate limit is enabled in `config/default.toml`, excess requests return `HTTP 429`. See `config/docs/api_reference.md` for details.

### Control a room

Use `POST /rooms/{id}/command`.

This is useful for agent-driven orchestration because it produces a per-device result array.

### Author Lua assets

Use:

- `config/scenes/` for manual user-invoked flows
- `config/automations/` for trigger-driven flows
- `config/docs/lua_runtime_guide.md` for the current contract

Automation runner concurrency and the backstop execution timeout are tunable via `[automations.runner]` in `config/default.toml` — no code changes needed.

Current automation trigger types:

- `device_state_change`
- `weather_state`
- `adapter_lifecycle`
- `system_error`
- `wall_clock`
- `cron`
- `sunrise`
- `sunset`
- `interval`

## Adding A New Adapter

Use this checklist:

1. read `config/docs/adapter_authoring_guide.md`
2. inspect the closest existing adapter crate
3. create `crates/adapter-<name>`
4. add config struct and validation
5. implement `AdapterFactory`
6. register with `inventory`
7. implement polling and optional commands
8. add tests
9. link the crate through root `Cargo.toml`, `crates/adapters/Cargo.toml`, and `crates/adapters/src/lib.rs`
10. add config example to `config/default.toml`
11. add a crate README
12. run focused tests

## Guardrails

Agents should avoid:

- editing `crates/api/src/main.rs` to add adapter-specific wiring
- editing `crates/core/src/config.rs` for adapter-specific config structs
- introducing new capabilities when existing ones are sufficient
- breaking `room_id` persistence during adapter refreshes
- changing runtime-wide contracts casually while implementing one adapter

## Recommended Validation Steps

For adapter work:

```bash
cargo fmt --all
cargo test -p adapter-your-name -p smart-home-adapters
```

For broader safety:

```bash
cargo check --workspace
cargo test --workspace
```

## MCP Tooling

The MCP server (`crates/mcp-server`) is a standalone binary that speaks JSON-RPC 2.0
over stdio (protocol version `2024-11-05`). It is launched by an MCP host as a subprocess
and does not start automatically with the API. The API must be running before the MCP
server is started.

Run it with:

```bash
cargo run -p mcp-server -- --token <BEARER_TOKEN>
# --api-url defaults to http://127.0.0.1:3001
# --workspace defaults to .
# SMART_HOME_TOKEN env var can substitute for --token
```

Available tools:

| Tool | What it does |
|---|---|
| `list_devices` | Proxies `GET /devices` — full canonical device graph |
| `list_rooms` | Proxies `GET /rooms` |
| `list_capabilities` | Proxies `GET /capabilities` — capability schemas |
| `list_adapters` | Proxies `GET /adapters` — adapter runtime status |
| `list_files` | Proxies `GET /files` — Lua assets in scenes/automations/scripts dirs |
| `read_file` | Proxies `GET /files/{path}` — read a Lua asset by relative path |
| `write_file` | Proxies `PUT /files/{path}` — create or overwrite a Lua asset |
| `reload_scenes` | Proxies `POST /scenes/reload` |
| `reload_automations` | Proxies `POST /automations/reload` |
| `reload_scripts` | Proxies `POST /scripts/reload` |
| `scaffold_adapter` | Creates a new adapter crate skeleton on disk with the correct factory boilerplate |
| `run_cargo_check` | Runs `cargo check` as a subprocess (workspace or single package) |
| `run_cargo_test` | Runs `cargo test` as a subprocess (workspace or single package) |

`scaffold_adapter` prints the three manual registration steps required after scaffolding
(root `Cargo.toml`, `crates/adapters/Cargo.toml`, `crates/adapters/src/lib.rs`) but does
not modify those files automatically.
