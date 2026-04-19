# Smart Home API Reference

This document describes the current HTTP and WebSocket API exposed by `crates/api`.

Base URL by default:

- `http://127.0.0.1:3000`

This comes from `api.bind_address` in `config/default.toml`.

Replace `127.0.0.1:3000` in the examples below if you changed `api.bind_address`.

Cross-origin browser clients are disabled by default. To allow external dashboards or other browser clients, configure explicit origins under `api.cors` in `config/default.toml`:

```toml
[api.cors]
enabled = true
allowed_origins = ["http://127.0.0.1:8080"]
```

Only bare `http` and `https` origins are accepted. Paths, queries, fragments, and wildcard origins are not supported.

## Rate Limiting

Write endpoints (`POST /devices/{id}/command`, `POST /rooms/{id}/command`, `POST /groups/{id}/command`, `POST /scenes/{id}/execute`) are subject to an optional token-bucket rate limiter.

When enabled, requests that exceed the configured rate return:

```
HTTP 429 Too Many Requests
```

Rate limiting is controlled via `[api.rate_limit]` in `config/default.toml`:

```toml
[api.rate_limit]
enabled = false
requests_per_second = 100
burst_size = 20
```

It is disabled by default. MCP tools and agents that issue many rapid commands should either keep rate limiting disabled or set `requests_per_second` and `burst_size` high enough to avoid 429 responses.

## Conventions

- request and response bodies are JSON unless stated otherwise
- device IDs are stable strings like `elgato_lights:light:0` or `roku_tv:tv`
- room IDs are stable strings like `living_room` or `outside`
- command payloads use canonical capability/action pairs

## Health

### `GET /diagnostics/reload_watch`

Returns current reload watch configuration for scenes, automations, and scripts.

Example (uses `adapter-ollama` for `ctx:invoke`; see `crates/adapter-ollama/README.md` for the full `ollama:*` target contract):


Response shape:

```json
{
  "status": "ok",
  "watches": [
    { "target": "scenes", "enabled": true, "directory": "config/scenes" },
    { "target": "automations", "enabled": false, "directory": "config/automations" },
    { "target": "scripts", "enabled": false, "directory": "config/scripts" }
  ]
}
```

### `GET /health`

Returns a simple health response.

Example:

```bash
curl http://127.0.0.1:3000/health
```

Response:

```json
{ "status": "ok" }
```

## Adapters

### `GET /adapters`

Returns the list of configured adapters that were successfully built and enabled.

Example:

```bash
curl http://127.0.0.1:3000/adapters
```

Response shape:

```json
[
  { "name": "open_meteo", "status": "running" },
  { "name": "roku_tv", "status": "running" }
]
```

## Scenes

### `GET /scenes`

Returns all scenes loaded from the configured scene directory.

Example:

```bash
curl http://127.0.0.1:3000/scenes
```

Response shape:

```json
[
  {
    "id": "video",
    "name": "Video",
    "description": "Prepare devices for a video call"
  }
]
```

### `POST /scenes/reload`

Reloads the scene catalog from the configured scenes directory.

Behavior:

- validates all scene files before activation
- atomically swaps the active in-memory catalog on success
- keeps the previous catalog active when any file fails validation

Example:

```bash
curl -X POST http://127.0.0.1:3000/scenes/reload
```

Success response:

```json
{
  "status": "ok",
  "target": "scenes",
  "loaded_count": 12,
  "errors": [],
  "duration_ms": 9
}
```

Failure response:

```json
{
  "status": "error",
  "target": "scenes",
  "loaded_count": 0,
  "errors": [
    {
      "file": "config/scenes/broken.lua",
      "message": "scene file config/scenes/broken.lua is missing function field 'execute': ..."
    }
  ],
  "duration_ms": 3
}
```

Runtime events emitted over WebSocket `/events` during reload:

- `scene.catalog_reload_started`
- `scene.catalog_reloaded`
- `scene.catalog_reload_failed`

### `POST /scenes/{id}/execute`

Executes one loaded scene by ID.

Example:

```bash
curl -X POST http://127.0.0.1:3000/scenes/video/execute
```

Success response:

```json
{
  "status": "ok",
  "results": [
    { "target": "roku_tv:tv", "status": "ok", "message": null },
    { "target": "elgato_lights:light:0", "status": "ok", "message": null }
  ]
}
```

Per-target statuses:

- `ok`
- `unsupported`
- `error`

Error behavior:

- `404` if the scene does not exist
- `400` if the scene fails to execute or emits an invalid canonical command

Scene execution currently runs commands sequentially in the order the Lua scene emits them.

## Devices

## Automations

### `POST /automations/reload`

Reloads the automation catalog from the configured automations directory.

Behavior:

- validates all automation files before activation
- atomically swaps the active in-memory automation catalog on success
- restarts trigger loops using the new catalog on success
- preserves automation enabled/disabled toggles for unchanged automation IDs
- keeps the previous catalog active when any file fails validation

Example:

```bash
curl -X POST http://127.0.0.1:3000/automations/reload
```

Response shape is the same as `POST /scenes/reload` with `target` set to `"automations"`.

Runtime events emitted over WebSocket `/events` during reload:

- `automation.catalog_reload_started`
- `automation.catalog_reloaded`
- `automation.catalog_reload_failed`

## Scripts

### `POST /scripts/reload`

Acknowledges script directory changes for operator workflows and emits scripts reload lifecycle events.

Behavior:

- validates that the scripts directory is readable
- emits scripts reload lifecycle events over `/events`
- does not interrupt in-flight scene or automation executions

Example:

```bash
curl -X POST http://127.0.0.1:3000/scripts/reload
```

Response shape is the same as other reload endpoints with `target` set to `"scripts"`.

Runtime events emitted over WebSocket `/events` during reload:

- `scripts.reload_started`
- `scripts.reloaded`
- `scripts.reload_failed`

## Capabilities

### `GET /capabilities`

Returns the canonical capability catalog used for device validation and command validation.

Response shape:

```json
{
  "capabilities": [
    {
      "domain": "lighting",
      "key": "brightness",
      "schema": { "type": "percentage", "values": [] },
      "read_only": false,
      "actions": ["set", "increase", "decrease"],
      "description": "Brightness level as a percentage (0-100)."
    },
    {
      "domain": "climate",
      "key": "hvac_mode",
      "schema": {
        "type": "enum",
        "values": ["off", "heat", "cool", "auto", "heat_cool", "dry", "fan_only"]
      },
      "read_only": false,
      "actions": ["set"],
      "description": "Thermostat operating mode."
    }
  ],
  "ownership": {
    "canonical_attribute_location": "device.attributes.<capability_key>",
    "custom_attribute_prefix": "custom.",
    "vendor_metadata_field": "metadata.vendor_specific",
    "rules": [
      "Use device.attributes.<capability_key> whenever a canonical capability already exists for the state or command.",
      "Use custom.<adapter>.<field> only for current-state attributes that do not fit an existing canonical capability.",
      "Use metadata.vendor_specific for adapter metadata, opaque upstream identifiers, and descriptive fields that are not canonical device state.",
      "Do not duplicate the same meaning in both a canonical capability and vendor-specific fields."
    ]
  }
}
```

Current domains in the catalog include weather, lighting, sensor, climate, energy, media, and access-control capabilities.

### `GET /devices`

Returns all devices currently present in the in-memory registry.

Optional query params:

- `ids`: repeatable device ID filter; when provided, only matching devices are returned in the requested order

Example:

```bash
curl http://127.0.0.1:3000/devices
```

Filtered example:

```bash
curl "http://127.0.0.1:3000/devices?ids=open_meteo:temperature_outdoor&ids=open_meteo:wind_speed"
```

This includes devices restored from persistence during startup.

If an `ids` value does not match any current device, it is omitted from the response.

### `GET /devices/{id}`

Returns one device by ID.

Example:

```bash
curl http://127.0.0.1:3000/devices/roku_tv:tv
```

404 response example:

```json
{ "error": "device 'nonexistent' not found" }
```

### `POST /devices/{id}/room`

Assigns a device to a room or clears room assignment.

Request body:

```json
{ "room_id": "living_room" }
```

To clear assignment:

```json
{ "room_id": null }
```

Example:

```bash
curl -X POST http://127.0.0.1:3000/devices/roku_tv:tv/room \
  -H 'Content-Type: application/json' \
  -d '{"room_id":"living_room"}'
```

Behavior:

- returns 404 if the device does not exist
- returns 400 if the referenced room does not exist
- returns the updated device record on success

### `POST /devices/{id}/command`

Sends one canonical command to one device.

Example:

```bash
curl -X POST http://127.0.0.1:3000/devices/roku_tv:tv/command \
  -H 'Content-Type: application/json' \
  -d '{"capability":"power","action":"toggle"}'
```

Example with value:

```bash
curl -X POST http://127.0.0.1:3000/devices/elgato_lights:light:0/command \
  -H 'Content-Type: application/json' \
  -d '{"capability":"brightness","action":"set","value":50}'
```

Success response:

```json
{ "status": "ok" }
```

Error behavior:

- 404 if the device does not exist
- 400 if the command is invalid for the capability schema
- 400 if the adapter recognizes the command but rejects or fails it
- 501 if device commands are not implemented for that device

## Rooms

### `GET /rooms`

Returns all rooms.

Example:

```bash
curl http://127.0.0.1:3000/rooms
```

### `POST /rooms`

Creates or updates a room.

Request body:

```json
{
  "id": "living_room",
  "name": "Living Room"
}
```

Example:

```bash
curl -X POST http://127.0.0.1:3000/rooms \
  -H 'Content-Type: application/json' \
  -d '{"id":"outside","name":"Outside"}'
```

Validation:

- room ID must not be empty
- room name must not be empty

### `GET /rooms/{id}`

Returns one room by ID.

### `GET /rooms/{id}/devices`

Returns all devices currently assigned to the room.

Example:

```bash
curl http://127.0.0.1:3000/rooms/living_room/devices
```

### `POST /rooms/{id}/command`

Fans out one canonical command to every device assigned to the room.

Example:

```bash
curl -X POST http://127.0.0.1:3000/rooms/living_room/command \
  -H 'Content-Type: application/json' \
  -d '{"capability":"power","action":"off"}'
```

Response shape:

```json
[
  { "device_id": "roku_tv:tv", "status": "ok", "message": null },
  { "device_id": "open_meteo:wind_speed", "status": "unsupported", "message": "device commands are not implemented" }
]
```

Possible per-device statuses:

- `ok`
- `unsupported`
- `error`

## Groups

Groups are explicit user-defined collections of devices. Membership is static and managed directly with device IDs.

### `GET /groups`

Returns all groups.

Example:

```bash
curl http://127.0.0.1:3000/groups
```

Response shape:

```json
[
  {
    "id": "bedroom_lamps",
    "name": "Bedroom Lamps",
    "members": ["zigbee2mqtt:bedside_left", "zigbee2mqtt:bedside_right"]
  }
]
```

### `POST /groups`

Creates or updates a group.

Request body:

```json
{
  "id": "bedroom_lamps",
  "name": "Bedroom Lamps",
  "members": ["zigbee2mqtt:bedside_left", "zigbee2mqtt:bedside_right"]
}
```

Validation:

- group ID must not be empty
- group name must not be empty
- members must not contain empty IDs
- every member must refer to an existing device

### `GET /groups/{id}`

Returns one group by ID.

### `DELETE /groups/{id}`

Deletes a group.

### `POST /groups/{id}/members`

Replaces group membership with an explicit device list.

Request body:

```json
{
  "members": ["zigbee2mqtt:bedside_left", "zigbee2mqtt:bedside_right"]
}
```

Behavior:

- returns 404 if the group does not exist
- returns 400 if any member device does not exist
- removes duplicate member IDs while preserving first-seen order

### `GET /groups/{id}/devices`

Returns all devices currently in the group.

### `POST /groups/{id}/command`

Fans out one canonical command to every device currently in the group.

Example:

```bash
curl -X POST http://127.0.0.1:3000/groups/bedroom_lamps/command \
  -H 'Content-Type: application/json' \
  -d '{"capability":"power","action":"off"}'
```

Response shape:

```json
[
  { "device_id": "zigbee2mqtt:bedside_left", "status": "ok", "message": null },
  { "device_id": "zigbee2mqtt:bedside_right", "status": "ok", "message": null }
]
```

Possible per-device statuses:

- `ok`
- `unsupported`
- `error`

## WebSocket Events

### `GET /events`

Upgrade to a WebSocket connection to receive live runtime events.

Example:

```bash
wscat -c ws://127.0.0.1:3000/events
```

Current emitted event frame types:

- `device.state_changed`
- `device.removed`
- `device.room_changed`
- `room.added`
- `room.updated`
- `room.removed`
- `group.added`
- `group.updated`
- `group.removed`
- `group.members_changed`
- `adapter.started`
- `system.error`

Example frames:

```json
{ "type": "adapter.started", "adapter": "roku_tv" }
```

```json
{ "type": "device.state_changed", "id": "roku_tv:tv", "state": { "power": "off", "state": "online" } }
```

```json
{ "type": "device.room_changed", "id": "roku_tv:tv", "room_id": "living_room" }
```

```json
{ "type": "system.error", "message": "roku_tv poll failed: ..." }
```

Notes:

- internal `DeviceSeen` refreshes are filtered out of the public event stream
- if the subscriber lags badly, a `system.error` frame is emitted indicating dropped events

## Device Shape

The API returns the `Device` shape from `smart-home-core`.

Important fields:

- `id`
- `room_id`
- `kind`
- `attributes`
- `metadata`
- `updated_at`
- `last_seen`

`updated_at` changes only when meaningful state changes.
`last_seen` changes on successful observation even if state stayed the same.

## Canonical Command Shape

Commands always use:

```json
{
  "capability": "...",
  "action": "...",
  "value": null
}
```

`value` is omitted for actions like `on`, `off`, and `toggle`.
`value` is required for actions like `set`.

Examples:

```json
{ "capability": "power", "action": "on" }
```

```json
{ "capability": "brightness", "action": "set", "value": 42 }
```

```json
{
  "capability": "color_temperature",
  "action": "set",
  "value": { "value": 3000, "unit": "kelvin" }
}
```

## Scene Asset Contract

Scenes are structured Lua modules loaded from the configured scene directory.

Each scene file must return a table with:

- `id`
- `name`
- optional `description`
- `execute = function(ctx) ... end`

Current host API exposed to scenes:

- `ctx:command(device_id, command_table)`
- `ctx:invoke(target, payload_table)`
- `ctx:get_device(device_id)`
- `ctx:list_devices()`
- `ctx:get_room(room_id)`
- `ctx:list_rooms()`
- `ctx:list_room_devices(room_id)`
- `ctx:log(level, message, fields?)`

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

`command_table` maps to the same canonical command shape used by `POST /devices/{id}/command`.

`ctx:invoke(target, payload_table)` dispatches a service-style call to the adapter that owns the target prefix and returns a Lua value.

Invoke targets are adapter-defined. Each adapter that supports `ctx:invoke` documents its available targets, payload fields, and response shapes in its own README. For example, `crates/adapter-ollama/README.md` documents the `ollama:*` targets.

## Automations

Automation assets live in `config/automations/`.

Current trigger types:

- `device_state_change`
- `weather_state`
- `adapter_lifecycle`
- `system_error`
- `wall_clock`
- `cron`
- `sunrise`
- `sunset`
- `interval`

Example:

```lua
return {
  id = "rain_check",
  name = "Rain Check",
  trigger = {
    type = "device_state_change",
    device_id = "weather:outside",
    attribute = "rain",
    equals = true,
  },
  execute = function(ctx, event)
    local result = ctx:invoke("ollama:vision", {
      prompt = "Reply only true or false. Are clothes on the clothesline?",
      image_base64 = "BASE64_IMAGE_HERE",
    })

    if result.boolean == true then
      ctx:command("elgato_lights:light:0", {
        capability = "power",
        action = "on",
      })
    end
  end
}
```

`device_state_change` fields:

- `device_id` required
- `attribute` optional
- `equals` optional
- `above` optional numeric threshold
- `below` optional numeric threshold
- `debounce_secs` optional stable-state delay before execution
- `duration_secs` optional must-remain-matching delay before execution

`weather_state` fields:

- `device_id` required
- `attribute` required
- `equals` optional
- `above` optional numeric threshold
- `below` optional numeric threshold
- `debounce_secs` optional
- `duration_secs` optional

`adapter_lifecycle` fields:

- `adapter` optional
- `event` required, currently `started`

`system_error` fields:

- `contains` optional substring match against the error message

`wall_clock` fields:

- `hour` required, `0..23`
- `minute` required, `0..59`

`cron` fields:

- `expression` required, currently interpreted as a UTC seven-field cron expression

`sunrise` fields:

- `offset_mins` optional, defaults to `0`

`sunset` fields:

- `offset_mins` optional, defaults to `0`

Automations can also include an optional top-level `conditions` list with AND semantics.

Supported condition types:

- `device_state`
- `presence`
- `time_window`
- `room_state`
- `sun_position`

Automations can also include an optional top-level `state` table:

- `cooldown_secs` optional
- `dedupe_window_secs` optional
- `resumable_schedule` optional

`interval` fields:

- `every_secs` required and must be greater than zero

## Agent Usage Notes

For MCP-style tools or agents working against the running system:

- use `GET /scenes` to discover available scene assets
- use `GET /devices` to discover the live canonical device graph
- use `GET /rooms` to discover the room model
- use `POST /scenes/{id}/execute` for scene-driven orchestration
- use `POST /devices/{id}/room` to attach devices to user-defined rooms
- use `POST /devices/{id}/command` for direct device control
- use `POST /rooms/{id}/command` for room-wide control fanout
- use `/events` to react to runtime changes

When in doubt, treat the HTTP API as the external system contract and the adapter crates as the integration contract.
