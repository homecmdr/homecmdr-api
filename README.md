# Smart Home

Smart Home is a Rust workspace for a local home automation runtime built around:

- adapter-driven device ingestion and control
- an in-memory device and room registry
- an HTTP API for inspection and commands
- a WebSocket event stream for live updates
- SQLite-backed current-state and history persistence (PostgreSQL also supported)
- a factory-based adapter model designed to be easy for humans and agentic AI to extend

This project supports agentic workflows: agents can safely add adapters, automations, scenes, and Lua-based scripting, with an MCP server (`crates/mcp-server`) available for tool-assisted operations.

## Read This First

Start here depending on your goal:

- run the system: `config/default.toml`
- use the API: `config/docs/api_reference.md`
- build an external dashboard template: `config/docs/dashboard_template_guide.md`
- write Lua scenes and automations: `config/docs/lua_runtime_guide.md`
- build a new adapter: `config/docs/adapter_authoring_guide.md`
- operate as an agent in this repo: `config/docs/agent_workflows.md`
- understand why the factory model exists: `config/docs/adapter_factory_refactor_plan.md`
- see the original scene implementation plan: `config/docs/lua_scenes_implementation_plan.md`

Adapter-specific docs:

- `crates/adapter-open-meteo/README.md`
- `crates/adapter-elgato-lights/README.md`
- `crates/adapter-ollama/README.md`
- `crates/adapter-roku-tv/README.md`
- `crates/adapter-zigbee2mqtt/README.md`

External reference examples:

- `examples/dashboard-template/README.md`

Lua runtime docs:

- `config/docs/lua_runtime_guide.md`

## Workspace Layout

```text
smart-home/
├── config/
│   ├── default.toml
│   ├── scenes/
│   ├── automations/
│   ├── scripts/
│   └── docs/
├── crates/
│   ├── adapters/
│   ├── adapter-elgato-lights/
│   ├── adapter-ollama/
│   ├── adapter-open-meteo/
│   ├── adapter-roku-tv/
│   ├── adapter-zigbee2mqtt/
│   ├── api/
│   ├── automations/
│   ├── core/
│   ├── lua-host/
│   ├── scenes/
│   ├── store-sql/
│   └── store-postgres/
└── README.md
```

## Core Concepts

### Runtime

- `crates/core` defines the runtime contracts, device model, command model, capability catalog, registry, and event bus.
- `crates/api` starts the runtime and exposes HTTP and WebSocket interfaces.
- `crates/store-sql` is the default SQLite persistence backend. Stores current device and room state, full device/attribute history, command audit log, and scene/automation execution history.
- `crates/store-postgres` is an alternative PostgreSQL backend implementing the same traits. Enable it with `persistence.backend = "postgres"` and a `database_url` in `config/default.toml`.

### Adapters

Adapters are the integration boundary.

Each adapter crate owns:

- its config struct
- config validation
- external protocol/client behavior
- canonical state mapping
- command translation
- factory registration
- tests

The system uses a compile-time factory registry:

- `crates/core/src/adapter.rs` defines `Adapter`, `AdapterFactory`, and `RegisteredAdapterFactory`
- `inventory` is used for distributed factory registration
- `crates/api` discovers factories dynamically at startup
- `crates/adapters` exists only to link adapter crates into the final binary

### Devices And Rooms

The runtime maintains:

- devices in an in-memory registry
- rooms as first-class entities
- device-to-room assignments independent of adapter refreshes

This means adapter polls should preserve room assignment rather than overwrite it.

### Persistence

Two backends are supported, both implementing the same `DeviceStore` and `ApiKeyStore` traits:

**SQLite (default)**

- configured via `persistence.backend = "sqlite"` and `database_url = "sqlite://data/smart-home.db"`
- schema is created and migrated automatically inside `crates/store-sql`; no external tool needed
- startup hydrates the registry from stored device and room rows before adapters begin polling

**PostgreSQL**

- configured via `persistence.backend = "postgres"` and `database_url = "postgres://user:pass@host/db"`
- implemented in `crates/store-postgres`; no TimescaleDB or other extensions required
- a `docker-compose.yml` ships at the workspace root with a `postgres:16-alpine` service for local use

Both backends store:

- latest known devices and rooms
- full device and attribute history
- command audit log
- scene and automation execution history

Rate limiting on write endpoints is configurable via `[api.rate_limit]` in `config/default.toml`.
The server handles SIGTERM and ctrl-c and drains in-flight requests with a 30-second timeout before exit.

## Current Adapters

### Open-Meteo

- crate: `crates/adapter-open-meteo`
- type: poll-only sensor adapter
- publishes weather sensors

### Elgato Lights

- crate: `crates/adapter-elgato-lights`
- type: polled and commandable multi-device adapter
- exposes one logical light device per physical light index

### Roku TV *(bundled example)*

- crate: `crates/adapter-roku-tv`
- type: polled and commandable single-device adapter
- currently exposes power control for one TV using a static IP
- **bundled as an example adapter** — not part of the official supported set

## Configuration

The top-level adapter config model is generic.

Example:

```toml
[adapters.open_meteo]
enabled = true
latitude = 51.5
longitude = -0.1
poll_interval_secs = 90
```

`core` treats each `[adapters.<name>]` section as generic `serde_json::Value` data.
Each adapter crate is responsible for parsing and validating its own config.

Default config lives at:

- `config/default.toml`

Current default config includes:

- `open_meteo` (official)
- `elgato_lights` (official)
- `zigbee2mqtt` (official)
- `roku_tv` (bundled example)
- `ollama` (bundled example)

The default asset layout is:

- `config/scenes/` for structured Lua scenes
- `config/automations/` for event-driven Lua automations
- `config/scripts/` for shared Lua helper modules (`require`-able from scenes and automations)

## Running The API

From the workspace root:

```bash
cargo run -p api -- --config config/default.toml
```

The API binds to `api.bind_address` from `config/default.toml` by default. The shipped default is `127.0.0.1:3000`.

See full endpoint details in:

- `config/docs/api_reference.md`

## Typical API Tasks

Replace `127.0.0.1:3000` in the examples below if you changed `api.bind_address`.

### Get all devices

```bash
curl http://127.0.0.1:3000/devices
```

### Get one device

```bash
curl http://127.0.0.1:3000/devices/roku_tv:tv
```

### List rooms

```bash
curl http://127.0.0.1:3000/rooms
```

### Create a room

```bash
curl -X POST http://127.0.0.1:3000/rooms \
  -H 'Content-Type: application/json' \
  -d '{"id":"outside","name":"Outside"}'
```

### Assign a device to a room

```bash
curl -X POST http://127.0.0.1:3000/devices/roku_tv:tv/room \
  -H 'Content-Type: application/json' \
  -d '{"room_id":"living_room"}'
```

### Send a command to one device

```bash
curl -X POST http://127.0.0.1:3000/devices/roku_tv:tv/command \
  -H 'Content-Type: application/json' \
  -d '{"capability":"power","action":"toggle"}'
```

### Send one command to every device in a room

```bash
curl -X POST http://127.0.0.1:3000/rooms/living_room/command \
  -H 'Content-Type: application/json' \
  -d '{"capability":"power","action":"off"}'
```

### List scenes

```bash
curl http://127.0.0.1:3000/scenes
```

### Execute a scene

```bash
curl -X POST http://127.0.0.1:3000/scenes/video/execute
```

### Subscribe to live events

```bash
wscat -c ws://127.0.0.1:3000/events
```

## Command Model

Commands are canonical across adapters.

Example:

```json
{
  "capability": "brightness",
  "action": "set",
  "value": 50
}
```

Examples already used in the repo:

```json
{ "capability": "power", "action": "on" }
```

```json
{ "capability": "power", "action": "toggle" }
```

```json
{
  "capability": "color_temperature",
  "action": "set",
  "value": { "value": 3000, "unit": "kelvin" }
}
```

Validation happens in `core` before the adapter sees the command.
Adapters are responsible for translating canonical commands into vendor-specific operations.

## Lua Scenes

Scenes are file-backed Lua assets loaded at startup from `config/scenes/`.

Each scene file must return a table with:

- `id`
- `name`
- optional `description`
- `execute = function(ctx) ... end`

Example:

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

Current scene host API:

- `ctx:command(device_id, command_table)`
- `ctx:invoke(target, payload_table)`

Scene execution uses the same canonical command validation and adapter dispatch path as `POST /devices/{id}/command`.
`ctx:invoke(...)` routes service-style calls to the owning adapter and returns a Lua value converted from the adapter response.

Example Ollama invocation:

```lua
return {
  id = "check_clothesline",
  name = "Check Clothesline",
  execute = function(ctx)
    local result = ctx:invoke("ollama:vision", {
      prompt = "Reply only true or false. Are clothes on the clothesline?",
      image_base64 = "BASE64_IMAGE_HERE",
    })

    if result.boolean == true then
      ctx:command("roku_tv:tv", {
        capability = "power",
        action = "off",
      })
    end
  end
}
```

Ollama chat example:

```lua
local result = ctx:invoke("ollama:chat", {
  messages = {
    {
      role = "user",
      content = "Give me a short status summary.",
    },
  },
})

local reply = result.message.content
```

Automations live in `config/automations/` and support these trigger types:

- `device_state_change` — fires when a device attribute changes
- `weather_state` — fires when a weather sensor crosses a threshold
- `adapter_lifecycle` — fires when an adapter starts or stops
- `system_error` — fires on any system error event
- `wall_clock` — fires at a specific time of day
- `cron` — fires on a cron expression schedule
- `sunrise` / `sunset` — fires at computed solar events for the configured location
- `interval` — fires repeatedly on a fixed time interval

Automation example:

```lua
return {
  id = "hourly_summary",
  name = "Hourly Summary",
  trigger = {
    type = "interval",
    every_secs = 3600,
  },
  execute = function(ctx, event)
    local result = ctx:invoke("ollama:chat", {
      messages = {
        {
          role = "user",
          content = "Summarize the current home status in one sentence.",
        },
      },
    })

    local summary = result.message.content
  end
}
```

Lua helper modules can now live in `config/scripts/` and be loaded from scenes or automations with `require(...)`.

Example:

```lua
local ollama = require("ollama")
local has_clothes = ollama.vision_bool(ctx, "Reply only true or false. Are clothes on the line?", snapshot_base64)
```

## Event Model

WebSocket clients connected to `/events` receive normalized runtime events such as:

- `device.state_changed`
- `device.removed`
- `device.room_changed`
- `room.added`
- `room.updated`
- `room.removed`
- `adapter.started`
- `system.error`

`DeviceSeen` events are internal and intentionally filtered from the public WebSocket stream.

## Documentation Strategy

This repository now documents two audiences explicitly:

### Human readers

Humans should be able to:

- understand the runtime model
- run the API
- inspect devices and rooms
- add integrations safely

### Agentic AI using MCP tooling

Agents should be able to:

- discover the current architecture quickly
- identify the right files to edit
- avoid changing core files unnecessarily
- use the API safely and predictably
- scaffold adapters and verify them with focused tests

The docs under `config/docs/` are written with both audiences in mind.

## Development Commands

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
```

## Near-Term Direction

This project is intended to grow in these areas:

- adapter creation through agent-friendly workflows
- automations
- scenes
- scripting through Lua

The current adapter factory model is the foundation for that work.
