# HomeCmdr Plugin Authoring Guide

This guide walks through creating a new WASM plugin for HomeCmdr from scratch.

WASM plugins are the only supported way to add new adapters.  The old
compile-time `inventory::submit!` mechanism is deprecated and will be removed.

The canonical reference implementation is `plugins/open-meteo/`.  Read its
source alongside this guide.

---

## 1. Prerequisites

### Rust toolchain

You need a stable Rust toolchain plus the `wasm32-wasip2` target:

```bash
rustup target add wasm32-wasip2
```

`wasm32-wasip2` is the WASIp2 / component-model target.  It requires Rust
1.82 or later.

### Verify the target is available

```bash
rustup target list --installed | grep wasm32-wasip2
```

---

## 2. Understand the plugin contract

Plugins communicate with the host through a WIT interface defined in:

```
crates/plugin-host/wit/homecmdr-plugin.wit
```

Read that file before writing code.  The full interface is:

```wit
package homecmdr:plugin@0.1.0;

interface types {
    record device-update {
        vendor-id: string,
        kind: string,            // "sensor" | "light" | "switch" | "virtual"
        attributes-json: string, // JSON-encoded map<string, AttributeValue>
    }
    record command-result {
        handled: bool,
        error: option<string>,
    }
}

interface host-http {
    get: func(url: string) -> result<string, string>;
}

interface host-log {
    log: func(level: string, message: string);
}

interface plugin {
    use types.{device-update, command-result};
    name:    func() -> string;
    init:    func(config-json: string) -> result<_, string>;
    poll:    func() -> result<list<device-update>, string>;
    command: func(device-id: string, command-json: string) -> result<command-result, string>;
}

world adapter {
    import host-http;
    import host-log;
    export plugin;
}
```

Host responsibilities:

- calls `init()` once at startup with the JSON from `[adapters.<name>]` in `config/default.toml`
- calls `poll()` on the interval set in `<name>.plugin.toml`
- calls `command()` when a device command is dispatched to this adapter
- provides `host-http.get` and `host-log.log` as imported functions

Plugin responsibilities:

- implement all four exported functions
- perform all HTTP through `host-http.get` (direct sockets are not available)
- keep all mutable state in `thread_local!` (WASM is single-threaded)
- prefix device IDs with the adapter name: `"{name}:{vendor_id}"`

---

## 3. Choose the plugin name

The name must be stable and snake_case.  It is used in:

- `[plugin] name` in the manifest (`<name>.plugin.toml`)
- `[adapters.<name>]` in `config/default.toml`
- the `name()` export function
- device ID prefixes: `"<name>:<vendor_id>"`

Examples: `open_meteo`, `zigbee2mqtt`, `sonos`.

---

## 4. Create the plugin crate

Plugin crates must live **outside** the main workspace.  The build target is
`wasm32-wasip2`, which is incompatible with the host workspace's native target.

A conventional location is `plugins/<name>/`:

```
plugins/
  my-plugin/
    Cargo.toml
    .cargo/config.toml
    wit/
      homecmdr-plugin.wit   ← copy from crates/plugin-host/wit/
    src/
      lib.rs
    my_plugin.plugin.toml
```

### `Cargo.toml`

```toml
[workspace]
# This crate is intentionally outside the main workspace.

[package]
name = "my-plugin-wasm"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen  = "0.57.1"
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
# Optional: SDK helper functions (adjust path relative to this crate)
homecmdr-plugin-sdk = { path = "../../crates/plugin-sdk" }

[profile.release]
opt-level    = "z"   # minimise WASM binary size
lto          = true
codegen-units = 1
```

The empty `[workspace]` table prevents Cargo from walking up to the main
workspace and trying to build this crate as a native host target.

### `.cargo/config.toml`

```toml
[build]
target = "wasm32-wasip2"
```

This makes `cargo build` default to `wasm32-wasip2` without requiring
`--target` on every invocation.

### Copy the WIT file

```bash
cp crates/plugin-host/wit/homecmdr-plugin.wit plugins/my-plugin/wit/
```

The WIT file must be present at build time for `wit_bindgen::generate!`.  Keep
your copy in sync with `crates/plugin-host/wit/homecmdr-plugin.wit` — the host
uses the same interface definition.

---

## 5. Implement the plugin

### Skeleton

```rust
// plugins/my-plugin/src/lib.rs

wit_bindgen::generate!({
    world: "adapter",
    path: "wit/homecmdr-plugin.wit",
});

use exports::homecmdr::plugin::plugin::{CommandResult, DeviceUpdate, Guest};
use homecmdr::plugin::host_http;
use homecmdr::plugin::host_log;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Config — parsed from the JSON passed to init()
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    // ... your fields here ...
}

fn default_true() -> bool { true }

// ---------------------------------------------------------------------------
// Plugin state (thread_local — WASM is single-threaded)
// ---------------------------------------------------------------------------

thread_local! {
    static STATE: std::cell::RefCell<Option<Config>> =
        std::cell::RefCell::new(None);
}

// ---------------------------------------------------------------------------
// Guest implementation
// ---------------------------------------------------------------------------

struct MyPlugin;

impl Guest for MyPlugin {
    fn name() -> String {
        "my_plugin".to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("failed to parse my_plugin config: {e}"))?;

        if !config.enabled {
            host_log::log("info", "my_plugin is disabled by config");
            return Ok(());
        }

        host_log::log("info", "my_plugin initialised");
        STATE.with(|s| { *s.borrow_mut() = Some(config); });
        Ok(())
    }

    fn poll() -> Result<Vec<DeviceUpdate>, String> {
        // Fetch data, build DeviceUpdate list.
        Ok(vec![])
    }

    fn command(_device_id: String, _command_json: String) -> Result<CommandResult, String> {
        // Return handled: false for read-only plugins.
        Ok(CommandResult { handled: false, error: None })
    }
}

export!(MyPlugin);
```

### Building DeviceUpdate

Every update must carry a `vendor_id` (the part after the adapter-name prefix),
a `kind`, and an `attributes_json` string.

Using the SDK helpers:

```rust
use homecmdr_plugin_sdk::{DeviceKind, measurement_attributes};

DeviceUpdate {
    vendor_id: "temperature_outdoor".into(),
    kind: DeviceKind::Sensor.as_str().into(),
    attributes_json: measurement_attributes("temperature_outdoor", 21.5, "celsius"),
}
```

Without the SDK:

```rust
DeviceUpdate {
    vendor_id: "temperature_outdoor".into(),
    kind: "sensor".into(),
    attributes_json: serde_json::json!({
        "temperature_outdoor": { "value": 21.5, "unit": "celsius" }
    }).to_string(),
}
```

### Attribute shapes

Use existing canonical attribute keys from `crates/core/src/capability.rs`
whenever a standard capability already covers the value.

| Attribute key            | Shape                                              | Use for                          |
|--------------------------|---------------------------------------------------|----------------------------------|
| `temperature_outdoor`    | `{"value": f64, "unit": "celsius"}`               | Air temperature                  |
| `humidity`               | `{"value": f64, "unit": "percent"}`               | Relative humidity                |
| `pressure`               | `{"value": f64, "unit": "hPa"}`                   | Barometric pressure              |
| `wind_speed`             | `{"value": f64, "unit": "km/h"}`                  | Wind speed                       |
| `wind_direction`         | `i64` (degrees)                                   | Wind bearing                     |
| `rainfall`               | `{"value": f64, "unit": "mm", "period": "hour"}`  | Precipitation accumulation       |
| `power`                  | `bool`                                            | On/off state                     |
| `brightness`             | `i64` (0–100)                                     | Light brightness %               |
| `color_temperature`      | `i64` (kelvin)                                    | Light colour temperature         |

For values that do not fit any canonical key, use `custom.<adapter>.<field>`.
For opaque vendor metadata, add a nested `metadata.vendor_specific` object.

### Performing HTTP

All HTTP must go through the host-provided `host_http::get` import.  Direct
TCP sockets are not available inside WASM components.

```rust
use homecmdr::plugin::host_http;

let body = host_http::get(&url)
    .map_err(|e| format!("HTTP GET failed: {e}"))?;
let data: MyResponse = serde_json::from_str(&body)
    .map_err(|e| format!("parse error: {e}"))?;
```

Only GET is currently supported.  If you need POST or auth headers, implement
them in a future WIT extension — do not embed an HTTP library in the WASM
binary.

### Logging

```rust
use homecmdr::plugin::host_log;

host_log::log("info",  "my_plugin polling");
host_log::log("error", &format!("poll failed: {err}"));
```

Valid levels: `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`.

With the SDK macro:

```rust
use homecmdr_plugin_sdk::hc_log;

hc_log!(info,  "my_plugin polling");
hc_log!(error, "poll failed: {}", err);
```

### Handling commands

For read-only adapters always return `handled: false`:

```rust
fn command(_device_id: String, _command_json: String) -> Result<CommandResult, String> {
    Ok(CommandResult { handled: false, error: None })
}
```

For commandable devices, deserialise `command_json` (it is a JSON-encoded
`DeviceCommand`), check that `device_id` starts with your adapter's prefix,
and return `handled: true` if the command was applied.

---

## 6. Create the manifest

Every plugin needs a `<name>.plugin.toml` manifest alongside the `.wasm` file
in `config/plugins/`.

```toml
[plugin]
name        = "my_plugin"
version     = "0.1.0"
description = "My HomeCmdr plugin"
api_version = "0.1.0"

[runtime]
poll_interval_secs = 60
```

Field reference:

| Field                       | Required | Description                                              |
|-----------------------------|----------|----------------------------------------------------------|
| `plugin.name`               | yes      | Must match the `[adapters.<name>]` key in the config     |
| `plugin.version`            | yes      | Semver string for your plugin                            |
| `plugin.description`        | no       | Human-readable summary                                   |
| `plugin.api_version`        | no       | WIT API version; defaults to `"0.1.0"`                   |
| `runtime.poll_interval_secs`| no       | Default poll interval; defaults to `300`                 |

---

## 7. Add adapter config

Add a section to `config/default.toml` for your adapter.  The whole section is
passed as JSON to `init()`:

```toml
[adapters.my_plugin]
enabled = true
# ... your fields here ...
```

If the plugin is optional or needs credentials, default `enabled = false` and
document what the user must fill in before enabling.

---

## 8. Enable plugins in config

Ensure the `[plugins]` block is present and enabled (it is by default):

```toml
[plugins]
enabled   = true
directory = "config/plugins"
```

---

## 9. Build and deploy

```bash
# From the plugin crate directory:
cd plugins/my-plugin

# Build the WASM binary.
cargo build --release

# The output is at:
#   target/wasm32-wasip2/release/my_plugin_wasm.wasm
# (crate-name with hyphens replaced by underscores)

# Copy binary and manifest to the plugins directory.
cp target/wasm32-wasip2/release/my_plugin_wasm.wasm \
   ../../config/plugins/my_plugin.wasm
cp my_plugin.plugin.toml \
   ../../config/plugins/my_plugin.plugin.toml
```

The wasm filename must match the manifest stem: `my_plugin.wasm` pairs with
`my_plugin.plugin.toml`.

After deploying, restart the API (or wait for a future hot-reload feature) to
load the new plugin.

---

## 10. Testing

WASM components cannot be tested with `cargo test` on the wasm32 target.  The
recommended strategy is:

### Unit-test pure logic on the host target

Extract all logic that does not touch `host_http` or `host_log` into plain
Rust functions, then test those with normal `#[test]` blocks:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response() {
        let json = r#"{"current": {"temperature_2m": 20.0, ...}}"#;
        let parsed: ForecastResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.current.temperature_2m, 20.0);
    }
}
```

Run these tests against the host target (not wasm32):

```bash
cargo test --target x86_64-unknown-linux-gnu
```

Or, if you're on a machine where the default target is `x86_64`, just:

```bash
cargo test
```

This requires temporarily removing (or making conditional) the `.cargo/config.toml`
`target = "wasm32-wasip2"` line, or passing `--target` explicitly.

### Integration-test through the host plugin system

The main workspace test suite (`cargo test --workspace`) includes
`crates/plugin-host` integration tests that load the compiled
`open_meteo.wasm` end-to-end.  Add similar tests for your plugin in
`crates/plugin-host/src/lib.rs` once the wasm binary is committed.

---

## 11. Using the plugin-sdk

`crates/plugin-sdk` is a helper library that provides:

- `DeviceKind` — enum with `.as_str()` for the four device types
- `measurement_attributes(key, value, unit)` — builds measurement JSON
- `accumulation_attributes(key, value, unit, period)` — builds accumulation JSON
- `integer_attributes(key, value)` — builds integer scalar JSON
- `float_attributes(key, value)` — builds float scalar JSON
- `text_attributes(key, value)` — builds string attribute JSON
- `bool_attributes(key, value)` — builds boolean attribute JSON
- `multi_attributes(pairs)` — builds a multi-key attributes JSON object
- `hc_log!(level, ...)` — logging macro (requires WIT bindings in calling crate)

Add it as a path dependency:

```toml
[dependencies]
homecmdr-plugin-sdk = { path = "../../crates/plugin-sdk" }
```

---

## 12. Review checklist

Before finishing, verify:

- [ ] `[workspace]` table is present in `Cargo.toml` (prevents cargo walking up)
- [ ] `.cargo/config.toml` sets `target = "wasm32-wasip2"`
- [ ] `crate-type = ["cdylib"]`
- [ ] `wit/homecmdr-plugin.wit` is copied from `crates/plugin-host/wit/`
- [ ] `wit_bindgen::generate!` uses `world: "adapter"`
- [ ] `export!(MyPlugin)` is called at crate root
- [ ] `name()` returns the exact snake_case name used in config
- [ ] `init()` parses and validates config, returns `Err(...)` for bad config
- [ ] `poll()` performs HTTP only via `host_http::get`
- [ ] `command()` returns `handled: false` for devices this plugin does not own
- [ ] Device `vendor_id` values do NOT include the adapter name prefix
  (the host adds it; the full device ID becomes `"<name>:<vendor_id>"`)
- [ ] `<name>.plugin.toml` is present with matching `[plugin] name`
- [ ] `[adapters.<name>]` section exists in `config/default.toml`
- [ ] `.wasm` and `.plugin.toml` are copied to `config/plugins/`
- [ ] `[plugins] enabled = true` in `config/default.toml`
- [ ] Pure logic is covered by unit tests on the host target
- [ ] `cargo build --release` produces a valid WASM binary

---

## 13. Common mistakes

1. **Putting the plugin inside the main workspace** — the `[workspace]` table
   will be missing, Cargo will try to build it as a native crate, and
   `wasm32-wasip2`-only features will fail.

2. **Using `reqwest` or `tokio`** — these pull in a Tokio runtime which panics
   when instantiated inside an async context on the host side.  All networking
   must use `host_http::get`.

3. **Forgetting `export!(MyPlugin)`** — the crate will compile but the host
   will fail to instantiate it because no exports are registered.

4. **Mismatch between manifest name and config key** — the manifest
   `[plugin] name` must exactly match the `[adapters.<name>]` key.  A
   mismatch causes the plugin to load but receive an empty config object.

5. **Including the adapter name in `vendor_id`** — the full device ID is
   `"<name>:<vendor_id>"`.  If your `vendor_id` already contains the name,
   commands and device lookups will fail.

6. **Returning `Err(...)` from `command()` for unknown devices** — return
   `Ok(CommandResult { handled: false, error: None })` for devices this
   plugin does not own.  `Err` means the plugin owned the device but execution
   failed.

7. **Calling `init()` state without checking** — if `init()` returned early
   because `enabled = false`, `STATE` will be `None`.  Check it at the start
   of `poll()` and return a suitable error or empty list.

8. **Committing the `target/` directory** — add `target/` to your plugin's
   `.gitignore`.  The wasm binary in `config/plugins/` is committed; the
   build artefacts are not.

---

## 14. Reference implementation

`plugins/open-meteo/` is the canonical example.  Read it to see:

- config struct with `serde::Deserialize` and sensible defaults
- `thread_local!` state pattern
- HTTP via `host_http::get` and JSON parsing
- building multiple `DeviceUpdate` values per poll
- read-only `command()` implementation
- the full set of attribute helper functions (now in `homecmdr-plugin-sdk`)
