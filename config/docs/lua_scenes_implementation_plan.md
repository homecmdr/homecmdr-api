# Smart Home Lua Scenes Implementation Plan

> **Goal:** Add file-backed scenes implemented in Lua with a structured module contract.
> **First asset:** `Video` scene that turns off `roku_tv:tv` and turns on `elgato_lights:light:0`.
> **Chosen scene contract:** each scene file returns `{ id, name, execute = function(ctx) ... end }`.

---

## Decisions

- Scenes will be stored as Lua files on disk, not in SQLite.
- Scene files will live under `config/scenes/`.
- Future automation and free-form Lua assets should use sibling directories:
- `config/scenes/`
- `config/automations/`
- `config/scripts/`
- Scenes will be loaded at API startup and kept in memory.
- Scene execution will use the existing canonical command path through `Runtime::command_device()`.
- The first pass will support scene listing and execution only.
- Scene files will be structured Lua modules, not bare top-level scripts.
- The first pass will not implement hot reload, scheduling, triggers, or SQL persistence for scene assets.

---

## Why This Approach

The current codebase already has the right low-level seam for scene execution:

- `crates/core/src/runtime.rs` exposes `Runtime::command_device()`
- `crates/core/src/command.rs` validates canonical commands
- `crates/core/src/registry.rs` provides device and room lookups
- `crates/api/src/main.rs` already exposes thin HTTP handlers over runtime operations

Scenes are therefore an orchestration layer over existing commands, not a new device control system.

Storing scenes as files keeps them:

- easy to version in git
- easy for humans and agents to author
- separate from device and room persistence
- aligned with the repo's stated direction toward automation, scene, and Lua assets

---

## Target Folder Layout

```text
smart-home/
├── config/
│   ├── default.toml
│   ├── scenes/
│   │   └── video.lua
│   ├── automations/
│   ├── scripts/
│   └── docs/
│       └── lua_scenes_implementation_plan.md
├── crates/
│   ├── core/
│   ├── api/
│   ├── scenes/
│   └── ...
└── README.md
```

Notes:

- `config/scenes/` is the initial scene asset root.
- `config/automations/` and `config/scripts/` should exist as the intended long-term layout even if not used yet.
- A dedicated `crates/scenes` crate keeps Lua and scene loading concerns out of `core` and `api`.

---

## Scene File Contract

Each scene file must return a Lua table with this shape:

```lua
return {
  id = "video",
  name = "Video",
  execute = function(ctx)
    ctx:command("roku_tv:tv", {
      capability = "power",
      action = "off",
    })

    ctx:command("elgato_lights:light:0", {
      capability = "power",
      action = "on",
    })
  end
}
```

Required fields:

- `id`: stable scene ID used by the API
- `name`: human-readable display name
- `execute`: function invoked by the runtime host

Optional fields for the first pass:

- `description`: short human-readable summary

Validation rules:

- `id` must not be empty
- `name` must not be empty
- `execute` must be a function
- scene IDs must be unique across loaded files
- the loader should report file path and scene ID clearly on validation failure

---

## Lua Host API For Scenes

The initial scene execution context should be intentionally small.

Expose to `execute(ctx)`:

- `ctx:command(device_id, command_table)`

`command_table` must map cleanly to `DeviceCommand`:

- `capability`
- `action`
- optional `value`

Example:

```lua
ctx:command("elgato_lights:light:0", {
  capability = "brightness",
  action = "set",
  value = 50,
})
```

Execution semantics for `ctx:command(...)`:

- convert Lua table into canonical `DeviceCommand`
- validate it using existing Rust validation
- call `Runtime::command_device()`
- treat `Ok(false)` as unsupported for that device
- record a per-command result for the API response
- continue executing subsequent scene steps unless an explicit future fail-fast policy is added

Recommended first-pass result capture:

- every `ctx:command(...)` call appends one execution result entry
- the scene executor returns the collected result list to the API layer

Deferred host APIs:

- `ctx:get_device(device_id)`
- `ctx:list_devices()`
- `ctx:command_room(room_id, command)`
- delays, waits, conditionals, event subscriptions, and reusable libraries

---

## New Crate: `crates/scenes`

Add a new crate responsible for:

- scene config and model types
- scene file discovery and loading
- Lua runtime integration
- scene execution and result collection

Recommended responsibilities:

### Scene model

Define Rust types for:

- loaded scene metadata
- scene source path
- execution result entries
- scene registry or catalog

Suggested model shape:

```rust
pub struct SceneSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

pub struct SceneExecutionResult {
    pub target: String,
    pub status: String,
    pub message: Option<String>,
}
```

The exact field names may be adjusted, but they should remain API-friendly and easy to serialize.

### Scene loader

Responsibilities:

- read all `*.lua` files from the configured directory
- evaluate each file in Lua
- validate required fields
- reject duplicate scene IDs
- retain enough metadata to execute the scene later

Recommended behavior:

- fail startup if scene loading is enabled and any scene file is invalid
- include file path and scene ID in error messages where possible

### Scene executor

Responsibilities:

- instantiate a Lua environment for each execution or use another isolated strategy
- bind a Rust-backed `ctx` userdata or table
- invoke the scene's `execute(ctx)` function
- collect per-command execution results
- return a final result list to the caller

### Dependency recommendation

Use `mlua` for Lua integration.

Reasons:

- actively used Rust Lua binding
- good serde support path
- suitable for structured host function exposure

---

## Config Changes

Add a top-level scene config section to `crates/core/src/config.rs`.

Suggested config:

```toml
[scenes]
enabled = true
directory = "config/scenes"
```

Recommended Rust shape:

```rust
#[derive(Debug, Deserialize)]
pub struct ScenesConfig {
    pub enabled: bool,
    pub directory: String,
}
```

Add it to `Config` alongside `runtime`, `logging`, `persistence`, and `adapters`.

Validation rules:

- if `scenes.enabled` is true, `scenes.directory` must not be empty
- path validation beyond existence can be deferred to the scene loader

Update `config/default.toml` to include:

```toml
[scenes]
enabled = true
directory = "config/scenes"
```

---

## API Additions

Add two endpoints in `crates/api/src/main.rs`.

### `GET /scenes`

Returns loaded scenes as metadata only.

Suggested response shape:

```json
[
  {
    "id": "video",
    "name": "Video",
    "description": "Prepare the room for a video call"
  }
]
```

### `POST /scenes/{id}/execute`

Executes one scene by ID.

Suggested response shape:

```json
{
  "status": "ok",
  "results": [
    { "target": "roku_tv:tv", "status": "ok", "message": null },
    { "target": "elgato_lights:light:0", "status": "ok", "message": null }
  ]
}
```

Error behavior:

- `404` if the scene ID is not found
- `400` if Lua execution fails due to invalid scene behavior or command shape
- `500` only for unexpected internal failures that should not be user input errors

API composition recommendation:

- replace the current single `Arc<Runtime>` app state with a small shared app state struct
- example members:
- `runtime: Arc<Runtime>`
- `scenes: Arc<SceneCatalog>`

This keeps the API layer thin while allowing scene handlers to access both systems cleanly.

---

## First Scene Asset

Add `config/scenes/video.lua`.

Recommended first version:

```lua
return {
  id = "video",
  name = "Video",
  description = "Prepare devices for a video call",
  execute = function(ctx)
    ctx:command("roku_tv:tv", {
      capability = "power",
      action = "off",
    })

    ctx:command("elgato_lights:light:0", {
      capability = "power",
      action = "on",
    })
  end
}
```

This uses existing supported commands:

- `roku_tv:tv` supports `power:off`
- `elgato_lights:light:0` supports `power:on`

No new device capabilities are required for the first scene.

---

## Execution Semantics

Recommended first-pass behavior:

- execute scene commands sequentially in Lua call order
- collect a result for each command attempt
- do not roll back already-applied commands if a later command fails
- report partial failure clearly through per-command results

This keeps the first implementation simple and predictable.

Future enhancements can add:

- explicit fail-fast mode
- parallel execution for independent steps
- retries
- delays and waits
- reusable helper libraries

---

## Validation And Error Reporting

Scene loading errors should be explicit and actionable.

Examples:

- invalid scene file: missing `id`
- invalid scene file: missing `execute`
- duplicate scene ID `video`
- invalid command table passed to `ctx:command(...)`

Execution errors should preserve enough context to debug quickly:

- scene ID
- file path where relevant
- target device ID if a command failed
- canonical validation error message if command parsing failed

---

## Testing Plan

### Unit tests in `crates/scenes`

Add tests for:

- loading a valid scene file
- rejecting missing `id`
- rejecting missing `name`
- rejecting missing `execute`
- rejecting duplicate scene IDs
- mapping Lua command tables into `DeviceCommand`

### Integration tests in `crates/api`

Reuse the existing test style in `crates/api/src/main.rs`.

Recommended tests:

- `GET /scenes` returns loaded scenes
- `POST /scenes/video/execute` invokes the expected commands against a mock adapter
- unknown scene ID returns `404`

### Command execution test strategy

Use a mock commandable adapter similar to the existing `CommandAdapter` pattern in `crates/api/src/main.rs`.

This keeps scene tests focused on orchestration instead of depending on live Roku or Elgato devices.

---

## Documentation Updates

Update these docs as part of implementation:

- `README.md`
- `config/docs/api_reference.md`
- `config/docs/agent_workflows.md`

Recommended additions:

- new `GET /scenes` and `POST /scenes/{id}/execute` docs
- scene directory conventions
- scene Lua module contract
- mention that scenes are file-backed assets loaded at startup

---

## Phase Plan

### Phase 1 - Config And Asset Layout

Tasks:

- add `ScenesConfig` to `crates/core/src/config.rs`
- update `config/default.toml`
- create `config/scenes/video.lua`
- create empty `config/automations/` and `config/scripts/` directories if desired for structure clarity

Acceptance criteria:

- config supports a scene directory
- the repo contains the first scene asset

### Phase 2 - Scene Crate

Tasks:

- create `crates/scenes`
- add scene model types
- add scene loader
- add Lua integration and execution host

Acceptance criteria:

- valid scene files load successfully
- invalid scene files fail with clear errors

### Phase 3 - API Integration

Tasks:

- load scenes at startup in `crates/api/src/main.rs`
- introduce shared app state for runtime plus scenes
- add `GET /scenes`
- add `POST /scenes/{id}/execute`

Acceptance criteria:

- scenes are listable over HTTP
- `video` scene can be executed through the API

### Phase 4 - Tests And Docs

Tasks:

- add unit tests in `crates/scenes`
- add API tests for scene listing and execution
- update repo docs

Acceptance criteria:

- focused tests pass
- docs reflect the new scene system accurately

---

## Non-Goals For This First Pass

- no hot reload of scene files
- no automation triggers or schedules
- no generic free-form script runner API
- no persistent scene storage in SQLite
- no rollback or transactional command semantics
- no custom Lua package/module system beyond what is required for basic scene loading

---

## Summary

The smallest correct implementation is:

- add a scene directory to config
- add a dedicated `crates/scenes` crate
- load structured Lua scene modules from `config/scenes/`
- expose only listing and execution in the API
- use the existing canonical device command flow under the hood

This delivers the first `Video` scene cleanly while setting up a strong base for future `automations/` and free-form `scripts/` work.
