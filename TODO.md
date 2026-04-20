# HomeCmdr — Project TODO

Tracked improvements and planned work across the HomeCmdr project.
Items are roughly ordered by priority within each section.

---

## CLI (`homecmdr-cli`)

### `plugin.toml` config manifest — adapter design contract
**Status:** Implemented in v0.3.0

Each adapter crate must ship a `plugin.toml` that declares its config fields
(key, type, description, default, required/optional, secret flag). The CLI reads
this file after extraction and interactively prompts the user, then appends the
completed `[adapters.<name>]` block to `config/default.toml`. The CLI hard-fails
if `plugin.toml` is absent — there is no fallback.

**Open questions / follow-on work:**
- Secret fields (`secret = true`) currently use a plain prompt. Consider adding
  masked input via `rpassword` crate so passwords are not echoed to the terminal.
- Consider a `homecmdr plugin configure <name>` command to re-run the prompts
  for an already-installed plugin (e.g. after a password change).
- Validate typed input at prompt time: `u64` fields should reject non-numeric
  input and enforce `>= 1` minimums where the adapter requires it, rather than
  letting the API fail on startup.

---

### Database backend selection during `homecmdr init`
**Status:** Implemented in v0.3.0

`homecmdr init` now asks whether to use SQLite (default, embedded, no setup) or
PostgreSQL (external server). PostgreSQL prompts for host, port, database,
username, and password and assembles the `postgres://` URL.

**Open questions / follow-on work:**
- When PostgreSQL is selected, attempt a test connection before writing config
  so the user gets an immediate error rather than a service startup failure.
- For PostgreSQL, offer to create the database automatically if it does not exist
  (requires a superuser connection, so this may need a separate prompt).
- Document the `HOMECMDR_MASTER_KEY` and `HOMECMDR_DATA_DIR` env var overrides
  more prominently in the init output and generated config comments.

---

### `homecmdr plugin configure <name>`
**Status:** Planned

Re-run the `plugin.toml` prompts for an already-installed plugin and update the
existing `[adapters.<name>]` block in `config/default.toml` in place (rather than
appending a duplicate block). Useful after changing a device IP or MQTT password.

---

### `homecmdr upgrade`
**Status:** Planned

Download the latest `homecmdr-api` source archive, replace the workspace, preserve
the existing `config/` directory and state file, rebuild, and restart the service.
Needs careful handling of workspace patches (installed plugins must be re-applied).

---

### `homecmdr plugin update <name>`
**Status:** Planned

Pull the latest version of a specific plugin from the registry, replace the crate
directory, re-patch workspace files, and rebuild.

---

## API (`homecmdr-api`)

### PostgreSQL migration tooling
**Status:** Planned

The SQLite backend uses in-code schema init (`sqlite.rs`). The PostgreSQL backend
(`store-postgres`) should follow the same approach for now, but a proper migration
story (e.g. `sqlx migrate`) should be added before a 1.0 release so schema changes
can be applied to production databases without data loss.

### Adapter hot-reload
**Status:** Planned

Currently the server must be restarted to pick up config changes. A `SIGHUP`
handler or a `POST /api/reload` endpoint that gracefully restarts only the
affected adapter(s) without dropping the event bus would improve the ops story.

### Dashboard CORS defaults
**Status:** Known friction

The default config allows `http://127.0.0.1:8080` as a CORS origin. Users
accessing the dashboard from another machine must edit this manually. The CLI
`init` prompt could ask for allowed origins (or a flag to allow all origins for
local-network-only deployments).

---

## Adapters (`adapters/`)

### `plugin.toml` for any new adapters
**Status:** Ongoing requirement

`plugin.toml` is now a required part of the adapter design contract. Any new
adapter submitted to the registry must include it. See the existing four adapters
for examples.

### `adapter-roku-tv` — README accuracy
**Status:** Known issue

The README states the device registers as `DeviceKind::Switch` but the source
registers it as `DeviceKind::Virtual`. The README also understates the available
capabilities (power only) — the adapter actually supports `media_app`,
`media_source`, and `media_playback`. README should be updated to match the code.
