# Agent Workflows

This document explains how an agentic AI should approach work in this repository.

It is written for future MCP-assisted tooling, but it is useful now even without MCP.

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

### Control a room

Use `POST /rooms/{id}/command`.

This is useful for agent-driven orchestration because it produces a per-device result array.

### Author Lua assets

Use:

- `config/scenes/` for manual user-invoked flows
- `config/automations/` for trigger-driven flows
- `config/docs/lua_runtime_guide.md` for the current contract

Current automation trigger types:

- `device_state_change`
- `weather_state`
- `device_room_change`
- `room_change`
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

## Future MCP Tooling Fit

MCP tooling will likely help agents with:

- scaffolding adapter crates
- reading the live device graph from the API
- listing available capabilities and commands
- validating workspace linkage
- generating config examples
- running focused tests
- creating automation, scene, and Lua assets consistently

The current docs are structured so that those future tools have a clear textual contract to follow.
