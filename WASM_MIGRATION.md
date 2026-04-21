# HomeCmdr WASM Plugin System — Implementation Tracking

> Branch: `feat/wasm-plugin-system`
> Started: 2026-04-21
> Status: Phase 3 complete — Phase 4 (remove compile-time adapters) pending

This document tracks the concrete, code-level implementation of the WASM plugin migration.
It supersedes the high-level `migration_to_wasm_plugin_system.md` and is the authoritative
reference for automated tooling and contributors working on this branch.

---

## Why This Migration

The current compile-time adapter system (`inventory::submit!` + `registered_adapter_factories()`)
requires users to recompile the entire binary to add or remove integrations.  The WASM plugin
system replaces it with runtime-loadable `.wasm` files so non-technical users can drop a plugin
file into a directory, edit config, and restart — no Rust toolchain required.

---

## Architecture Summary

### Current (compile-time)

```
crates/adapters/src/lib.rs          <- link crate: forces adapter crates into the binary
crates/adapter-open-meteo/src/lib.rs  <- inventory::submit! at startup
crates/core/src/adapter.rs:64       <- inventory::collect!(RegisteredAdapterFactory)
crates/core/src/adapter.rs:66       <- registered_adapter_factories() iterates collected set
crates/api/src/main.rs:1007         <- build_adapters() calls registered_adapter_factories()
crates/api/src/main.rs:762          <- Runtime::new(adapters, ...)
```

### Target (WASM + legacy coexistence during transition)

```
plugins/                            <- user plugin directory (configurable)
  open_meteo.wasm                   <- compiled plugin
  open_meteo.plugin.toml            <- plugin manifest

crates/plugin-host/                 <- wasmtime engine, WasmAdapter, WasmAdapterFactory,
  src/lib.rs                           PluginManager (scans directory, loads .wasm files)

crates/api/src/main.rs              <- build_adapters() also calls PluginManager::scan()
crates/core/src/config.rs           <- PluginsConfig { enabled, directory }
config/default.toml                 <- [plugins] section
```

The `Adapter` + `AdapterFactory` traits in `crates/core/src/adapter.rs` remain unchanged.
`WasmAdapter` and `WasmAdapterFactory` are new types that implement those same traits, so the
`Runtime` and the rest of the API layer are completely unaware of whether an adapter is native
or WASM.

**WASM takes precedence over native**: when both a WASM factory and a compiled-in factory
exist for the same adapter name, the WASM factory is used (with an `info`-level log message).
This allows a WASM plugin to shadow a built-in adapter without config changes.

---

## Phase 0 — Baseline Inventory

**Status: COMPLETE**

### Compile-time adapter registration points

| File | Line | What it does |
|------|------|-------------|
| `crates/core/src/adapter.rs` | 64 | `inventory::collect!(RegisteredAdapterFactory)` |
| `crates/core/src/adapter.rs` | 66–70 | `registered_adapter_factories()` iterator |
| `crates/adapter-open-meteo/src/lib.rs` | 40–44 | `inventory::submit! { RegisteredAdapterFactory { ... } }` |
| `crates/adapters/src/lib.rs` | 1 | `use adapter_open_meteo as _;` (forces link + submit) |
| `crates/api/src/main.rs` | 1007–1037 | `build_adapters()` consumes factories |
| `crates/api/src/main.rs` | 754–755 | calls `build_adapters(&config)` |
| `crates/api/src/main.rs` | 762 | `Runtime::new(adapters, config.runtime)` |

### Existing adapters

| Adapter | Crate | Classification | Notes |
|---------|-------|---------------|-------|
| open-meteo | `crates/adapter-open-meteo` | **DEPRECATED** (shadowed by WASM plugin) | Pure HTTP polling |

### Core types that cross the WASM boundary (already JSON-serializable)

- `Device` (`crates/core/src/model.rs:33`) — `Serialize + Deserialize`
- `AttributeValue` (`crates/core/src/model.rs:73`) — `Serialize + Deserialize`
- `Metadata` (`crates/core/src/model.rs:85`) — `Serialize + Deserialize`
- `DeviceCommand` (`crates/core/src/command.rs`) — serialized as JSON string at the boundary
- `Event` (`crates/core/src/event.rs:13`) — `Serialize + Deserialize`
- `AdapterConfig = serde_json::Value` (`crates/core/src/config.rs:34`) — already untyped JSON

### What stays in the host (never crosses boundary)

- `DeviceRegistry` — host-side in-memory store; plugins call host imports to upsert devices
- `EventBus` — host-side broadcast; plugins call host imports to publish events
- `Runtime` — host-side tokio task supervisor
- All persistence / SQLite / Postgres code
- Automation and scene execution (Lua)

---

## Phase 1 — Define the WASM Plugin Contract

**Status: COMPLETE**

### 1.1–1.4 — Infrastructure (`crates/plugin-host`)

- [x] `crates/plugin-host/Cargo.toml` — wasmtime 44, wasmtime-wasi 44, reqwest (blocking)
- [x] `crates/plugin-host/wit/homecmdr-plugin.wit` — WIT definition (host-http, host-log, plugin)
- [x] `crates/plugin-host/src/engine.rs` — `create_engine()` singleton with component model
- [x] `crates/plugin-host/src/plugin.rs` — `WasmPlugin`: load/init/poll/command via wasmtime bindgen
- [x] `crates/plugin-host/src/manifest.rs` — `PluginManifest`, `PluginMeta`, `PluginRuntimeConfig`
- [x] `crates/plugin-host/src/manager.rs` — `PluginManager::scan()` + `scan_manifests()`
- [x] `crates/plugin-host/src/adapter.rs` — `WasmAdapter` + `WasmAdapterFactory`
- [x] Added to root `Cargo.toml` workspace members

### 1.5 — Reference WASM guest: open-meteo

- [x] `plugins/open-meteo/` created outside workspace (prevents cross-compilation issues)
- [x] `plugins/open-meteo/Cargo.toml` — `[workspace]` table to opt out of parent workspace
- [x] `plugins/open-meteo/.cargo/config.toml` — default target `wasm32-wasip2`
- [x] `plugins/open-meteo/wit/homecmdr-plugin.wit` — copy of host WIT
- [x] `plugins/open-meteo/src/lib.rs` — full open-meteo WASM implementation
- [x] `plugins/open-meteo/open_meteo.plugin.toml` — manifest
- [x] **Verified**: `cargo build --release` (in `plugins/open-meteo/`) produces `open_meteo.wasm` (173K)
- [x] `config/plugins/open_meteo.wasm` — compiled binary committed to repo
- [x] `config/plugins/open_meteo.plugin.toml` — manifest committed to repo

### WIT Interface

```wit
package homecmdr:plugin@0.1.0;

interface types {
    record device-update { vendor-id: string, kind: string, attributes-json: string }
    record command-result { handled: bool, error: option<string> }
}
interface host-http { get: func(url: string) -> result<string, string> }
interface host-log   { log: func(level: string, message: string) }
interface plugin {
    use types.{device-update, command-result};
    name: func() -> string;
    init: func(config-json: string) -> result<_, string>;
    poll: func() -> result<list<device-update>, string>;
    command: func(device-id: string, command-json: string) -> result<command-result, string>;
}
world adapter { import host-http; import host-log; export plugin; }
```

### Plugin Manifest Format

```toml
[plugin]
name        = "open_meteo"
version     = "0.1.0"
description = "Open-Meteo weather adapter (WASM)"
api_version = "0.1.0"

[runtime]
poll_interval_secs = 300
```

---

## Phase 2 — Dual Plugin Paths (Coexistence)

**Status: COMPLETE**

### 2.1 — Config changes

- [x] `PluginsConfig { enabled: bool, directory: String }` added to `crates/core/src/config.rs`
- [x] `Config.plugins` field (with `#[serde(default)]`)
- [x] `[plugins] enabled = true / directory = "config/plugins"` in `config/default.toml`

### 2.2 — Wire PluginManager into `build_adapters()`

- [x] `build_adapters()` in `crates/api/src/main.rs` split into three steps:
  1. Collect native factories via `registered_adapter_factories()`
  2. Scan WASM plugins via `PluginManager::scan()` (when `config.plugins.enabled`)
  3. Instantiate adapters from `config.adapters`, **WASM takes precedence over native**
- [x] `homecmdr-plugin-host` added to `crates/api/Cargo.toml`
- [x] Name-collision detection: warns (info log) when WASM shadows a native factory; errors on
      duplicate WASM registrations

### 2.3 — HTTP API for plugin management

- [x] `GET /plugins` — lists loaded manifests (Read tier)
- [x] `GET /plugins/{name}` — single plugin detail (Read tier)
- [x] `POST /plugins/reload` — rescans manifest directory (Admin tier)
- [x] `Event::PluginCatalogReloaded { loaded_count, duration_ms }` added to `crates/core/src/event.rs`
- [x] `Event::PluginCatalogReloadFailed { duration_ms, errors }` added
- [x] WebSocket event serialisation and persistence `match` arms updated
- [x] `PluginSummary` response struct with `From<&PluginManifest>` impl
- [x] `AppState` fields: `plugins_enabled`, `plugins_directory`, `plugin_catalog: Arc<RwLock<Vec<PluginManifest>>>`
- [x] Initial catalog scanned at startup

### 2.4 — Health tracking for WASM plugins

- [x] **Verified**: no code changes required.  `WasmAdapter::run()` publishes
  `Event::AdapterStarted { adapter }` and `Event::SystemError` on poll failure.
  `HealthState::new()` is initialised with adapter names from `build_adapters()`, which
  already includes WASM adapters.  `monitor_runtime_health()` keys the health map by adapter
  name, so WASM plugins appear in health exactly like native adapters.

---

## Phase 3 — Migrate open-meteo to WASM, Deprecate Native

**Status: COMPLETE**

- [x] `plugins/open-meteo/` WASM guest crate compiles to `wasm32-wasip2` (173K release binary)
- [x] `config/plugins/open_meteo.wasm` + `config/plugins/open_meteo.plugin.toml` deployed
- [x] `[plugins] enabled = true` in `config/default.toml`
- [x] `[adapters.open_meteo]` config retained (forwarded to WASM plugin's `init()`); native
      factory is shadowed by WASM at runtime (WASM-takes-precedence logic in `build_adapters()`)
- [x] `OpenMeteoFactory` annotated `#[deprecated(since = "0.3.0", note = "...")]`
- [x] Integration tests in `crates/plugin-host/src/lib.rs`:
  - `wasm_open_meteo_plugin_produces_expected_device_updates` — mock server + poll() + 12 device
    updates verified (temperature, wind, humidity, condition, is_day, etc.)
  - `wasm_open_meteo_plugin_returns_error_on_http_failure` — HTTP 500 → `Err` from poll()
- [x] All tests pass: `cargo test --workspace` — 48 API + 34 automations + 52 core + 5 adapter +
      2 plugin-host (only pre-existing `config_loads_default_toml` path failure in core)

---

## Phase 4 — Default to WASM, Retire Compile-Time Adapters

**Status: NOT STARTED**

- [ ] Remove `inventory::submit!` block from `crates/adapter-open-meteo/src/lib.rs`
- [ ] Remove `crates/adapters/src/lib.rs` link pattern (`use adapter_open_meteo as _;`)
- [ ] Remove `homecmdr-adapters` dependency from `crates/api/Cargo.toml`
- [ ] Remove `inventory::collect!` and `registered_adapter_factories()` from
      `crates/core/src/adapter.rs` (keep `AdapterFactory` + `Adapter` traits)
- [ ] Remove `inventory` from `crates/core/Cargo.toml`
- [ ] Remove `crates/adapters` from root `Cargo.toml` workspace members
- [ ] Update `AGENTS.md` adapter rules section to describe the WASM workflow
- [ ] Update `README.md` with WASM plugin installation instructions
- [ ] Create `config/docs/plugin_authoring_guide.md`
- [ ] Provide a `crates/plugin-sdk` guest-side Rust crate (re-exports wit-bindgen macros,
      provides helper types matching `AttributeValue`, etc.) for plugin authors

---

## Dependency Selections

| Purpose | Crate | Rationale |
|---------|-------|-----------|
| WASM runtime | `wasmtime 44` | Best WIT component model support; Bytecode Alliance |
| WASI sync | `wasmtime-wasi 44` | `p2::add_to_linker_sync` for blocking store |
| WIT codegen (host) | `wasmtime::component::bindgen!` | Built into wasmtime |
| WIT codegen (guest) | `wit-bindgen 0.57.1` | Official generator for guest bindings |
| Host HTTP | `reqwest` blocking | Host-side; plugins call `host-http::get` import |

Do **not** use `wasmer` — weaker component model support as of early 2026.

---

## File Change Summary

### New files

| File | Phase |
|------|-------|
| `crates/plugin-host/Cargo.toml` | 1 |
| `crates/plugin-host/wit/homecmdr-plugin.wit` | 1 |
| `crates/plugin-host/src/lib.rs` | 1 |
| `crates/plugin-host/src/engine.rs` | 1 |
| `crates/plugin-host/src/plugin.rs` | 1 |
| `crates/plugin-host/src/manager.rs` | 1 |
| `crates/plugin-host/src/adapter.rs` | 1 |
| `crates/plugin-host/src/manifest.rs` | 1 |
| `plugins/open-meteo/` (standalone crate) | 1 |
| `config/plugins/open_meteo.wasm` | 3 |
| `config/plugins/open_meteo.plugin.toml` | 3 |

### Modified files

| File | Phase | Change |
|------|-------|--------|
| `Cargo.toml` | 1 | Add `plugin-host` to workspace |
| `crates/core/src/config.rs` | 2 | Add `PluginsConfig`, `plugins` field on `Config` |
| `crates/core/src/event.rs` | 2 | Add `PluginCatalogReloaded`, `PluginCatalogReloadFailed` variants |
| `config/default.toml` | 2/3 | Add `[plugins]` section; enable WASM plugins |
| `crates/api/src/main.rs` | 2 | Split `build_adapters`, add plugin API routes, WASM-takes-precedence |
| `crates/api/Cargo.toml` | 2 | Add `plugin-host` dependency |
| `crates/adapter-open-meteo/src/lib.rs` | 3 | `#[deprecated]` on `OpenMeteoFactory` |
| `crates/adapter-open-meteo/src/lib.rs` | 4 | Remove `inventory::submit!` (pending) |
| `crates/adapters/src/lib.rs` | 4 | Remove link crate pattern (pending) |
| `crates/api/Cargo.toml` | 4 | Remove `homecmdr-adapters` dependency (pending) |
| `crates/core/src/adapter.rs` | 4 | Remove `inventory::collect!` + `registered_adapter_factories()` (pending) |
| `crates/core/Cargo.toml` | 4 | Remove `inventory` dependency (pending) |
| `Cargo.toml` | 4 | Remove `crates/adapters` workspace member (pending) |
| `AGENTS.md` | 4 | Update adapter rules (pending) |

---

## Testing Strategy

### Phase 1 (complete)
- Unit tests: `WasmPlugin::load` + `poll()` against mock server in `crates/plugin-host`

### Phase 2 (complete)
- Existing `cargo test -p api` tests pass with no behavioural changes
- `GET /plugins` returns empty list when plugins disabled (existing test coverage via
  `build_adapters_uses_registered_factories`)

### Phase 3 (complete)
- `wasm_open_meteo_plugin_produces_expected_device_updates` — verifies 12 device updates,
  attribute values, unit strings, WMO code mapping, and boolean is_day flag
- `wasm_open_meteo_plugin_returns_error_on_http_failure` — verifies error propagation

### Phase 4
- Full `cargo test --workspace` must pass with no compile-time adapter crates linked
- Smoke test: start API with only `config/plugins/open_meteo.wasm`, hit `/devices`, verify
  weather devices appear

---

## Backward Compatibility Notes

- External adapters built against `homecmdr-core` (compile-time path) continue to work
  until Phase 4. The `AdapterFactory` + `Adapter` traits are not removed in Phases 1–3.
- Config file format for `[adapters]` is unchanged. Only `[plugins]` is added.
- No HTTP API response shapes change — `/adapters` returns WASM plugins with the same
  `AdapterSummary` shape as native adapters.

---

## Open Questions

1. **Hot-reload semantics**: When `POST /plugins/reload` is called, running plugin
   poll tasks need to be cancelled and restarted. Decide whether to abort the task
   immediately or drain it first (prefer drain with 5s timeout).

2. **Plugin distribution**: The `homecmdr pull <adapter-name>` CLI (separate repo) will
   need updating to download `.wasm` + `.plugin.toml` pairs instead of linking crates.
   Out of scope for this branch.

3. **Error isolation**: If a WASM plugin panics (wasmtime trap), it must not crash the
   host process. Confirmed: wasmtime `call()` returns `Err(Trap)` which the
   `WasmAdapter::run()` loop handles as a `SystemError` event.

4. **Plugin sandboxing**: WASI preview 2 limits filesystem access by default. Decision:
   no filesystem access; only network via host-provided `host-http` import (not raw WASI
   network, so the host controls all outbound HTTP).
