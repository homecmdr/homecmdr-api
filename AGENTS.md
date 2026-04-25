# AGENTS.md — HomeCmdr API

## Workspace layout

Cargo workspace with 9 member crates under `crates/`:

| Crate | Role |
|---|---|
| `api` | Main binary — axum HTTP server, all handlers, entry point |
| `core` | Domain types, traits, runtime, event bus, config structs |
| `lua-host` | Lua 5.4 scripting engine (mlua) for scenes & automations |
| `automations` | Automation catalog, runner, trigger logic |
| `scenes` | Scene catalog and execution |
| `store-sql` | SQLite + PostgreSQL persistence via sqlx (schema as `const` SQL) |
| `store-postgres` | PostgreSQL-specific store impl |
| `plugin-host` | WASM plugin loader (wasmtime component-model) |
| `plugin-sdk` | SDK for out-of-process IPC plugins (distinct from WASM) |

`plugins/open-meteo/` is a separate sub-workspace; its compiled `.wasm` output goes into `config/plugins/` and is not built by the main workspace CI.

## Essential commands

```bash
# Build / check
cargo build                          # whole workspace
cargo build --release -p api         # release binary (named `api`, deployed as `homecmdr`)
cargo check -p api                   # fast type-check without full compile

# Test
cargo test                           # all crates
cargo test -p api                    # API integration tests only
cargo test -p api health_returns_ok  # single named test
cargo test -p api devices -- --nocapture  # pattern match + show stdout

# Lint / format
cargo clippy --all-targets --all-features
cargo fmt
cargo fmt --check                    # CI-style check

# Docker / local stack
docker compose up postgres           # Postgres only (for local dev against Postgres)
docker compose up                    # full stack (app + Postgres)
```

There is no `npm`, `make`, or `task` runner — pure Cargo.

## Configuration (not `.env`)

Config is TOML-only; there is no dotenv/`.env` loading.

- `config/default.toml` — canonical defaults, committed
- `config/local.toml` — personal dev overrides, **gitignored** (do not commit)
- `HOMECMDR_CONFIG=<path>` — override config file path
- `HOMECMDR_MASTER_KEY=<key>` — overrides `auth.master_key` from config
- `HOMECMDR_DATA_DIR=<dir>` — base dir for relative `database_url` paths

Default bind: `127.0.0.1:3001`. Local config (already present) uses `0.0.0.0:3001`.

## Running the server

```bash
cargo run -p api                                 # uses config/default.toml
HOMECMDR_CONFIG=config/local.toml cargo run -p api
```

## Tests

- All API integration tests are in `crates/api/src/tests.rs`.
- Tests spin up a real axum server on `127.0.0.1:0` (random port) — no external services needed.
- Each test gets an isolated SQLite DB via `temp_sqlite_url()` (nanosecond-stamped file in `$TMPDIR`). These are **not cleaned up** automatically.
- Test auth key: `Authorization: Bearer homecmdr-test-key-for-tests-only`

## Database schema

No migration files exist. The entire schema is inline `const &str` SQL in `crates/store-sql/src/sqlite.rs` (and the Postgres counterpart). `CREATE TABLE IF NOT EXISTS` runs on every startup when `persistence.auto_create = true`. Evolutionary changes require manual `ALTER TABLE` — there is no versioned migration system.

`.sqlx/` is gitignored. If offline sqlx query checking is ever enabled, run `cargo sqlx prepare`.

## CI

Only a release workflow exists (`.github/workflows/release.yml`). It triggers on `v*` tags and builds `x86_64` and `aarch64` Linux binaries. **There are no CI jobs for `cargo test`, `cargo clippy`, or `cargo fmt --check`** — all quality checks are local-only.

Release flow:
```bash
git tag -a v0.2.0 -m "v0.2.0"
git push origin v0.2.0
# CI builds and attaches binaries to a GitHub Release automatically
```

## Non-obvious conventions

- **Binary naming:** the crate is `api` (`cargo build -p api`), but the binary is deployed/renamed as `homecmdr` (Dockerfile, systemd unit, release asset `homecmdr-server-<target>`).
- **WASM plugins:** loaded at runtime from `config/plugins/*.wasm` + `*.plugin.toml` pairs. Modify `plugins/open-meteo/` source, build it separately, then copy `.wasm` into `config/plugins/`. CI does not build plugins.
- **Hot reload:** scenes/automations support `watch = true` in config for live reload without restart (uses `notify` crate, see `crates/api/src/reload.rs`).
- **Auth:** Bearer-token only. Master key (SHA-256 hashed internally) grants Admin role and bypasses the per-key `ApiKeyStore`.
- **`config/local.toml` exists on disk** but is gitignored — treat as local-only, never commit.
