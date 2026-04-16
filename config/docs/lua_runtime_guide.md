# Lua Runtime Guide

This document describes the current Lua runtime surface for scenes and automations.

It supersedes the older scene-only implementation plan in `lua_scenes_implementation_plan.md` for day-to-day usage.

## Purpose

Lua assets are the main user-authored orchestration layer.

Current Lua asset roots:

- `config/scenes/`
- `config/automations/`
- `config/scripts/`

## Current Rust Layout

- `crates/lua-host/`
  - shared Lua execution context
  - `ctx:command(...)`
  - `ctx:invoke(...)`
  - Lua <-> `AttributeValue` conversion
- `crates/scenes/`
  - scene catalog and scene execution
- `crates/automations/`
  - automation catalog
  - trigger matching
  - automation runner

This keeps the Lua host logic shared and avoids duplicating scripting behavior across scenes and automations.

## Shared Host API

Both scenes and automations currently receive the same host API through `ctx`.

Available methods:

- `ctx:command(device_id, command_table)`
- `ctx:invoke(target, payload_table)`
- `ctx:get_device(device_id)`
- `ctx:list_devices()`
- `ctx:get_room(room_id)`
- `ctx:list_rooms()`
- `ctx:list_room_devices(room_id)`
- `ctx:log(level, message, fields?)`

### `ctx:command(...)`

`command_table` maps to the canonical Rust `DeviceCommand` shape:

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

Execution behavior:

- command validation uses the existing canonical Rust path
- dispatch goes through `Runtime::command_device()`
- per-command results are recorded for scene execution responses

### `ctx:invoke(...)`

`ctx:invoke(target, payload_table)` dispatches a service-style request to the adapter that owns the target prefix.

Example:

```lua
local result = ctx:invoke("ollama:chat", {
  messages = {
    {
      role = "user",
      content = "Give me a short home summary.",
    },
  },
})

local reply = result.message.content
```

### State Read Helpers

Lua assets can inspect the current registry snapshot without going back through HTTP.

- `ctx:get_device(device_id)` returns a device table or `nil`
- `ctx:list_devices()` returns an array of device tables
- `ctx:get_room(room_id)` returns a room table or `nil`
- `ctx:list_rooms()` returns an array of room tables
- `ctx:list_room_devices(room_id)` returns devices currently assigned to a room

Returned device tables include:

- `id`
- `room_id`
- `kind`
- `attributes`
- `metadata`
- `updated_at`
- `last_seen`

### Logging

Lua assets can emit structured logs through `ctx:log(level, message, fields)`.

Example:

```lua
ctx:log("info", "rain automation fired", {
  automation_id = "rain_reminder",
  device_id = event.device_id,
  recovered = event.recovered == true,
})
```

## Scripts

Scripts are reusable Lua helper modules.

Location:

- `config/scripts/*.lua`

Scripts are not top-level executable assets in the current design. They are loaded from scenes and automations with `require(...)`.

Example helper module:

```lua
-- config/scripts/ollama.lua
local M = {}

function M.vision_bool(ctx, prompt, image_base64)
  local result = ctx:invoke("ollama:vision", {
    prompt = prompt,
    image_base64 = image_base64,
  })

  return result.boolean == true
end

return M
```

Example usage from an automation or scene:

```lua
local ollama = require("ollama")

if ollama.vision_bool(ctx, "Reply only true or false. Are clothes on the line?", snapshot_base64) then
  ctx:command("elgato_lights:light:0", {
    capability = "power",
    action = "on",
  })
end
```

Namespaced modules are also supported:

- `require("lighting.helpers")` -> `config/scripts/lighting/helpers.lua`

Current rules:

- module names must stay within `config/scripts/`
- modules are cached by Lua's `require(...)` within a single Lua execution
- there is no hot reload across runs

## Scenes

Scenes are manual, user-invoked actions.

Location:

- `config/scenes/*.lua`

Current contract:

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

Required fields:

- `id`
- `name`
- `execute`

Optional fields:

- `description`

Notes:

- scenes are loaded at API startup
- duplicate scene IDs fail startup
- scene files must return a Lua table

## Automations

Automations are trigger-driven Lua assets.

Location:

- `config/automations/*.lua`

Current contract:

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
  end,
}
```

Required fields:

- `id`
- `name`
- `trigger`
- `execute`

Optional fields:

- `description`

### Trigger Types

Current first-pass trigger types:

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

#### `device_state_change`

Fields:

- `device_id` required
- `attribute` optional
- `equals` optional
- `above` optional numeric threshold
- `below` optional numeric threshold
- `debounce_secs` optional stable-state delay before execution
- `duration_secs` optional must-remain-matching delay before execution

Example:

```lua
trigger = {
  type = "device_state_change",
  device_id = "weather:outside",
  attribute = "rain",
  equals = true,
}
```

Behavior:

- listens for `Event::DeviceStateChanged`
- matches the target device ID
- if `attribute` is present, requires that attribute to be present in the changed attribute set
- if `equals` is also present, the attribute value must match exactly
- `above` and `below` treat numeric attributes, including measurement-shaped `{ value, unit }` objects, as threshold checks
- threshold triggers only fire when the value crosses into the matching range
- `debounce_secs` waits for the matching attribute value to remain stable before running
- `duration_secs` waits for the matching attribute value to remain true for the full duration before running

#### `weather_state`

Fields:

- `device_id` required
- `attribute` required
- `equals` optional
- `above` optional numeric threshold
- `below` optional numeric threshold
- `debounce_secs` optional
- `duration_secs` optional

Behavior:

- uses the same matching rules as `device_state_change`
- intended for weather or other derived environmental devices where an automation wants a more explicit semantic trigger type

#### `device_room_change`

Fields:

- `device_id` optional
- `room_id` optional

At least one of `device_id` or `room_id` must be provided.

Behavior:

- listens for `Event::DeviceRoomChanged`
- can match a specific device, a specific destination room, or both

#### `room_change`

Fields:

- `room_id` optional

Behavior:

- listens for `Event::RoomAdded`, `Event::RoomUpdated`, and `Event::RoomRemoved`
- if `room_id` is set, only that room matches

#### `adapter_lifecycle`

Fields:

- `adapter` optional
- `event` required

Supported events:

- `started`

Behavior:

- listens for `Event::AdapterStarted`
- can scope to one adapter or all adapters

#### `system_error`

Fields:

- `contains` optional

Behavior:

- listens for `Event::SystemError`
- if `contains` is set, the system error message must include that substring

#### `wall_clock`

Fields:

- `hour` required, `0..23`
- `minute` required, `0..59`

Behavior:

- runs once per UTC day at the requested hour and minute
- event payload includes `type`, `scheduled_at`, `hour`, `minute`, and `timezone`

Example:

```lua
trigger = {
  type = "wall_clock",
  hour = 6,
  minute = 30,
}
```

#### `cron`

Fields:

- `expression` required

Behavior:

- uses a UTC cron schedule
- event payload includes `type`, `scheduled_at`, `expression`, and `timezone`
- current implementation uses a seven-field cron expression with seconds support

Example:

```lua
trigger = {
  type = "cron",
  expression = "0 */5 * * * * *",
}
```

#### `sunrise`

Fields:

- `offset_mins` optional, defaults to `0`

Behavior:

- schedules from the configured location in `adapters.open_meteo.latitude` and `adapters.open_meteo.longitude`
- event payload includes `type`, `scheduled_at`, `offset_mins`, and `timezone`

#### `sunset`

Fields:

- `offset_mins` optional, defaults to `0`

Behavior:

- schedules from the configured location in `adapters.open_meteo.latitude` and `adapters.open_meteo.longitude`
- event payload includes `type`, `scheduled_at`, `offset_mins`, and `timezone`

## Scheduling Notes

Current scheduling behavior is intentionally simple:

- scheduled automations use UTC
- there is no configurable timezone yet
- resumable schedules can persist the last completed scheduled fire time per automation
- missed runs while the process is down are not replayed in bulk; the next scheduled fire resumes from persisted schedule state

## Delay And Wait Policy

Lua scripts do not currently expose a first-class `sleep` or `wait` primitive.

Intentional policy:

- keep long waits out of Lua execution so one script cannot hold a worker slot or hide timeout behavior
- use trigger-level `debounce_secs`, `duration_secs`, `interval`, `wall_clock`, `cron`, `sunrise`, or `sunset` scheduling instead of in-script waiting
- if more complex delayed workflows are needed later, add them as explicit automation runtime features rather than a general Lua blocking primitive

## Persisted Runtime State

Automations can optionally declare runtime state policy with a top-level `state` table.

Supported fields:

- `cooldown_secs`
- `dedupe_window_secs`
- `resumable_schedule`

Example:

```lua
return {
  id = "rain_reminder",
  name = "Rain Reminder",
  state = {
    cooldown_secs = 300,
    dedupe_window_secs = 60,
  },
  trigger = {
    type = "device_state_change",
    device_id = "weather:outside",
    attribute = "rain",
    equals = true,
  },
  execute = function(ctx, event)
    ctx:log("info", "rain reminder fired", { device_id = event.device_id })
  end,
}
```

Behavior:

- cooldown suppresses any re-trigger during the configured window after a successful execution
- dedupe suppresses identical trigger payloads during the configured window
- resumable schedules persist the last completed scheduled fire so restart resumes from the next occurrence

Event object passed to `execute(ctx, event)` includes:

- `event.type`
- `event.device_id`
- `event.attribute` when filtered by attribute
- `event.value` when filtered by attribute
- `event.previous_value` when the triggering attribute had a previous value
- `event.attributes`

#### `interval`

Fields:

- `every_secs` required and must be greater than zero

Example:

```lua
trigger = {
  type = "interval",
  every_secs = 3600,
}
```

Event object passed to `execute(ctx, event)` includes:

- `event.type = "interval"`
- `event.scheduled_at`
- `event.every_secs`

## Ollama Integration Through Lua

Ollama is exposed through `ctx:invoke(...)` and the `adapter-ollama` crate.

Current built-in targets:

- `ollama:generate`
- `ollama:vision`
- `ollama:chat`
- `ollama:embeddings`
- `ollama:tags`
- `ollama:ps`
- `ollama:show`
- `ollama:version`

Examples:

```lua
local result = ctx:invoke("ollama:vision", {
  prompt = "Reply only true or false. Are clothes on the clothesline?",
  image_base64 = snapshot_base64,
})
```

```lua
local result = ctx:invoke("ollama:chat", {
  messages = {
    {
      role = "system",
      content = "Be concise.",
    },
    {
      role = "user",
      content = "Summarize the weather in one sentence.",
    },
  },
})
```

## Configuration

Current config sections:

```toml
[scenes]
enabled = true
directory = "config/scenes"

[automations]
enabled = true
directory = "config/automations"
```

## Current Limitations

Not implemented yet:

- hot reload for scenes or automations
- cron or wall-clock scheduling beyond `interval`
- room-scoped Lua helpers
- direct device read helpers on `ctx`
- explicit Lua logging helpers

## Recommended Usage Split

- scenes: manual, user-invoked flows
- automations: trigger-driven decisions
- adapters: external system integrations
- future scripts: reusable Lua helpers
