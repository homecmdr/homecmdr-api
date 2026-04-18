# Execution Modes and Sleep — Implementation Plan

## Branch

`feat/execution-modes-and-sleep`

## Goal

Add `ctx:sleep(secs)` to the Lua host API and introduce per-automation / per-scene
execution modes (`parallel`, `single`, `queued`, `restart`) modelled on Home Assistant's
script run modes.

These two features are coupled: `ctx:sleep` requires the wall-clock execution timeout to
be replaced with an instruction-count limit, which is also the foundation needed for
long-running modes such as `queued` and `restart`.

---

## Design Decisions (already agreed)

| Question | Decision |
|---|---|
| Default mode | `parallel` (max 8) — preserves current behaviour |
| Scope | Both automations **and** scenes |
| Restart cancel mechanism | Best-effort via Lua VM hook cancel token |
| Compute limit | 5,000,000 instructions (replaces 10 s wall-clock) |
| Max field | Optional per-automation / per-scene `max` in Lua |

---

## Part 1 — `ctx:sleep(secs)` + hook redesign

### Files: `crates/lua-host/src/lib.rs`, `crates/lua-host/Cargo.toml`

#### Add `ctx:sleep(secs)`

New method on `LuaExecutionContext`:

```lua
ctx:sleep(5)      -- integer or float seconds; max 3600
```

Rust implementation:

```rust
methods.add_method("sleep", |_, _, secs: f64| {
    if !(0.0..=3600.0).contains(&secs) {
        return Err(mlua::Error::external(
            "sleep: seconds must be between 0 and 3600",
        ));
    }
    block_in_place(|| {
        Handle::current().block_on(tokio::time::sleep(
            std::time::Duration::from_secs_f64(secs),
        ))
    });
    Ok(())
});
```

During the sleep no Lua instructions execute, so the instruction-count hook is
unaffected.

#### Replace wall-clock hook with instruction-count + cancellation hook

Current `install_execution_timeout_hook` uses `Instant::now()`. This counts sleep
time as elapsed and kills automations that call `ctx:sleep`. Replace it:

```rust
pub fn install_execution_hook(
    lua: &Lua,
    max_instructions: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut count = 0u64;
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(10_000),
        move |_lua, _debug| {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(mlua::Error::runtime("execution cancelled"));
            }
            count += 10_000;
            if count >= max_instructions {
                return Err(mlua::Error::runtime("execution compute limit exceeded"));
            }
            Ok(mlua::VmState::Continue)
        },
    );
}
```

Extend `LuaRuntimeOptions`:

```rust
pub struct LuaRuntimeOptions {
    pub scripts_root: Option<PathBuf>,
    pub max_instructions: u64,                        // default 5_000_000
    pub cancel: Option<Arc<AtomicBool>>,              // None = no external cancel
}
```

Make `install_execution_hook` `pub` so `crates/automations` can call it directly
(scenes do not need the cancel token for now, but can pass a no-op one).

#### Tests to add (`crates/lua-host`)

- `sleep_pauses_execution_without_consuming_instruction_budget`
- `compute_limit_fires_on_infinite_loop`
- `cancellation_token_kills_execution`
- `sleep_validates_negative_and_over_limit`

---

## Part 2 — Execution modes (automations)

### File: `crates/automations/src/lib.rs`

#### New type

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionMode {
    Parallel { max: usize },   // default: Parallel { max: 8 }
    Single,
    Queued { max: usize },
    Restart,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        ExecutionMode::Parallel { max: 8 }
    }
}
```

`Automation` gains:

```rust
pub execution_mode: ExecutionMode,
```

#### Lua syntax (parsed in `load_automation_file`)

```lua
-- string shorthand (modes without meaningful max):
mode = "single"
mode = "restart"

-- table form with optional max:
mode = { type = "parallel", max = 3 }
mode = { type = "queued", max = 10 }
```

If `mode` is absent → `Parallel { max: 8 }` (no behaviour change for existing automations).

#### Per-automation runtime concurrency state

```rust
struct PerAutomationConcurrency {
    active: usize,
    cancel: Option<Arc<AtomicBool>>,           // restart: token for running instance
    abort: Option<tokio::task::AbortHandle>,   // restart: allow tokio abort attempt
    queue: std::collections::VecDeque<PendingExecution>,  // queued: waiting triggers
}

struct PendingExecution {
    event: smart_home_core::model::AttributeValue,
    scheduled_for: Option<chrono::DateTime<chrono::Utc>>,
}

type ConcurrencyMap = Arc<tokio::sync::Mutex<HashMap<String, PerAutomationConcurrency>>>;
```

Add `ConcurrencyMap` to `ExecutionControl`.

#### Mode dispatch logic (in `spawn_automation_execution`)

| Mode | On trigger fire | On execution complete |
|---|---|---|
| `Parallel { max }` | `active < max` → spawn; else drop+warn | decrement `active` |
| `Single` | `active == 0` → spawn; else drop+warn | decrement `active` |
| `Queued { max }` | `active == 0` → spawn; `active > 0 && queue.len() < max` → enqueue; else drop+warn | decrement `active`; if queue non-empty pop + spawn next |
| `Restart` | set old cancel token; optionally abort old handle; spawn new with fresh token | decrement `active` |

#### Timeout changes

- **Remove** the 10-second outer `tokio::time::timeout` on the execution join handle.
- Replace with a high backstop: `const AUTOMATION_BACKSTOP_TIMEOUT: Duration = Duration::from_secs(3600);`
- The instruction-count hook is now the sole guard against infinite Lua compute loops.
- Remove `timeout: Duration` from `ExecutionControl`; add `max_instructions: u64`.

#### Tests to add (`crates/automations`)

- `single_mode_drops_concurrent_trigger`
- `parallel_mode_allows_concurrent_up_to_max`
- `parallel_mode_drops_beyond_max`
- `queued_mode_runs_triggers_sequentially`
- `queued_mode_drops_when_queue_full`
- `restart_mode_cancels_running_and_starts_new`
- `sleep_in_automation_completes_without_timeout`
- `default_mode_is_parallel_max_8` (regression — existing behaviour preserved)

---

## Part 3 — Execution modes (scenes)

### File: `crates/scenes/src/lib.rs`

#### New type: `SceneRunner`

Scenes are invoked on demand (HTTP), not from an event loop, so mode enforcement
lives in a thin wrapper:

```rust
pub struct SceneRunner {
    catalog: Arc<SceneCatalog>,
    concurrency: ConcurrencyMap,   // same type as automations
}

impl SceneRunner {
    pub fn new(catalog: SceneCatalog) -> Self { ... }
    pub fn execute(
        &self,
        id: &str,
        runtime: Arc<Runtime>,
    ) -> Result<Option<Vec<SceneExecutionResult>>> { ... }
}
```

`Scene` gains:

```rust
pub execution_mode: ExecutionMode,
```

Lua syntax: identical to automations (`mode = "single"`, `mode = { type = "queued", max = 3 }`).

Default: `Parallel { max: 8 }`.

`SceneRunner::execute` checks the per-scene concurrency state, then either runs
inline (or returns a distinct `SceneDropped` / `SceneQueued` result for the API to
surface as a sensible HTTP status code).

#### API wiring (`crates/api/src/main.rs`)

- Replace direct `SceneCatalog::execute` calls with `SceneRunner::execute`.
- Return `423 Locked` when a scene is dropped due to `single` saturation.
- Return `202 Accepted` when a scene is queued (queued scenes run asynchronously).

#### Tests to add (`crates/scenes`)

- `single_mode_rejects_concurrent_scene_execution`
- `parallel_mode_allows_concurrent_executions`
- `sleep_in_scene_works`

---

## Part 4 — Documentation

### File: `config/docs/lua_runtime_guide.md`

1. Add `ctx:sleep(secs)` to the **Shared Host API** method list.
2. Add a `ctx:sleep(...)` reference section (same style as `ctx:command`).
3. **Replace** the "Delay And Wait Policy" section with a new "Sleep" section
   documenting `ctx:sleep` and the 3600 s per-call cap.
4. Add an **Execution Modes** section:
   - Four modes with semantics table
   - Lua syntax examples for both string shorthand and table form
   - Note that `mode` applies to both scenes and automations
   - Note that `parallel` (max 8) is the default

---

## Final Lua example

After implementation `config/scenes/bedtime.lua` should look like:

```lua
return {
  id = "bedtime",
  name = "Bedtime",
  description = "Turn off bedroom lamps, wait, then the TV",
  mode = "single",
  execute = function(ctx)
    ctx:command_group("bedroom_lamps", { capability = "power", action = "off" })
    ctx:sleep(5)
    ctx:command("roku_tv:tv", { capability = "power", action = "off" })
  end
}
```

---

## Implementation order

1. `crates/lua-host` — `ctx:sleep`, new hook, tests
2. `crates/automations` — `ExecutionMode`, concurrency map, mode dispatch, timeout redesign, tests
3. `crates/scenes` — `SceneRunner`, mode parsing, tests
4. `crates/api` — wire `SceneRunner`
5. `config/docs/lua_runtime_guide.md` — sleep + modes docs
6. `config/scenes/bedtime.lua` — update to use sleep and single mode
7. Run `cargo fmt --all && cargo test --workspace` — all must pass

---

## Acceptance criteria

- `cargo test --workspace` passes with no failures or new warnings.
- `config/scenes/bedtime.lua` uses `ctx:sleep(5)` between lamp group and TV command.
- An infinite Lua loop is killed by the instruction-count limit, not a wall-clock timeout.
- Calling `ctx:sleep(5)` in an automation does **not** trigger the compute limit.
- Declaring `mode = "single"` on an automation drops a second trigger while the first is running.
- Declaring `mode = { type = "queued", max = 3 }` queues up to 3 pending triggers.
- Declaring `mode = "restart"` cancels the running execution when a new trigger fires.
- Existing automations with no `mode` field behave identically to before.
