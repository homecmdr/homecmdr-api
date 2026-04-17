# Zigbee2MQTT Adapter Implementation Plan

> Status: proposed implementation plan for the first Zigbee2MQTT adapter.
> Scope: deliver a working v1 for locally tested Zigbee bulbs and a power plug backed by Zigbee2MQTT over MQTT.

> **Goal:** Add a new `zigbee2mqtt` adapter crate that auto-discovers supported devices from Zigbee2MQTT, maps their state into the Smart Home canonical device model, and translates canonical commands back into Zigbee2MQTT MQTT messages.
> **First supported devices:** two bulbs and one power plug already paired in a local Zigbee2MQTT instance.
> **Chosen v1 boundary:** auto-discover devices, but only implement the capability patterns already proven by the test bulbs and plug.

---

## Decisions

- The adapter will use MQTT directly, not the Zigbee2MQTT frontend or HTTP API.
- The adapter crate will be named `adapter-zigbee2mqtt` and the adapter name will be `zigbee2mqtt`.
- The adapter will auto-discover devices from retained `bridge/devices` payloads.
- The adapter will not be hardcoded to exact model IDs for the test devices.
- The adapter will support only the capability patterns exercised by the current bulbs and plug in v1.
- Unsupported upstream fields will remain in `metadata.vendor_specific` rather than forcing new canonical capabilities.
- Device command success will require observed-state confirmation from MQTT where practical.
- The adapter will be event-driven and reconnect on MQTT failures instead of using a polling loop.
- The adapter must preserve existing `room_id` values on registry refreshes.

---

## Why This Approach

The current runtime already has the right seams for a Zigbee2MQTT adapter:

- `crates/core/src/adapter.rs` defines the adapter contract and factory registration model
- `crates/core/src/command.rs` and `crates/core/src/registry.rs` already enforce canonical command and attribute validation
- `crates/api` dynamically discovers registered adapter factories without adapter-specific startup wiring
- existing adapters already establish the patterns for config ownership, state mapping, command translation, and tests

Zigbee2MQTT itself is MQTT-first. Using MQTT directly gives the adapter:

- push-based state updates instead of polling
- access to retained discovery payloads on startup
- a stable control surface for commands
- less dependence on optional Zigbee2MQTT frontend behavior

The v1 boundary is intentionally narrow. The goal is to get the paired bulbs and plug working reliably first, then expand coverage after the transport and adapter lifecycle are proven.

---

## V1 Scope

### Supported device classes

- bulbs that expose the tested lighting feature set
- smart plugs that expose the tested switch and energy telemetry feature set

### Supported light capabilities

- `power`
- `state`
- `brightness`
- `color_xy`
- `color_temperature`
- `color_mode`

### Supported plug capabilities

- `power`
- `state`
- `power_consumption`
- `energy_total`
- `energy_today`
- `energy_yesterday`
- `energy_month`
- `voltage`
- `current`

### Supported commands in v1

- bulbs:
- `power`: `on`, `off`, `toggle`
- `brightness`: `set`
- `color_xy`: `set`
- `color_temperature`: `set`
- plugs:
- `power`: `on`, `off`, `toggle`

### Non-goals for v1

- sensors, remotes, buttons, occupancy devices, contact sensors, locks, covers, thermostats
- Zigbee groups
- permit-join and other bridge administration commands
- OTA actions
- power-on behavior or overload-protection configuration
- generalized support for every Zigbee2MQTT `exposes` shape
- broader light controls like `color_rgb`, `color_hs`, `color_hex`, `effect`, or `transition`

---

## Zigbee2MQTT MQTT Contract

The adapter should assume a configurable base topic, defaulting to `zigbee2mqtt`.

### Topics to subscribe to

- `<base_topic>/bridge/state`
- `<base_topic>/bridge/devices`
- `<base_topic>/bridge/event`
- `<base_topic>/+/availability`
- `<base_topic>/+`

### Topics to publish to

- `<base_topic>/<friendly_name>/set`

### Why these topics

- `bridge/devices` is the retained source of discovery and supported features
- per-device topics carry current device state
- per-device `availability` carries online/offline transitions when enabled in Zigbee2MQTT
- `bridge/state` indicates adapter-wide online/offline state
- `bridge/event` provides useful lifecycle signals and future expansion points

---

## Adapter Config

The adapter should own its config locally inside the new crate.

Recommended shape:

```toml
[adapters.zigbee2mqtt]
enabled = false
server = "mqtt://127.0.0.1:1883"
base_topic = "zigbee2mqtt"
client_id = "smart-home-zigbee2mqtt"
username = ""
password = ""
keepalive_secs = 30
command_timeout_secs = 5
```

Recommended Rust shape:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct Zigbee2MqttConfig {
    pub enabled: bool,
    pub server: String,
    #[serde(default = "default_base_topic")]
    pub base_topic: String,
    #[serde(default = "default_client_id")]
    pub client_id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_keepalive_secs")]
    pub keepalive_secs: u64,
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u64,
}
```

Validation rules:

- `server` must not be empty
- `base_topic` must not be empty
- `client_id` must not be empty
- `keepalive_secs` must be greater than zero
- `command_timeout_secs` must be greater than zero

---

## Workspace Changes

The new adapter must be linked in the three required places:

- root `Cargo.toml`
- `crates/adapters/Cargo.toml`
- `crates/adapters/src/lib.rs`

Expected new crate layout:

```text
smart-home/
├── config/
│   └── docs/
│       └── zigbee2mqtt_adapter_implementation_plan.md
├── crates/
│   ├── adapter-zigbee2mqtt/
│   │   ├── Cargo.toml
│   │   ├── README.md
│   │   └── src/
│   │       └── lib.rs
│   ├── adapters/
│   └── ...
└── Cargo.toml
```

---

## Discovery Model

The adapter should treat `bridge/devices` as the authoritative discovery input.

For v1 it should:

- parse retained `bridge/devices` JSON on startup and on subsequent updates
- ignore the coordinator entry
- ignore unsupported or disabled devices
- identify supported bulbs and plugs from `definition.exposes`
- use `friendly_name` as the routing key for MQTT topics
- use stable adapter-owned device IDs in the form `zigbee2mqtt:<friendly_name>` unless the `friendly_name` contains path separators that require normalization

### Discovery filtering rules

For bulbs, accept devices that expose at least:

- `state`
- `brightness`

Optionally support when also present:

- `color`
- `color_mode`
- `color_temp`

For plugs, accept devices that expose at least:

- `state`

Optionally support when also present:

- `power`
- `energy`
- `energy_today`
- `energy_yesterday`
- `energy_month`
- `voltage`
- `current`

The adapter should derive canonical capability coverage per device from `exposes`, not from a hardcoded per-model allowlist.

---

## Internal Adapter State

The adapter will need adapter-local state to merge discovery, live state, availability, and command confirmation.

Recommended tracked state:

- known discovered devices keyed by `friendly_name`
- latest raw device payload keyed by `friendly_name`
- latest availability keyed by `friendly_name`
- current bridge online/offline state
- pending command confirmations keyed by device and expected state fragment

This state can remain private to the adapter crate.

---

## Canonical Mapping

### Device IDs

Recommended v1 approach:

- use `zigbee2mqtt:<friendly_name>` when `friendly_name` is simple and API-safe
- if `friendly_name` contains `/`, normalize it for the Smart Home `DeviceId` and preserve the original upstream name in vendor metadata for MQTT routing

### Light mapping

Canonical `DeviceKind`:

- `DeviceKind::Light`

Canonical attribute mapping:

- upstream `state` -> canonical `power`
- per-device availability or inferred online state -> canonical `state`
- upstream `brightness` `0..254` -> canonical `brightness` `0..100`
- upstream `color.x` and `color.y` -> canonical `color_xy`
- upstream `color_temp` -> canonical `color_temperature` with unit `mireds`
- upstream `color_mode` -> canonical `color_mode`

Expected `color_mode` normalization in v1:

- `xy` -> `xy`
- `color_temp` -> `color_temp`

Light vendor metadata should include useful but non-canonical fields such as:

- `friendly_name`
- `ieee_address`
- `model`
- `vendor`
- `description`
- `linkquality`
- `color_temp_startup`

### Plug mapping

Canonical `DeviceKind`:

- `DeviceKind::Switch`

Canonical attribute mapping:

- upstream `state` -> canonical `power`
- per-device availability or inferred online state -> canonical `state`
- upstream numeric `power` -> canonical `power_consumption` with unit `watt`
- upstream `energy` -> canonical `energy_total`
- upstream `energy_today` -> canonical `energy_today`
- upstream `energy_yesterday` -> canonical `energy_yesterday`
- upstream `energy_month` -> canonical `energy_month`
- upstream `voltage` -> canonical `voltage` with unit `volt`
- upstream `current` -> canonical `current` with unit `ampere`

Important distinction:

- Zigbee2MQTT `state` is the source for canonical on/off `power`
- Zigbee2MQTT numeric `power` must map to canonical `power_consumption`, not canonical `power`

Plug vendor metadata should include useful but non-canonical fields such as:

- `friendly_name`
- `ieee_address`
- `model`
- `vendor`
- `description`
- `linkquality`
- `network_indicator`
- `outlet_control_protect`
- `overload_protection`
- `power_on_behavior`
- `update`

---

## Command Translation

### Light commands

- `power:on` -> `{"state":"ON"}`
- `power:off` -> `{"state":"OFF"}`
- `power:toggle` -> `{"state":"TOGGLE"}`
- `brightness:set` -> `{"brightness": <0..254>}` after scaling from canonical percentage
- `color_xy:set` -> `{"color": {"x": <x>, "y": <y>}}`
- `color_temperature:set` -> `{"color_temp": <mireds>}`

`color_temperature` command input rules:

- accept canonical `{"value": N, "unit": "mireds"}` directly
- accept canonical `{"value": N, "unit": "kelvin"}` and convert to mireds before publish

### Plug commands

- `power:on` -> `{"state":"ON"}`
- `power:off` -> `{"state":"OFF"}`
- `power:toggle` -> `{"state":"TOGGLE"}`

### Command completion strategy

Recommended v1 behavior:

- publish the upstream Zigbee2MQTT command payload
- wait for subscribed MQTT state messages to reflect the expected change
- update the registry from the observed new state
- return an error if the command was accepted locally but the expected state was not observed before timeout

This mirrors the confirmation style already used by the Elgato adapter and avoids writing optimistic registry state that may not match device reality.

---

## Run Loop And Reconnect Behavior

This adapter should be event-driven.

`run()` should:

- connect the MQTT client
- subscribe to the required topics
- publish `Event::AdapterStarted`
- process incoming MQTT messages continuously
- emit `Event::SystemError` on meaningful connection or parsing failures
- attempt reconnect on disconnect or recoverable failures
- keep running until the runtime stops it externally

Unlike the polled adapters, this implementation should not be built around `sleep(poll_interval)` loops.

---

## Stale Device Handling

`bridge/devices` should be treated as the authoritative inventory.

When a fresh `bridge/devices` payload arrives, the adapter should:

- rebuild the current supported-device set
- upsert all supported devices that are still present
- remove Smart Home devices previously owned by this adapter that are no longer present in `bridge/devices`

The adapter should not remove devices solely because they are temporarily offline.

---

## Dependency Choice

Recommended MQTT client dependency:

- `rumqttc`

Reasons:

- mature Rust MQTT client
- good Tokio fit
- suitable for long-lived async subscriptions and publishes
- appropriate for a reconnecting event-driven adapter loop

---

## Testing Plan

The adapter should have focused crate-local tests in the same spirit as the existing adapters.

### Test strategy

Recommended first-pass approach:

- hide the concrete MQTT client behind a tiny adapter-local abstraction
- use a fake implementation in tests to drive incoming messages and inspect publishes

This keeps tests deterministic and avoids requiring a real broker inside the test suite.

### Minimum tests

- factory returns `None` when disabled
- config validation rejects invalid required fields
- retained `bridge/devices` payload discovers two bulbs and one plug correctly
- unsupported or disabled Zigbee2MQTT devices are ignored
- plug state maps `state` to canonical `power`
- plug numeric `power` maps to canonical `power_consumption`
- plug `energy`, `energy_today`, `energy_yesterday`, and `energy_month` map correctly
- plug `voltage` and `current` map correctly
- bulb `brightness` maps from `0..254` to `0..100`
- bulb `color_xy` maps correctly from upstream payloads
- bulb `color_temperature` maps to canonical `mireds`
- bulb `color_mode` normalizes correctly
- light `brightness:set` publishes the expected upstream brightness value
- light `color_xy:set` publishes the expected upstream JSON payload
- light `color_temperature:set` publishes mireds and converts kelvin when needed
- plug `power` commands publish the expected upstream JSON payloads
- `room_id` is preserved across refreshes
- `updated_at` remains stable when a new payload does not change canonical state
- stale devices are removed when they disappear from `bridge/devices`

---

## Documentation Updates

Update these files during or immediately after implementation:

- `README.md`
- `config/default.toml`
- `config/docs/adapter_authoring_guide.md` if adapter examples should mention the new MQTT-backed pattern
- `crates/adapter-zigbee2mqtt/README.md`

The new adapter README should document:

- config example
- expected Zigbee2MQTT and broker prerequisites
- supported v1 device classes and capabilities
- command examples through the Smart Home API

---

## Phase Plan

### Phase 1 - Crate And Workspace Wiring

Tasks:

- create `crates/adapter-zigbee2mqtt`
- add crate dependencies
- register the adapter factory
- link the adapter in the root workspace and adapters linking crate

Acceptance criteria:

- the workspace builds with the new crate linked
- `build_adapters()` recognizes `zigbee2mqtt` when configured

### Phase 2 - Config And MQTT Transport

Tasks:

- add config parsing and validation
- add MQTT client abstraction
- implement connect, subscribe, publish, and reconnect behavior

Acceptance criteria:

- adapter builds successfully from config
- adapter can establish MQTT subscriptions and recover from disconnects in tests

### Phase 3 - Discovery And State Mapping

Tasks:

- parse `bridge/devices`
- derive supported capability sets from `exposes`
- track known devices and latest payloads
- map bulbs and plugs into canonical devices
- upsert and remove registry devices appropriately

Acceptance criteria:

- the two bulbs and the plug appear as canonical Smart Home devices
- plug telemetry and bulb color/brightness state map correctly

### Phase 4 - Command Handling

Tasks:

- implement canonical command translation for supported light and plug commands
- add observed-state confirmation with timeout
- refresh registry state from confirmed MQTT updates

Acceptance criteria:

- supported commands publish the correct Zigbee2MQTT payloads
- commands update device state only after confirmation

### Phase 5 - Tests And Docs

Tasks:

- add focused unit and adapter tests
- update config and repo docs
- add adapter README

Acceptance criteria:

- focused tests pass
- docs reflect the new adapter and its v1 limitations accurately

---

## Risks And Edge Cases

- `friendly_name` values may contain `/`, which is valid for MQTT but awkward for Smart Home `DeviceId` and HTTP paths
- Zigbee2MQTT can publish both on/off `state` and numeric `power`; the adapter must map them to different canonical capabilities
- not every bulb exposes every lighting capability, so capability derivation must remain per-device
- availability behavior depends on Zigbee2MQTT configuration; the adapter should degrade gracefully when device availability topics are absent
- extra upstream fields should not leak into canonical state unless they map cleanly to existing capabilities

---

## Summary

The smallest correct Zigbee2MQTT adapter for this repo is:

- a new MQTT-backed `zigbee2mqtt` adapter crate
- auto-discovery from `bridge/devices`
- event-driven device updates from subscribed MQTT topics
- canonical support for the light and plug capability patterns already present in the local test devices
- command translation with observed-state confirmation

This delivers a practical v1 for the two bulbs and power plug without over-designing the adapter before the core MQTT integration and discovery model are proven.
