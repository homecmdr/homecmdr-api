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

- [ ] `gh repo create homecmdr/adapters --public`
- [ ] `git init /home/andy/projects/homecmdr/adapters`
- [ ] Create directory structure:
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
- [ ] Copy each adapter's `Cargo.toml`, `src/lib.rs`, `README.md` from `homecmdr-api`
- [ ] Add standard "how to install" header to each adapter `README.md`
- [ ] Write `adapters.toml` registry manifest (all 4 adapters)
- [ ] Write top-level `README.md`
- [ ] Add `.gitignore`
- [ ] Initial commit → push → tag `v0.1.0` → GitHub Release

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

- [ ] `gh repo create homecmdr/homecmdr-cli --public`
- [ ] `cargo new --bin homecmdr-cli` at `/home/andy/projects/homecmdr/homecmdr-cli`
- [ ] Add dependencies to `Cargo.toml`:
  - `clap = { version = "4", features = ["derive"] }`
  - `reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls"] }`
  - `serde = { version = "1", features = ["derive"] }`
  - `toml = "0.8"`
  - `zip = "2"`
  - `anyhow = "1"`
- [ ] Implement `src/main.rs` — clap entrypoint, `pull` subcommand
- [ ] Implement `src/commands/pull.rs`:
  1. Walk up from `$CWD` to find workspace root (`Cargo.toml` containing `[workspace]`)
  2. Fetch `https://raw.githubusercontent.com/homecmdr/adapters/main/adapters.toml`
  3. Match adapter name — clear error if not found, list available adapters
  4. Download `https://github.com/homecmdr/adapters/archive/refs/heads/main.zip`
  5. Extract `adapters-main/<path>/` → `<workspace-root>/crates/<adapter-name>/`
  6. Patch workspace `Cargo.toml` — append to `members` if not already present
  7. Patch `crates/adapters/Cargo.toml` — append `<name> = { path = "../<name>" }`
  8. Print: `adapter-<name> added. Run 'cargo build' to rebuild.`
- [ ] Write `README.md`
- [ ] `cargo build` — verify it compiles
- [ ] Initial commit → push → tag `v0.1.0` → GitHub Release

---

## Phase 3 — Clean up `homecmdr-api`

Branch: `feat/extract-adapters`

### Files to delete
- [ ] `git rm -r crates/adapter-elgato-lights`
- [ ] `git rm -r crates/adapter-ollama`
- [ ] `git rm -r crates/adapter-roku-tv`
- [ ] `git rm -r crates/adapter-zigbee2mqtt`

### `Cargo.toml` (workspace root)
- [ ] Remove from `members`:
  - `"crates/adapter-zigbee2mqtt"`
  - `"crates/adapter-ollama"`
  - `"crates/adapter-elgato-lights"`
  - `"crates/adapter-roku-tv"`

### `crates/adapters/Cargo.toml`
- [ ] Remove deps: `adapter-zigbee2mqtt`, `adapter-elgato-lights`, `adapter-ollama`, `adapter-roku-tv`
- [ ] Keep: `adapter-open-meteo = { path = "../adapter-open-meteo" }`

### `config/default.toml`
- [ ] Remove `[adapters.elgato_lights]`, `[adapters.roku_tv]`, `[adapters.ollama]`, `[adapters.zigbee2mqtt]` blocks
- [ ] Keep `[adapters.open_meteo]`
- [ ] Add comment above `[adapters.open_meteo]` pointing to the adapters repo and CLI

### `README.md`
- [ ] "Adapter-specific docs" — remove 4 adapter links, keep `adapter-open-meteo`; add link to `https://github.com/homecmdr/adapters`
- [ ] "Current Adapters" section — remove 4 adapters; add note pointing to the adapters repo and CLI

### `AGENTS.md`
- [ ] Remove `adapter-elgato-lights`, `adapter-ollama`, `adapter-roku-tv`, `adapter-zigbee2mqtt` from focused test commands
- [ ] Update workspace map note for `crates/adapter-*`
- [ ] Adapter Rules — add note that adapters are installed via `homecmdr pull`

### `config/docs/adapter_authoring_guide.md`
- [ ] Add note at top pointing to `https://github.com/homecmdr/adapters` for publishing official adapters

### Version bump — all crates `0.1.0` → `0.2.0`
- [ ] `crates/core/Cargo.toml`
- [ ] `crates/api/Cargo.toml`
- [ ] `crates/adapters/Cargo.toml`
- [ ] `crates/automations/Cargo.toml`
- [ ] `crates/scenes/Cargo.toml`
- [ ] `crates/lua-host/Cargo.toml`
- [ ] `crates/store-sql/Cargo.toml`
- [ ] `crates/store-postgres/Cargo.toml`
- [ ] `crates/mcp-server/Cargo.toml`
- [ ] `crates/adapter-open-meteo/Cargo.toml`

### Verify and ship
- [ ] `cargo check --workspace` — must pass cleanly
- [ ] Commit all changes on `feat/extract-adapters`
- [ ] Push → PR to `main` → merge → delete branch
- [ ] Tag `v0.2.0` → GitHub Release

---

## Final state

```
github.com/homecmdr/
├── homecmdr-api     v0.2.0  (open-meteo bundled; 4 adapters extracted)
├── homecmdr-dash    v0.1.0  (unchanged)
├── homecmdr-cli     v0.1.0  (homecmdr pull <adapter>)
└── adapters         v0.1.0  (monorepo: 4 official adapters + registry manifest)
```
