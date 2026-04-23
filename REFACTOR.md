# HomeCmdr API — Structural Refactoring Tracker

Tracking progress on splitting large monolithic files into focused modules.
See code review findings for full rationale.

---

## Progress

| # | Crate / File | Lines | Severity | Status |
|---|---|---|---|---|
| 1 | `crates/api/src/main.rs` | 7,947 | Critical | ✅ Complete |
| 2 | `crates/automations/src/lib.rs` | 5,694 | Critical | ✅ Complete |
| 3 | `crates/store-sql` + `store-postgres` history filter dedup | ~400 dup | High | ✅ Complete |
| 4 | `crates/lua-host/src/lib.rs` | 1,137 | High | ⬜ Pending |
| 5 | `crates/scenes/src/lib.rs` | 983 | Moderate | ⬜ Pending |
| 6 | `crates/core/src/registry.rs` — move validators | 706 | Moderate | ⬜ Pending |
| 7 | `crates/core/src/config.rs` — config submodule | 460 | Low/Watch | ⬜ Pending |

---

## Task 1: `crates/api/src/main.rs`

**Goal:** Break the 7,947-line single-file binary into a module tree.

### Proposed module layout

```
crates/api/src/
├── main.rs               # entry point only: parse args, call startup::run()
├── state.rs              # AppState, ReloadController, HealthState, HistorySettings, BuiltAdapters
├── dto.rs                # All *Request / *Response structs
├── middleware.rs         # check_auth(), TokenBucket, SharedRateLimit, CORS helper
├── router.rs             # app() builder, route tier definitions
├── startup.rs            # build_adapters(), create_device_store(), run()
├── workers.rs            # run_persistence_worker(), monitor_runtime_health(), run_reload_target()
├── reload.rs             # reload_*_internal(), spawn_reload_watchers_if_enabled()
├── helpers.rs            # reconcile_device_store(), resolve_lua_path(), sha256_hex(), persist_*
├── handlers/
│   ├── mod.rs
│   ├── health.rs         # GET /health, GET /ready
│   ├── adapters.rs       # GET /adapters, GET /capabilities
│   ├── devices.rs        # CRUD /devices, /rooms, /groups
│   ├── scenes.rs         # /scenes
│   ├── automations.rs    # /automations
│   ├── history.rs        # /history, /audit
│   ├── plugins.rs        # /plugins, /files
│   └── events.rs         # WebSocket /events, IPC ingest
└── tests/
    └── mod.rs            # Integration test suite, make_state() factory
```

### Checklist

- [x] Read full `main.rs` to map all symbols
- [x] Create `state.rs`
- [x] Create `dto.rs`
- [x] Create `middleware.rs`
- [x] Create `helpers.rs`
- [x] Create `workers.rs`
- [x] Create `reload.rs`
- [x] Create `handlers/` tree
- [x] Create `router.rs`
- [x] Create `startup.rs`
- [x] Slim down `main.rs` to entry point only
- [x] Extract `tests/mod.rs`
- [x] `cargo check --workspace` passes
- [x] `cargo test -p api` passes

---

## Task 2: `crates/automations/src/lib.rs`

### Proposed module layout

```
crates/automations/src/
├── lib.rs                # public re-exports only
├── types.rs              # Automation, Trigger, Condition, AutomationSummary, etc.
├── catalog.rs            # impl AutomationCatalog
├── runner.rs             # impl AutomationRunner, impl AutomationController
├── concurrency.rs        # PerAutomationConcurrency, ConcurrencyMap, SpawnDecision
├── state.rs              # AutomationStateStore, should_skip_trigger(), persist_runtime_state()
├── conditions.rs         # evaluate_condition(), first_failed_condition()
├── schedule.rs           # next_schedule_time(), solar helpers
├── events.rs             # event builder functions
└── triggers/
    ├── mod.rs            # parse_trigger(), dispatch
    ├── device.rs         # DeviceStateChange parsing + run_event_trigger_loop()
    ├── scheduled.rs      # WallClock/Cron/Sunrise/Sunset + run_scheduled_trigger_loop()
    └── interval.rs       # Interval + run_interval_trigger_loop()
```

### Checklist

- [x] Create `types.rs`
- [x] Create `concurrency.rs`
- [x] Create `schedule.rs`
- [x] Create `events.rs`
- [x] Create `conditions.rs`
- [x] Create `state.rs`
- [x] Create `catalog.rs`
- [x] Create `runner.rs`
- [x] Create `triggers/mod.rs`
- [x] Create `triggers/device.rs`
- [x] Create `triggers/scheduled.rs`
- [x] Create `triggers/interval.rs`
- [x] Slim down `lib.rs` to re-exports + module declarations
- [x] Extract `tests.rs`
- [x] `cargo check --workspace` passes
- [x] `cargo test -p homecmdr-automations` passes (34/34)

---

## Task 3: Store history filter deduplication

Extract `HistorySelection` + `should_record_*` / `selection_allows_*` (~200 lines duplicated
between `sqlite.rs` and `postgres.rs`) into a shared location.

### Resolution

Extracted to `crates/core/src/history_filter.rs`. Both store crates now import
`HistorySelection` and the filter free-functions from `homecmdr_core::history_filter`.
`HistorySelection` is re-exported from each store crate's `lib.rs` for backward compatibility.

### Checklist

- [x] Create `crates/core/src/history_filter.rs` — `HistorySelection`, `should_record_*`, `selection_allows_*`, helpers
- [x] Add `pub mod history_filter` to `crates/core/src/lib.rs`
- [x] Remove `HistorySelection` and duplicated methods from `store-sql/src/sqlite.rs`
- [x] Remove `HistorySelection` and duplicated methods from `store-postgres/src/postgres.rs`
- [x] Update `store-sql/src/lib.rs` to re-export `HistorySelection` from `homecmdr_core`
- [x] Update `store-postgres/src/lib.rs` to re-export `HistorySelection` from `homecmdr_core`
- [x] `cargo check --workspace` passes
- [x] `cargo test -p store-sql` passes (16/16)

---

## Task 4: `crates/lua-host/src/lib.rs`

```
crates/lua-host/src/
├── lib.rs        # public re-exports
├── context.rs    # LuaExecutionContext UserData impl
├── convert.rs    # lua_value_to_attribute(), attribute_to_lua_value(), etc.
├── loader.rs     # ScriptLoader, require() searcher, path-traversal protection
└── runtime.rs    # ExecutionMode, prepare_lua(), install_execution_hook(), evaluate_module()
```

---

## Task 5: `crates/scenes/src/lib.rs`

```
crates/scenes/src/
├── lib.rs        # public re-exports
├── types.rs      # Scene, SceneSummary, SceneRunOutcome
├── catalog.rs    # impl SceneCatalog
├── runner.rs     # impl SceneRunner, concurrency tracking
└── loader.rs     # load_scene_file(), execute_scene_inline(), evaluate_scene_module()
```

---

## Task 6: `crates/core/src/registry.rs` — validator extraction

Move the ~250 lines of pure `validate_*` functions into `crates/core/src/capability.rs`
or a new `crates/core/src/validation.rs`. `registry.rs` calls into that module.

---

## Task 7: `crates/core/src/config.rs` — submodule (low priority)

Only if the file grows significantly. Target layout:

```
crates/core/src/config/
├── mod.rs          # Config, load_from_file(), validate()
├── api.rs          # ApiConfig, RateLimitConfig, ApiCorsConfig
├── persistence.rs  # PersistenceConfig, HistoryConfig
├── runtime.rs      # ScenesConfig, AutomationsConfig, ScriptsConfig
└── system.rs       # LocaleConfig, TelemetryConfig, AuthConfig, PluginsConfig
```
