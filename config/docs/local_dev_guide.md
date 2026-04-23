# HomeCmdr — Local Development Guide

A step-by-step reference for running the API locally, iterating on plugins,
and keeping local changes off the remote repo.

---

## Prerequisites

Install the Rust toolchain (stable) and the WASM target:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-wasip2
```

Verify:

```bash
rustc --version
rustup target list --installed | grep wasm32-wasip2
```

---

## 1. Create your local config file

`config/default.toml` is the committed baseline — **never edit it for local
dev purposes**.  It is what the production/CI environment uses.

Instead, create a local copy that is gitignored:

```bash
cp config/default.toml config/local.toml
```

`config/local.toml` is listed in `.gitignore` and will never be pushed to
remote.  All local additions (plugin adapter config, dev credentials, adjusted
ports, etc.) go in here.

---

## 2. Create the plugins directory

The plugin scanner expects this directory to exist.  Create it once:

```bash
mkdir -p config/plugins
```

Files placed here (`*.wasm`, `*.plugin.toml`) are also gitignored — they will
not be pushed.

---

## 3. Run the API

Always run from the **workspace root** (`homecmdr-api/`) so that relative
paths in config resolve correctly.

```bash
# From homecmdr-api/
HOMECMDR_CONFIG=config/local.toml cargo run -p api
```

The `HOMECMDR_CONFIG` variable tells the API to read your local config instead
of the default.  The API is ready when you see:

```
INFO homecmdr_api: listening on 127.0.0.1:3001
```

To override the master key without editing the file:

```bash
HOMECMDR_CONFIG=config/local.toml HOMECMDR_MASTER_KEY=mydevkey cargo run -p api
```

---

## 4. Adding a plugin for local testing

This is the manual equivalent of `homecmdr plugin add`.  You need three
things: the WASM binary, the manifest, and an adapter config block.

### Step A — build the plugin

From the plugin's own directory (these live in `../plugins/<name>/` relative
to the API workspace):

```bash
cd ../plugins/plugin-elgato-lights   # or whichever plugin

cargo build --release
# binary is at:  target/wasm32-wasip2/release/<crate_name>.wasm
```

The compiled filename uses underscores and matches the crate name in
`Cargo.toml`.  For `plugin-elgato-lights-wasm` this produces
`plugin_elgato_lights_wasm.wasm`.

### Step B — copy binary and manifest into config/plugins

```bash
# Back in homecmdr-api/
cp ../plugins/plugin-elgato-lights/target/wasm32-wasip2/release/plugin_elgato_lights_wasm.wasm \
   config/plugins/elgato_lights.wasm

cp ../plugins/plugin-elgato-lights/elgato_lights.plugin.toml \
   config/plugins/elgato_lights.plugin.toml
```

The `.wasm` filename stem must match the manifest stem:
`elgato_lights.wasm` + `elgato_lights.plugin.toml`.

The manifest's `[plugin] name` field must also match the key you add in the
next step.

### Step C — add the adapter config block to config/local.toml

Open `config/local.toml` and append a section for the plugin.  The entire
section is forwarded as JSON to the plugin's `init()` function at startup.

```toml
[adapters.elgato_lights]
enabled  = true
base_url = "http://127.0.0.1:9123"
```

Check the plugin's source or its `*.plugin.toml` for the available fields and
their defaults.

### Step D — restart the API

The plugin scanner runs at startup only.  Stop and restart:

```bash
HOMECMDR_CONFIG=config/local.toml cargo run -p api
```

On startup you should see a log line confirming the plugin loaded:

```
INFO homecmdr_plugin_host: loaded plugin elgato_lights
```

---

## 5. The inner dev loop (plugin source changes)

Once the plugin is set up, the loop for iterating on its source is:

```bash
# 1. Edit plugin source in ../plugins/plugin-elgato-lights/src/lib.rs

# 2. Rebuild the WASM binary
cd ../plugins/plugin-elgato-lights
cargo build --release

# 3. Copy the fresh binary (manifest does not need re-copying unless changed)
cd ../../homecmdr-api
cp ../plugins/plugin-elgato-lights/target/wasm32-wasip2/release/plugin_elgato_lights_wasm.wasm \
   config/plugins/elgato_lights.wasm

# 4. Restart the API
HOMECMDR_CONFIG=config/local.toml cargo run -p api
```

There is no hot-reload for plugins — a restart is always required after
replacing a `.wasm` file.

---

## 6. What is and is not pushed to remote

| File / directory | Committed? | Notes |
|---|---|---|
| `config/default.toml` | Yes | Prod baseline — do not add local plugin config here |
| `config/local.toml` | **No** | Gitignored — your local overrides live here |
| `config/plugins/*.wasm` | **No** | Gitignored — installed per environment |
| `config/plugins/*.plugin.toml` | **No** | Gitignored — installed per environment |
| `config/scenes/*.lua` | Yes | Scene scripts are source-controlled |
| `config/automations/*.lua` | Yes | Automation scripts are source-controlled |
| `config/scripts/*.lua` | Yes | Shared Lua modules are source-controlled |
| `data/*.db` | **No** | Gitignored — SQLite database |

The rule: anything environment-specific (local credentials, installed plugins,
runtime data) is gitignored.  Everything that defines behaviour (Lua scripts,
the default config skeleton) is committed.

---

## 7. Useful commands

```bash
# Check the whole workspace compiles (fast, no linking)
cargo check --workspace

# Run all tests
cargo test --workspace

# Run tests for a specific crate
cargo test -p homecmdr-core
cargo test -p homecmdr-plugin-host

# Format all Rust source
cargo fmt --all

# Check what plugins the API picked up (requires API running)
curl -H "Authorization: Bearer mydevkey" http://127.0.0.1:3001/adapters | jq

# List devices reported by plugins
curl -H "Authorization: Bearer mydevkey" http://127.0.0.1:3001/devices | jq

# Tail the API log at a higher verbosity
RUST_LOG=debug HOMECMDR_CONFIG=config/local.toml cargo run -p api
```

---

## 8. Troubleshooting

**Plugin does not appear in `/adapters`**
- Check that both `<name>.wasm` and `<name>.plugin.toml` are in `config/plugins/`.
- Check that `[plugins] enabled = true` and `directory = "config/plugins"` are in your local config.
- Check that `[adapters.<name>]` exists in your local config and the name matches `[plugin] name` in the manifest exactly (snake_case).
- Look for an error line at startup: `ERROR homecmdr_plugin_host`.

**`cargo run` picks up the wrong config**
- Make sure you are running from `homecmdr-api/` (the workspace root).
- Make sure `HOMECMDR_CONFIG=config/local.toml` is set in the same shell invocation.

**Plugin builds but crashes at init**
- Run with `RUST_LOG=debug` to see the full tracing output.
- The plugin's `init()` return value appears as a log line; any `Err(...)` string is printed there.

**`wasm32-wasip2` target not found**
```bash
rustup target add wasm32-wasip2
```

**Port already in use**
Change `api.bind_address` in `config/local.toml` to e.g. `"127.0.0.1:3002"`.
