# Adapter Extraction Plan

Extract the four bundled adapter crates from `homecmdr-api` into a single
`homecmdr/adapters` monorepo, and ship a `homecmdr-cli` binary that lets users
pull official adapters into their local workspace.

## Status legend

- [ ] not started
- [x] done

---

## Repos involved

| Repo | Action |
|---|---|
| `homecmdr/homecmdr-api` | Remove 4 adapters, update config/docs, bump to v0.2.0 |
| `homecmdr/adapters` | Create — monorepo for all official adapters + registry manifest |
| `homecmdr/homecmdr-cli` | Create — `homecmdr pull <adapter>` Rust binary |

---

## Git branching strategy

- **`homecmdr-api`**: all changes on branch `feat/extract-adapters`, PR to `main`, tag `v0.2.0` on merge
- **`homecmdr/adapters`**: new repo, single initial commit directly on `main`, tag `v0.1.0`
- **`homecmdr/homecmdr-cli`**: new repo, commits on `main` as CLI is built, tag `v0.1.0` when complete

---

## Phase 1 — Create `homecmdr/adapters` monorepo

- [x] `gh repo create homecmdr/adapters --public`
- [x] `git init /home/andy/projects/homecmdr/adapters`
- [x] Create directory structure:
  ```
  adapters/
  ├── adapters.toml
  ├── README.md
  ├── adapter-elgato-lights/
  │   ├── Cargo.toml
  │   ├── src/lib.rs
  │   └── README.md
  ├── adapter-ollama/
  │   ├── Cargo.toml
  │   ├── src/lib.rs
  │   └── README.md
  ├── adapter-roku-tv/
  │   ├── Cargo.toml
  │   ├── src/lib.rs
  │   └── README.md
  └── adapter-zigbee2mqtt/
      ├── Cargo.toml
      ├── src/lib.rs
      └── README.md
  ```
- [x] Copy each adapter's `Cargo.toml`, `src/lib.rs`, `README.md` from `homecmdr-api`
- [x] Add standard "how to install" header to each adapter `README.md`
- [x] Write `adapters.toml` registry manifest (all 4 adapters)
- [x] Write top-level `README.md`
- [x] Add `.gitignore`
- [x] Initial commit → push → tag `v0.1.0` → GitHub Release

### `adapters.toml` format

```toml
[[adapters]]
name = "adapter-elgato-lights"
display_name = "Elgato Key Light"
description = "Control Elgato Key Light and Key Light Air devices over the local network."
path = "adapter-elgato-lights"
version = "0.1.0"

[[adapters]]
name = "adapter-ollama"
display_name = "Ollama"
description = "Service-style Lua access to a local Ollama LLM instance."
path = "adapter-ollama"
version = "0.1.0"

[[adapters]]
name = "adapter-roku-tv"
display_name = "Roku TV"
description = "Power control for a Roku TV via the ECP HTTP API."
path = "adapter-roku-tv"
version = "0.1.0"

[[adapters]]
name = "adapter-zigbee2mqtt"
display_name = "Zigbee2MQTT"
description = "MQTT-backed adapter for Zigbee devices via Zigbee2MQTT."
path = "adapter-zigbee2mqtt"
version = "0.1.0"
```

---

## Phase 2 — Create `homecmdr/homecmdr-cli`

- [x] `gh repo create homecmdr/homecmdr-cli --public`
- [x] `cargo new --bin homecmdr-cli` at `/home/andy/projects/homecmdr/homecmdr-cli`
- [x] Add dependencies to `Cargo.toml`:
  - `clap = { version = "4", features = ["derive"] }`
  - `reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls"] }`
  - `serde = { version = "1", features = ["derive"] }`
  - `toml = "0.8"`
  - `zip = "2"`
  - `anyhow = "1"`
- [x] Implement `src/main.rs` — clap entrypoint, `pull` subcommand
- [x] Implement `src/commands/pull.rs`:
  1. Walk up from `$CWD` to find workspace root (`Cargo.toml` containing `[workspace]`)
  2. Fetch `https://raw.githubusercontent.com/homecmdr/adapters/main/adapters.toml`
  3. Match adapter name — clear error if not found, list available adapters
  4. Download `https://github.com/homecmdr/adapters/archive/refs/heads/main.zip`
  5. Extract `adapters-main/<path>/` → `<workspace-root>/crates/<adapter-name>/`
  6. Patch workspace `Cargo.toml` — append to `members` if not already present
  7. Patch `crates/adapters/Cargo.toml` — append `<name> = { path = "../<name>" }`
  8. Print: `adapter-<name> added. Run 'cargo build' to rebuild.`
- [x] Write `README.md`
- [x] `cargo build` — verify it compiles
- [x] Initial commit → push → tag `v0.1.0` → GitHub Release

---

## Phase 3 — Clean up `homecmdr-api`

Branch: `feat/extract-adapters`

### Files to delete
- [x] `git rm -r crates/adapter-elgato-lights`
- [x] `git rm -r crates/adapter-ollama`
- [x] `git rm -r crates/adapter-roku-tv`
- [x] `git rm -r crates/adapter-zigbee2mqtt`

### `Cargo.toml` (workspace root)
- [x] Remove from `members`:
  - `"crates/adapter-zigbee2mqtt"`
  - `"crates/adapter-ollama"`
  - `"crates/adapter-elgato-lights"`
  - `"crates/adapter-roku-tv"`

### `crates/adapters/Cargo.toml`
- [x] Remove deps: `adapter-zigbee2mqtt`, `adapter-elgato-lights`, `adapter-ollama`, `adapter-roku-tv`
- [x] Keep: `adapter-open-meteo = { path = "../adapter-open-meteo" }`

### `crates/adapters/src/lib.rs`
- [x] Remove 4 `use ... as _` imports; keep only `adapter-open-meteo`

### `config/default.toml`
- [x] Remove `[adapters.elgato_lights]`, `[adapters.roku_tv]`, `[adapters.ollama]`, `[adapters.zigbee2mqtt]` blocks
- [x] Keep `[adapters.open_meteo]`
- [x] Add comment above `[adapters.open_meteo]` pointing to the adapters repo and CLI

### `README.md`
- [x] "Adapter-specific docs" — remove 4 adapter links, keep `adapter-open-meteo`; add link to `https://github.com/homecmdr/adapters`
- [x] "Current Adapters" section — remove 4 adapters; add note pointing to the adapters repo and CLI

### `AGENTS.md`
- [x] Remove `adapter-elgato-lights`, `adapter-ollama`, `adapter-roku-tv`, `adapter-zigbee2mqtt` from focused test commands
- [x] Update workspace map note for `crates/adapter-*`
- [x] Adapter Rules — add note that adapters are installed via `homecmdr pull`

### `config/docs/adapter_authoring_guide.md`
- [x] Add note at top pointing to `https://github.com/homecmdr/adapters` for publishing official adapters

### Version bump — all crates `0.1.0` → `0.2.0`
- [x] `crates/core/Cargo.toml`
- [x] `crates/api/Cargo.toml`
- [x] `crates/adapters/Cargo.toml`
- [x] `crates/automations/Cargo.toml`
- [x] `crates/scenes/Cargo.toml`
- [x] `crates/lua-host/Cargo.toml`
- [x] `crates/store-sql/Cargo.toml`
- [x] `crates/store-postgres/Cargo.toml`
- [x] `crates/mcp-server/Cargo.toml`
- [x] `crates/adapter-open-meteo/Cargo.toml`

### Verify and ship
- [x] `cargo check --workspace` — must pass cleanly
- [x] Commit all changes on `feat/extract-adapters`
- [x] Push → PR to `main` → merge → delete branch
- [x] Tag `v0.2.0` → GitHub Release

---

## Final state

```
github.com/homecmdr/
├── homecmdr-api     v0.2.0  (open-meteo bundled; 4 adapters extracted) ✅
├── homecmdr-dash    v0.1.0  (unchanged) ✅
├── homecmdr-cli     v0.1.0  (homecmdr pull <adapter>) ✅
└── adapters         v0.1.0  (monorepo: 4 official adapters + registry manifest) ✅
```
