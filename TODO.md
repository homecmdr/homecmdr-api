# Smart Home TODO

Generated from a docs and code review of the current repository state.

## Current Baseline

These areas are already present and should not be re-planned as greenfield work:

- adapter factory model and linked adapter crates
- HTTP API for health, adapters, devices, rooms, scenes, commands, and WebSocket events
- Lua scenes, Lua automations, and shared Lua script modules
- SQLite-backed current-state persistence for devices and rooms
- room assignment persistence across device refreshes

## P0 Reliability And Correctness

- [x] Make `/health` and a new readiness surface report real system state instead of always returning `ok`.
- [x] Track adapter health separately from "configured and started" so the API can show degraded adapters after repeated poll or command failures.
- [x] Track persistence worker health, lag, last successful flush, and reconciliation state.
- [x] Track automation runner health, including last run result, last error, and dropped-event warnings.
- [x] Make event-triggered automations non-blocking so one slow Lua script or adapter invoke does not stall later automations.
- [x] Add bounded automation concurrency, execution timeouts, and clear failure reporting for stuck or slow Lua runs.
- [x] Add recovery behavior for automation event-bus lag so missed triggers are handled intentionally instead of only logging a warning.
- [x] Add graceful shutdown for persistence so the process can drain or flush pending writes before exit.
- [x] Align adapter retry behavior with one clear policy: either implement exponential backoff in adapters or update the trait/docs to match the fixed-sleep design.
- [x] Add explicit HTTP client timeouts and retry policy for external adapters.
- [x] Decide and enforce a canonical policy for unknown device attributes so registry validation and command validation are consistent.
- [x] Add failure-mode tests for adapter outages, persistence lag, automation lag recovery, slow Lua execution, and shutdown behavior.

## P1 History, Audit, And Time-Based State

- [x] Implement time-based device state history instead of current-state-only persistence.
- [x] Persist attribute-level history with timestamps so state changes can be queried over time.
- [x] Decide retention rules for history data and make them configurable.
- [x] Add history APIs for device timelines, attribute timelines, and time-range queries.
- [x] Add command audit history so device commands can be traced after the fact.
- [x] Add scene execution history with per-step results.
- [x] Add automation execution history with trigger payload, result, duration, and error details.
- [x] Define whether history is always on or controlled by telemetry selection config, and wire the existing telemetry config into real behavior.
- [x] Add migrations/versioning for persistence schema changes instead of relying only on `CREATE TABLE IF NOT EXISTS`.

## P1 Canonical Capability Coverage

- [x] Expand the canonical capability catalog beyond weather and lighting basics.
- [x] Add core sensor capabilities such as humidity, motion, contact, occupancy/presence, pressure, air quality, smoke, leak, and battery.
- [x] Add energy and power capabilities such as current power draw, voltage, current, accumulated energy, and cost-friendly meter shapes.
- [x] Add HVAC and climate capabilities such as target temperature, thermostat mode, fan mode, and heating/cooling state.
- [x] Add media capabilities needed for TVs and speakers such as volume, mute, input/source, playback transport, and app/media selection.
- [x] Add cover and access-control capabilities such as lock, door, garage, blind/shade position, and tilt where needed.
- [ ] Add camera and security-oriented capability shapes only when they can be expressed cleanly and safely.
- [x] Define capability ownership rules so adapters use canonical fields first and vendor metadata only for truly vendor-specific data.
- [x] Add a capability catalog API so clients and agents can discover supported capabilities and actions dynamically.

## P1 API Surface Completion

- [x] Add automation endpoints for listing automations and inspecting their trigger type, status, last run, and last error.
- [x] Add automation control endpoints for enable/disable, dry-run or validate, and manual execution where appropriate.
- [x] Add delete endpoints for rooms and any other missing lifecycle operations that are already supported in the registry.
- [x] Add history endpoints once time-based persistence exists.
- [x] Add a capabilities endpoint that exposes canonical schemas and supported actions.
- [x] Add adapter detail endpoints that expose health, last success, last error, and runtime status.
- [x] Add a metrics or diagnostics endpoint for local observability.
- [x] Decide whether scenes and automations need reload endpoints or whether restart-only loading remains the intended contract.

## P1 Automation And Lua Runtime Expansion

- [ ] Extend automation triggers beyond `device_state_change` and `interval`.
- [ ] Add wall-clock and cron-style scheduling.
- [ ] Add sunrise/sunset and weather/time derived triggers.
- [ ] Add threshold-crossing, debounce, and duration-based triggers.
- [ ] Add room-scoped and multi-device condition triggers.
- [ ] Add triggers for room changes, adapter lifecycle events, and selected system errors where useful.
- [ ] Add optional persisted automation state for cooldowns, dedupe windows, and resumable schedules.
- [ ] Add Lua read helpers for current device state, room membership, and simple registry queries.
- [ ] Add Lua logging helpers so scenes and automations can emit useful structured logs.
- [ ] Decide whether Lua needs a safe delay/wait primitive or whether that should stay outside first-class automation logic.

## P2 Adapter And Device Coverage

- [ ] Extend the Roku adapter beyond power so it can represent a more complete TV/media device.
- [ ] Extend existing adapters to use any newly added canonical capabilities where that improves interoperability.
- [ ] Add the next practical adapters needed for a reliable home graph, likely starting with common sensor or bridge integrations rather than niche devices.
- [ ] Define a priority list of target adapters based on real household value and protocol stability.
- [ ] Ensure every adapter documents its supported capabilities, commands, and known limitations.

## P2 Events And Observability

- [ ] Add normalized events for scene started/completed/failed.
- [ ] Add normalized events for automation triggered/completed/failed.
- [ ] Add normalized events for command issued/completed/failed.
- [ ] Decide whether `DeviceSeen` should remain internal only or gain an opt-in diagnostics stream.
- [ ] Add correlation IDs or similar tracing so a trigger can be followed through command fanout and resulting state changes.

## P2 Testing And Verification

- [ ] Add interval automation tests, including schedule drift and missed-tick behavior.
- [ ] Add automation validation tests for bad trigger shapes, duplicate IDs, script loading failures, and Lua runtime errors.
- [ ] Add persistence restart tests that cover rooms, devices, and later history tables together.
- [ ] Add end-to-end tests for degraded external services and recovery behavior.
- [ ] Add API tests for every new endpoint as it is introduced.
- [ ] Add clippy to the standard verification path if it is meant to be a repository quality gate.

## P2 Documentation Cleanup

- [ ] Update `README.md` so it reflects that scenes, automations, scripts, and SQLite persistence already exist.
- [ ] Update `README.md` workspace layout so it includes active crates such as `automations` and `lua-host`.
- [ ] Update docs that still describe shipped functionality as future work.
- [ ] Add a single architecture/status document that distinguishes shipped features, partial features, and planned features.
- [ ] Keep API docs aligned with the real surface as endpoints are added.

## P3 Nice-To-Have Platform Work

- [ ] Implement PostgreSQL support behind the existing persistence abstraction when SQLite no longer fits the workload.
- [ ] Add hot reload for scenes, automations, and scripts if restart-only workflows become a bottleneck.
- [ ] Add a more complete local operator workflow for backups, schema upgrades, and integrity checks.

## Suggested Priority Order For Planning

- [ ] First plan and deliver truthful health plus automation and persistence reliability.
- [ ] Then plan and deliver time-based history and missing observability.
- [ ] Then expand canonical capabilities and the API surface together so adapters and clients have a stable contract.
- [ ] Then expand automation triggers and adapter/device coverage.
- [ ] Finally tackle backend expansion and workflow improvements.
