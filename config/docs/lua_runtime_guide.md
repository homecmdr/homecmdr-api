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
- `ctx:command_group(group_id, command_table)`
- `ctx:invoke(target, payload_table)`
- `ctx:get_device(device_id)`
- `ctx:list_devices()`
- `ctx:get_room(room_id)`
- `ctx:list_rooms()`
- `ctx:list_room_devices(room_id)`
- `ctx:get_group(group_id)`
- `ctx:list_groups()`
- `ctx:list_group_devices(group_id)`
- `ctx:log(level, message, fields?)`
- `ctx:sleep(secs)`

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

### `ctx:command_group(...)`

`ctx:command_group(group_id, command_table)` fans out one command to every device currently in the group, in membership order.

The `command_table` shape is the same as `ctx:command(...)`.

Example:

```lua
ctx:command_group("bedroom_lamps", {
  capability = "power",
  action = "off",
})
```

Execution behavior:

- command is validated once before any dispatch
- returns a Lua error immediately if the group does not exist
- fans out `Runtime::command_device()` to each member in membership order
- one result is recorded per member device (same statuses: `ok`, `unsupported`, `error`)
- empty groups are a silent no-op (no results recorded)

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
- `ctx:get_group(group_id)` returns a group table or `nil`
- `ctx:list_groups()` returns an array of group tables
- `ctx:list_group_devices(group_id)` returns devices currently in a group

Returned group tables include:

- `id`
- `name`
- `members` (array of device IDs)

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

### `ctx:sleep(...)`

`ctx:sleep(secs)` pauses Lua execution for the given number of seconds without consuming the instruction budget and without blocking the async executor thread.

Parameters:

- `secs` — float or integer seconds; must be in the range `0..=3600`

Example:

```lua
ctx:command_group("bedroom_lamps", { capability = "power", action = "off" })
ctx:sleep(5)
ctx:command("roku_tv:tv", { capability = "power", action = "off" })
```

Notes:

- cancellation via a `restart` mode token only takes effect after the sleep returns; sleep itself is not interruptible
- values outside `0..=3600` produce a Lua error immediately

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
- new scene/automation executions use a fresh Lua state, so script edits are
  visible on subsequent executions

## Manual Reload Workflow

Scenes and automations support manual catalog reload via API endpoints:

- `POST /scenes/reload`
- `POST /automations/reload`

Use reload when you change catalog-level fields such as:

- scene/automation IDs and names
- execution mode (`mode`)
- automation trigger definitions (`trigger`) and conditions (`conditions`)

Notes:

- in-flight scene/automation executions continue on their current Lua context
- new executions after a successful reload use the new catalog
- if reload validation fails, the previous catalog remains active
- `POST /scripts/reload` acknowledges script changes and emits scripts reload events

Reload lifecycle events are published to the runtime event bus and WebSocket
`/events` stream:

- `scene.catalog_reload_started`
- `scene.catalog_reloaded`
- `scene.catalog_reload_failed`
- `automation.catalog_reload_started`
- `automation.catalog_reloaded`
- `automation.catalog_reload_failed`
- `scripts.reload_started`
- `scripts.reloaded`
- `scripts.reload_failed`

## Optional Watch Mode

`config/default.toml` supports optional watch flags:

- `[scenes] watch = false`
- `[automations] watch = false`
- `[scripts] watch = false`

When enabled for a target, saving `.lua` files under that directory triggers a
debounced reload using the same validation and swap pipeline as manual reload
endpoints.

Recommended operations guidance:

- keep `watch = false` in production unless you explicitly need live authoring
- use manual reload endpoints as the default operational flow

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
- `mode`

Notes:
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
- `conditions`
- `state`
- `mode`

Execution model:

- `trigger`: decides when the automation is considered
- `conditions`: optional filters; all conditions must be true at trigger time
- `execute`: Lua actions that run only after the trigger matched and every condition passed

### Trigger Types

Current first-pass trigger types:

- `device_state_change`
- `weather_state`
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

- runs once per local day in `locale.timezone` at the requested hour and minute
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

- schedules from the configured location in `locale.latitude` and `locale.longitude`
- event payload includes `type`, `scheduled_at`, `offset_mins`, and `timezone`

#### `sunset`

Fields:

- `offset_mins` optional, defaults to `0`

Behavior:

- schedules from the configured location in `locale.latitude` and `locale.longitude`
- event payload includes `type`, `scheduled_at`, `offset_mins`, and `timezone`

## Conditions

Conditions are optional filters evaluated after the trigger matches and before `execute(ctx, event)` runs.

All conditions use AND logic.

Supported condition types:

- `device_state`
- `presence`
- `time_window`
- `room_state`
- `sun_position`

#### `device_state`

Fields:

- `device_id` required
- `attribute` required
- `equals` optional
- `above` optional
- `below` optional

#### `presence`

Fields:

- `device_id` required
- `attribute` optional, defaults to `presence`
- `equals` optional, defaults to `true`

#### `time_window`

Fields:

- `start` required in `HH:MM`
- `end` required in `HH:MM`

Behavior:

- evaluated in `locale.timezone`
- supports overnight ranges such as `22:00` to `06:00`

#### `room_state`

Fields:

- `room_id` required
- `min_devices` optional
- `max_devices` optional

#### `sun_position`

Fields:

- `after` optional, `sunrise` or `sunset`
- `before` optional, `sunrise` or `sunset`
- `after_offset_mins` optional
- `before_offset_mins` optional

Behavior:

- uses configured latitude and longitude
- useful for checks like "only after sunset"

Example:

```lua
return {
  id = "movie_mode",
  name = "Movie Mode",
  trigger = {
    type = "device_state_change",
    device_id = "remote:living_room",
    attribute = "custom.remote.button",
    equals = "movie",
  },
  conditions = {
    {
      type = "time_window",
      start = "18:00",
      end = "23:00",
    },
    {
      type = "sun_position",
      after = "sunset",
    },
    {
      type = "device_state",
      device_id = "roku_tv:tv",
      attribute = "power",
      equals = false,
    },
  },
  execute = function(ctx, event)
    ctx:command("elgato_lights:light:0", {
      capability = "power",
      action = "on",
    })
  end,
}
```

## Scheduling Notes

Current scheduling behavior is intentionally simple:

- wall-clock conditions and wall-clock triggers use `locale.timezone`
- cron scheduling remains UTC
- resumable schedules can persist the last completed scheduled fire time per automation
- missed runs while the process is down are not replayed in bulk; the next scheduled fire resumes from persisted schedule state

## Execution Modes

Both scenes and automations support an optional `mode` field that controls what happens when a second invocation arrives while the first is still running.

| Mode | Behaviour |
|---|---|
| `parallel` (default) | Up to `max` (default `automations.runner.default_max_concurrent`) concurrent executions; additional triggers are dropped. |
| `single` | At most one execution at a time; concurrent triggers are dropped. |
| `queued` | One execution at a time; up to `max` (default unbounded) pending triggers are queued and run in order. |
| `restart` | Cancels the running execution and starts a fresh one immediately. |

`parallel` mode is the default for all scenes and automations that do not declare `mode`. The effective `max` defaults to the value of `automations.runner.default_max_concurrent` in `config/default.toml` (shipped as `8`); override it there without changing Rust code.

### Lua syntax

String shorthand (for modes with no meaningful `max`):

```lua
mode = "single"
mode = "restart"
```

Table form with optional `max`:

```lua
mode = { type = "parallel", max = 3 }
mode = { type = "queued", max = 10 }
```

### HTTP status codes (scenes)

| Outcome | Status |
|---|---|
| Completed | `200 OK` with results |
| Dropped | `423 Locked` |
| Queued | `202 Accepted` |

## Sleep

## Sleep

`ctx:sleep(secs)` is available in both scenes and automations. See `ctx:sleep(...)` in the Shared Host API section for full details.

For automation scheduling that does not require arbitrary in-script delays, prefer trigger-level primitives (`debounce_secs`, `duration_secs`, `interval`, `wall_clock`, `cron`, `sunrise`, `sunset`) over `ctx:sleep`. These primitives do not hold a worker slot and survive process restarts.

`ctx:sleep` is appropriate for short sequenced delays within a single execution (for example, turning off one group and then another with a 5-second gap).

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
[locale]
timezone = "Europe/London"

[scenes]
enabled = true
directory = "config/scenes"

[automations]
enabled = true
directory = "config/automations"

[automations.runner]
# Maximum number of concurrently executing automation runs across all automations
# that use the default parallel mode (i.e. no explicit `max` in their `mode` block).
default_max_concurrent = 8
# Hard ceiling on how long any single automation execution may run (seconds).
# Executions that exceed this are cancelled.
backstop_timeout_secs = 3600
```

## Current Limitations

Not implemented yet:

- first-class OR / nested boolean condition groups
- per-condition timezone overrides

## Recommended Usage Split

- scenes: manual, user-invoked flows
- automations: trigger-driven decisions
- adapters: external system integrations
- future scripts: reusable Lua helpers
