# Smart Home API Reference

This document describes the current HTTP and WebSocket API exposed by `crates/api`.

Base URL by default:

- `http://127.0.0.1:3000`

## Conventions

- request and response bodies are JSON unless stated otherwise
- device IDs are stable strings like `elgato_lights:light:0` or `roku_tv:tv`
- room IDs are stable strings like `living_room` or `outside`
- command payloads use canonical capability/action pairs

## Health

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

### `GET /devices`

Returns all devices currently present in the in-memory registry.

Example:

```bash
curl http://127.0.0.1:3000/devices
```

This includes devices restored from persistence during startup.

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
