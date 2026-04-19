# Smart Home Adapter Authoring Guide

This guide is for an agentic AI or engineer creating a new adapter crate in this workspace.

It reflects the current codebase, not an aspirational plugin system.

## Purpose

Use this guide when adding a new adapter such as:

- `zigbee2mqtt`
- `home_assistant`
- `denon_avr`
- `sonos`
- `tasmota`

The goal is to create a new adapter with the smallest correct change set while preserving the existing factory-based architecture.

## Current Architecture

The adapter system is compile-time linked and factory-driven.

- `crates/core` defines the shared runtime contracts.
- `crates/api` discovers adapter factories and builds adapters from generic config.
- `crates/adapters` is a link crate that pulls adapter crates into the final binary.
- each adapter crate owns its own config, validation, polling, command translation, and tests.

Relevant files:

- `crates/core/src/adapter.rs`
- `crates/core/src/config.rs`
- `crates/core/src/command.rs`
- `crates/core/src/capability.rs`
- `crates/core/src/model.rs`
- `crates/core/src/registry.rs`
- `crates/api/src/main.rs`
- `crates/adapters/src/lib.rs`

## What An Adapter Owns

Each adapter crate should own:

- its config struct
- config validation
- factory registration with `inventory`
- external API client logic
- canonical device mapping
- command translation from canonical commands to vendor-specific operations
- adapter-local tests

Each adapter crate should not require edits to:

- `crates/core/src/config.rs`
- `crates/api/src/main.rs`

You will still need to update Cargo linkage:

- workspace `Cargo.toml`
- `crates/adapters/Cargo.toml`
- `crates/adapters/src/lib.rs`

## Required Runtime Contracts

Read these first:

- `crates/core/src/adapter.rs`
- `crates/core/src/model.rs`
- `crates/core/src/registry.rs`
- `crates/core/src/command.rs`
- `crates/core/src/capability.rs`

Important rules:

1. All device IDs must be namespaced as `"{adapter_name}:{vendor_id}"`.
2. `run()` must publish `Event::AdapterStarted` before doing work.
3. `run()` must not exit on transient failures.
4. Polling failures should normally be logged and emitted as `Event::SystemError`.
5. Commands return:
   - `Ok(true)` when applied
   - `Ok(false)` when the device or command is not supported by that adapter
   - `Err(...)` when the adapter recognized the command but could not apply it
6. Registry updates must preserve prior `room_id`.
7. When state is unchanged, preserve `updated_at` and refresh only `last_seen`.

## Current Capability Model

Commands are validated in `core` before they reach your adapter.

This means:

- you can only use capabilities/actions already defined in `crates/core/src/capability.rs`
- if your integration can be expressed using existing capabilities, do not add new ones
- if it cannot, a core capability change is a separate architectural decision and should be treated explicitly

Current adapters demonstrate the intended pattern:

- `adapter-open-meteo` is read-only and publishes sensors
- `adapter-elgato-lights` is polled state plus commandable lights
- `adapter-roku-tv` is a single commandable switch-like device using existing `power`

## Existing Adapter Patterns

### Pattern 1: Poll-only sensor adapter

Use `adapter-open-meteo` as the reference.

Characteristics:

- no `command()` override needed
- maps external data into one or more `DeviceKind::Sensor` devices
- each poll upserts canonical state
- room assignment is preserved via `previous.and_then(|device| device.room_id.clone())`

Reference file:

- `crates/adapter-open-meteo/src/lib.rs`

### Pattern 2: Poll plus commands for multiple devices

Use `adapter-elgato-lights` as the reference.

Characteristics:

- one adapter owns multiple vendor devices
- device IDs encode vendor identity such as `elgato_lights:light:0`
- `command()` parses the device ID and returns `Ok(false)` for devices it does not own
- commands are translated from canonical shapes into vendor API payloads
- post-command state is confirmed before updating the registry
- stale devices are removed if they disappear upstream

Reference file:

- `crates/adapter-elgato-lights/src/lib.rs`

### Pattern 3: Poll plus commands for one logical device

Use `adapter-roku-tv` as the reference.

Characteristics:

- one adapter exposes one logical device
- `command()` checks one stable device ID
- command translation can be simple keypress mapping
- test-only base URL overrides are acceptable when the real protocol uses fixed ports

Reference file:

- `crates/adapter-roku-tv/src/lib.rs`

## Step-By-Step Procedure

### 1. Choose the adapter name

The adapter name must be stable and snake_case.

Examples:

- `open_meteo`
- `elgato_lights`
- `roku_tv`

This name is used in:

- the config section key: `[adapters.<name>]`
- `AdapterFactory::name()`
- `Adapter::name()`
- device ID prefixes
- error messages

Keep the crate name aligned:

- `crates/adapter-<name>`

### 2. Create the crate

Create a new crate:

- `crates/adapter-<name>/Cargo.toml`
- `crates/adapter-<name>/src/lib.rs`

Use existing adapter crates as your dependency template.

Typical dependencies:

- `anyhow`
- `async-trait`
- `chrono`
- `inventory`
- `reqwest` if HTTP is needed
- `serde`
- `serde_json`
- `smart-home-core`
- `tokio`
- `tracing`

### 3. Define the config struct

Example shape:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ExampleConfig {
    pub enabled: bool,
    pub base_url: String,
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub test_poll_interval_ms: Option<u64>,
}
```

Guidance:

- prefer explicit fields over free-form maps
- include `enabled`
- include a poll interval for polled integrations
- include test-only timing overrides when they materially simplify tests
- use serde defaults only when there is a clear stable default

### 4. Validate the config inside the adapter crate

Add a small `validate_config()` function.

Rules should be adapter-owned and error messages should name the full config path.

Examples:

- `adapters.elgato_lights.base_url must not be empty`
- `adapters.open_meteo.poll_interval_secs must be >= 60`
- `adapters.roku_tv.ip_address must not be empty`

Do not move adapter-specific validation into `core`.

### 5. Implement the factory

Pattern:

```rust
pub struct ExampleFactory;

static EXAMPLE_FACTORY: ExampleFactory = ExampleFactory;

inventory::submit! {
    RegisteredAdapterFactory {
        factory: &EXAMPLE_FACTORY,
    }
}

impl AdapterFactory for ExampleFactory {
    fn name(&self) -> &'static str {
        ADAPTER_NAME
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        let config: ExampleConfig = serde_json::from_value(config)
            .context("failed to parse example adapter config")?;
        validate_config(&config)?;

        if !config.enabled {
            return Ok(None);
        }

        Ok(Some(Box::new(ExampleAdapter::new(config))))
    }
}
```

Factory contract:

- `Ok(None)` means valid but disabled
- `Err(...)` means invalid config

### 6. Implement the adapter struct

Typical fields:

- HTTP client or protocol client
- parsed config or derived config fields
- poll interval
- any adapter-local state needed across polls

Examples:

- `adapter-open-meteo` stores `client`, `config`, `base_url`, `poll_interval`
- `adapter-elgato-lights` stores `client`, `config`, `poll_interval`, `known_lights`
- `adapter-roku-tv` stores `client`, `poll_interval`, `base_url`

Prefer storing derived fields if they reduce duplication.

### 7. Implement polling

Most adapters will want a `poll_once(&self, registry: &DeviceRegistry) -> Result<()>` helper.

Inside `poll_once`:

1. fetch external state
2. map it into canonical `Device` values
3. `registry.upsert(...)` each device
4. remove stale devices if the upstream source is authoritative and devices disappeared

When building devices:

- preserve prior `room_id`
- preserve `updated_at` when state and metadata are unchanged
- always set `last_seen` to now on successful observation

Minimal pattern:

```rust
let previous = registry.get(&device_id);
let device = build_device(vendor_id, state, previous.as_ref());
registry.upsert(device).await?;
```

### 8. Implement `run()`

Pattern:

```rust
#[async_trait]
impl Adapter for ExampleAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(Event::AdapterStarted {
            adapter: self.name().to_string(),
        });

        loop {
            if let Err(error) = self.poll_once(&registry).await {
                tracing::error!(error = %error, "example poll failed");
                bus.publish(Event::SystemError {
                    message: format!("example poll failed: {error}"),
                });
            }

            sleep(self.poll_interval).await;
        }
    }
}
```

Important note:

- `crates/core/src/adapter.rs` mentions exponential backoff in the trait docs, but current adapters use fixed sleep intervals after failures.
- Follow the current implementation style unless the user explicitly asks to change runtime behavior across adapters.

### 9. Implement commands only if needed

Only override `command()` when the adapter supports device control.

Pattern:

1. identify whether the device belongs to this adapter
2. return `Ok(false)` if it does not
3. translate the canonical command into vendor operations
4. apply the change
5. confirm resulting state if needed
6. update the registry with the canonical post-command state

Examples:

- `adapter-elgato-lights` parses the light index from the device ID
- `adapter-roku-tv` compares against one stable device ID

### 10. Respect canonical command semantics

Commands are already validated by `DeviceCommand::validate()` before adapters see them.

But you may still need adapter-local validation when the canonical value is broader than the vendor supports.

Example:

- `color_temperature` accepts a canonical kelvin range in `core`
- `adapter-elgato-lights` narrows that to `2900..=7000`

This is the right pattern:

- keep canonical validation in `core`
- add narrower vendor-specific validation in the adapter crate

### 11. Map to canonical device state

Use `DeviceKind` intentionally:

- `Sensor` for read-only measured values
- `Light` for controllable lighting devices
- `Switch` for generic on/off devices
- `Virtual` only when the device is logical rather than physical

Choose attributes from existing capabilities when possible.

Canonical ownership rules:

- use `device.attributes.<capability_key>` first whenever a canonical capability already covers the state or command
- use `custom.<adapter>.<field>` only for current-state attributes that do not fit the canonical catalog
- use `metadata.vendor_specific` for opaque upstream identifiers, descriptive metadata, and other non-canonical adapter details
- do not publish the same meaning in both a canonical capability and vendor-specific data

Examples already used in the repo:

- weather: `temperature_outdoor`, `wind_speed`, `wind_direction`
- lights: `power`, `state`, `brightness`, `color_temperature`
- Roku TV: `power`, `state`

Use `metadata.vendor_specific` for source details that are useful but not yet canonicalized.

Examples:

- Elgato stores `light_index`
- Roku stores `power_mode`, `friendly_name`, `model_name`

### 12. Handle disappearing devices carefully

If the upstream API is authoritative for the current device list, remove stale devices.

Reference:

- `adapter-elgato-lights` tracks `known_lights` and removes devices that disappear

Do not remove devices just because one poll failed.

### 13. Add workspace linkage

After the crate exists, update:

1. root `Cargo.toml`
2. `crates/adapters/Cargo.toml`
3. `crates/adapters/src/lib.rs`

Without this, the crate may compile in isolation but its factory will not be linked into the API binary.

### 14. Add config examples

Update at least:

- `config/default.toml`

If the adapter is generally optional, default it to disabled unless there is a strong reason not to.

### 15. Add tests in the adapter crate

Every adapter should have focused tests covering the highest-risk parts.

Minimum expected areas:

- config validation
- successful polling normalization
- command translation if commands exist
- state freshness behavior where relevant
- factory disabled behavior or invalid config behavior

Use the existing adapters as test style references.

Recommended test design:

- use a tiny in-process mock server built with `tokio::net::TcpListener`
- capture requests for assertions
- support ephemeral ports for test reliability
- use test-only constructor overrides when the real integration uses fixed hosts or ports

### 16. Verify with targeted commands first

Run targeted checks before a full workspace pass.

Examples:

```bash
cargo fmt --all
cargo test -p adapter-your-name -p smart-home-adapters
```

Then, if requested or appropriate:

```bash
cargo check --workspace
cargo test --workspace
```

## Code Template

Use this as a starting skeleton.

```rust
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use smart_home_core::adapter::{Adapter, AdapterFactory, RegisteredAdapterFactory};
use smart_home_core::bus::EventBus;
use smart_home_core::command::DeviceCommand;
use smart_home_core::config::AdapterConfig;
use smart_home_core::event::Event;
use smart_home_core::model::DeviceId;
use smart_home_core::registry::DeviceRegistry;
use tokio::time::{sleep, Duration};

const ADAPTER_NAME: &str = "example";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExampleConfig {
    pub enabled: bool,
    pub poll_interval_secs: u64,
}

pub struct ExampleFactory;

static EXAMPLE_FACTORY: ExampleFactory = ExampleFactory;

inventory::submit! {
    RegisteredAdapterFactory {
        factory: &EXAMPLE_FACTORY,
    }
}

pub struct ExampleAdapter {
    poll_interval: Duration,
}

impl ExampleAdapter {
    pub fn new(config: ExampleConfig) -> Self {
        Self {
            poll_interval: Duration::from_secs(config.poll_interval_secs),
        }
    }

    async fn poll_once(&self, registry: &DeviceRegistry) -> Result<()> {
        let _ = registry;
        Ok(())
    }
}

impl AdapterFactory for ExampleFactory {
    fn name(&self) -> &'static str {
        ADAPTER_NAME
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        let config: ExampleConfig = serde_json::from_value(config)
            .context("failed to parse example adapter config")?;

        if config.poll_interval_secs == 0 {
            bail!("adapters.example.poll_interval_secs must be >= 1");
        }

        if !config.enabled {
            return Ok(None);
        }

        Ok(Some(Box::new(ExampleAdapter::new(config))))
    }
}

#[async_trait]
impl Adapter for ExampleAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(Event::AdapterStarted {
            adapter: self.name().to_string(),
        });

        loop {
            if let Err(error) = self.poll_once(&registry).await {
                bus.publish(Event::SystemError {
                    message: format!("example poll failed: {error}"),
                });
            }

            sleep(self.poll_interval).await;
        }
    }

    async fn command(
        &self,
        _device_id: &DeviceId,
        _command: DeviceCommand,
        _registry: DeviceRegistry,
    ) -> Result<bool> {
        Ok(false)
    }
}
```

## Common Mistakes

Avoid these mistakes:

1. Editing `crates/api/src/main.rs` to manually instantiate your adapter.
2. Editing `crates/core/src/config.rs` to add adapter-specific config structs.
3. Using device IDs without the adapter prefix.
4. Dropping `room_id` on refresh.
5. Replacing `updated_at` on every poll even when state is identical.
6. Returning `Err(...)` for a device ID the adapter does not own.
7. Forgetting to link the crate through `crates/adapters`.
8. Adding new capabilities when an existing one is already sufficient.
9. Writing tests that require a fixed local port unless the test has exclusive control.
10. Treating vendor metadata as canonical attributes prematurely.

## Review Checklist For An Agent

Before finishing, verify all of the following:

- a new crate exists under `crates/adapter-<name>`
- the crate has an `AdapterFactory`
- the crate registers itself with `inventory`
- the adapter name matches the config section key
- the adapter is linked in root `Cargo.toml`
- the adapter is linked in `crates/adapters/Cargo.toml`
- `crates/adapters/src/lib.rs` imports the crate for side-effect registration
- `config/default.toml` contains a usable example section
- device IDs are namespaced correctly
- room assignment is preserved across refreshes
- `updated_at` stays stable for identical state
- commands return `Ok(false)` for unsupported devices or unsupported command routing
- focused tests cover polling and commands where applicable
- `cargo fmt --all` passes
- targeted tests pass

## Current Documentation Drift To Be Aware Of

As of this guide's creation, some docs are slightly stale.

- `README.md` workspace layout does not yet list `crates/adapter-roku-tv`
- `README.md` has an Open-Meteo section whose config example currently shows Elgato config
- `crates/core/src/adapter.rs` mentions exponential backoff, but current adapters use fixed retry sleeps

Do not blindly copy stale docs over the source of truth in the code.

When there is disagreement, prefer the current implementation unless the task is explicitly to clean up or change the contract.

## MCP Tooling

The MCP server (`crates/mcp-server`) exposes tools that assist with adapter authoring:

- `scaffold_adapter` — generates a new adapter crate skeleton with the correct
  `inventory::submit!` factory boilerplate and prints the three manual registration steps
  (root `Cargo.toml`, `crates/adapters/Cargo.toml`, `crates/adapters/src/lib.rs`)
- `run_cargo_check` — runs `cargo check` on the workspace or a focused package
- `run_cargo_test` — runs `cargo test` on the workspace or a focused package
- `list_capabilities` — lists the canonical capability schemas the runtime knows about

This guide remains the authoritative implementation contract. The MCP tools handle
scaffolding and verification; adapters still translate external vendor protocols into
canonical state and command semantics.
