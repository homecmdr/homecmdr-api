# Lua Hot Reload Plan

## Goal

Add safe, observable, and testable reload support for Lua assets (`scenes`,
`automations`, `scripts`) to improve authoring UX (human + MCP/AI) while
preserving runtime stability.

---

## Current State (Baseline)

- Scenes and automations are loaded at API startup.
- `/scenes/reload` and `/automations/reload` are intentionally not implemented.
- Script modules are loaded from `config/scripts` via `require(...)`.
- Startup/restart is currently required to pick up file edits.

---

## Design Principles

1. **Safety first** - never partially apply a broken catalog; keep old catalog
   active on any failure.
2. **Atomic swap** - build candidate catalog fully in memory, then swap in one
   operation.
3. **Operator control** - start with manual reload endpoints; no default
   auto-watch behavior.
4. **Good observability** - return clear API reload results, emit runtime
   events, log structured diagnostics.
5. **Backwards compatibility** - existing startup load behavior remains
   unchanged.

---

## Scope

### In scope (v1)

- Manual reload endpoints:
  - `POST /scenes/reload`
  - `POST /automations/reload`
  - `POST /scripts/reload`
- Full validation before activation.
- Atomic replacement of in-memory scene/automation catalogs.
- Runtime events for reload lifecycle.
- API docs and tests.

### Out of scope (v1)

- Default-on file-watch hot reload.
- Fine-grained per-file incremental reload.
- Mid-execution cancellation or restart of running automation jobs.
- Versioning or history UI for Lua assets.

---

## API Contract

### `POST /scenes/reload`

Rebuild the scenes catalog from the configured scenes directory. If all files
parse and validate successfully, atomically swap the active catalog. If any file
fails, keep the previous catalog and return a detailed error summary.

Response (success):

```json
{
  "status": "ok",
  "target": "scenes",
  "loaded_count": 12,
  "errors": []
}
```

Response (failure):

```json
{
  "status": "error",
  "target": "scenes",
  "loaded_count": 0,
  "errors": [
    {
      "file": "broken_scene.lua",
      "message": "missing required field: execute"
    }
  ]
}
```

### `POST /automations/reload`

Rebuild the automation catalog from the configured automations directory.
Reinitialize automation runner trigger bindings against the new catalog.
Existing in-flight executions continue to completion; new trigger evaluations
use the new catalog after the swap.

Same response shape as scenes reload.

### `POST /scripts/reload`

Signal that script module content has changed. Subsequent Lua executions will
load updated module content. Behavior within already-running execution contexts
is not guaranteed. Returns same response shape.

---

## Event Model Additions

Publish normalized events onto the event bus and expose over WebSocket `/events`:

| Event type                          | When emitted                                  |
|-------------------------------------|-----------------------------------------------|
| `scene.catalog_reload_started`      | Reload requested, load beginning              |
| `scene.catalog_reloaded`            | Swap succeeded                                |
| `scene.catalog_reload_failed`       | Candidate invalid, old catalog kept           |
| `automation.catalog_reload_started` | Reload requested, load beginning              |
| `automation.catalog_reloaded`       | Swap succeeded                                |
| `automation.catalog_reload_failed`  | Candidate invalid, old catalog kept           |
| `scripts.reload_started`            | Scripts reload requested                      |
| `scripts.reloaded`                  | Scripts reload acknowledged                   |
| `scripts.reload_failed`             | Scripts reload failed                         |

Event payload fields (all types):

- `target` - `"scenes"`, `"automations"`, or `"scripts"`
- `loaded_count` - number of assets loaded (on success)
- `duration_ms` - time taken for load + validation
- `errors` - array of `{ file, message }` objects (on failure)

---

## Runtime Semantics

### Scenes

- Scene executions already in progress continue using their loaded Lua context.
- New executions after the swap use the new catalog.
- Duplicate IDs or malformed files fail the candidate and block the swap.

### Automations

- Trigger matching switches to the new catalog only after a successful rebuild
  and runner update.
- Running automation executions are not forcibly killed.
- Persisted runtime state (cooldown, dedupe, resumable schedule) is preserved
  keyed by automation ID. State for IDs no longer in the catalog is left
  untouched (safe default).
- If the trigger type changes for the same ID, the new schedule behavior takes
  effect at the next evaluation cycle.

### Scripts

- For v1, script changes are guaranteed visible to new Lua executions after
  reload.
- No guarantee for execution contexts already in progress at reload time.
- Document this limitation clearly in `lua_runtime_guide.md`.

---

## Implementation Plan

### Phase 1 — Manual Reload Endpoints (MVP)

**Files to change:**

- `crates/scenes/src/lib.rs` - expose `reload_from_directory` that builds a
  candidate catalog and returns `Result<SceneCatalog, Vec<ReloadError>>`.
- `crates/automations/src/lib.rs` - expose `reload_from_directory` with the
  same pattern.
- `crates/core/src/event.rs` - add reload lifecycle event variants.
- `crates/api/src/main.rs`:
  - implement `POST /scenes/reload` handler.
  - implement `POST /automations/reload` handler.
  - wire new events into websocket serialization.
  - update diagnostics if relevant.
- `config/docs/api_reference.md` - document new endpoints.
- `config/docs/lua_runtime_guide.md` - add reload workflow section.

**Acceptance criteria:**

- Editing a Lua file and calling the reload endpoint applies changes immediately.
- A broken Lua file never replaces the active catalog.
- The API returns the full error list, not just the first failure.
- WebSocket emits `scene.catalog_reloaded` or `scene.catalog_reload_failed`.
- All existing tests pass.
- New tests cover success and failure paths.

### Phase 2 — Scripts Reload Semantics

**Files to change:**

- `crates/lua-host/src/lib.rs` - clarify and expose any module cache
  invalidation behavior.
- `crates/api/src/main.rs` - implement `POST /scripts/reload`.
- `config/docs/lua_runtime_guide.md` - document cache behavior explicitly.

**Acceptance criteria:**

- Script edits are visible to subsequent scene and automation executions after
  reload.
- Cache behavior is documented clearly for human and MCP callers.

### Phase 3 — Optional File-Watch Mode

**Files to change:**

- `crates/core/src/config.rs` - add optional `watch` boolean flags:
  - `[scenes] watch = false`
  - `[automations] watch = false`
  - `[scripts] watch = false`
- `crates/api/src/main.rs` - add debounced file-watcher startup when flags are
  enabled; reuse the same reload pipeline as the manual endpoints.
- `config/default.toml` - add `watch = false` defaults (explicitly opt-in).
- `config/docs/` - document dev vs prod watch behavior.

**Acceptance criteria:**

- With `watch = true`, saving a Lua file triggers reload automatically.
- With `watch = false` (default), behavior is identical to the current startup
  load.
- File-watch reload uses the same validation and atomic-swap guarantees as the
  manual endpoints.

---

## Error Handling Strategy

- Aggregate all parse/validation errors across all files in one pass; do not
  stop at the first failure.
- Include file path and concise error message per failure.
- Preserve the previous active catalog on any error.
- Structured log output includes:
  - `target`
  - `duration_ms`
  - `error_count`
  - first error summary as a top-level field for quick scanning

---

## Test Plan

### Unit tests

| Case                               | Target crate      |
|------------------------------------|-------------------|
| Valid scene set loads cleanly      | `smart-home-scenes` |
| Duplicate scene IDs fail           | `smart-home-scenes` |
| Malformed Lua file fails           | `smart-home-scenes` |
| Valid automation set loads cleanly | `smart-home-automations` |
| Duplicate automation IDs fail      | `smart-home-automations` |
| Candidate failure keeps old catalog | both              |

### API tests

| Case                                                       | Crate |
|------------------------------------------------------------|-------|
| `POST /scenes/reload` success returns `loaded_count`       | `api` |
| `POST /scenes/reload` failure returns error list           | `api` |
| `POST /automations/reload` success path                    | `api` |
| `POST /automations/reload` failure path                    | `api` |
| WS event `scene.catalog_reloaded` emitted on success       | `api` |
| WS event `scene.catalog_reload_failed` emitted on failure  | `api` |

### Integration behavior tests

| Case                                                                    |
|-------------------------------------------------------------------------|
| Execute scene before reload, edit Lua, reload, confirm new behavior     |
| Broken automation update does not affect currently running automations  |
| Running automation execution continues during reload boundary           |

---

## Security and Operations Notes

- Reload endpoints are write-equivalent operations. In production, protect them
  behind a reverse proxy auth layer or restrict to loopback access.
- Future: consider an `X-Admin-Token` header requirement configurable in
  `config/default.toml`.
- Future metrics counters (when telemetry layer is extended):
  - `reload_attempt_total{target}`
  - `reload_success_total{target}`
  - `reload_failure_total{target}`

---

## MCP / AI Authoring Workflow Fit

This is the primary motivation for hot reload.

Intended loop:

1. MCP tool writes or edits Lua file under `config/scenes/`,
   `config/automations/`, or `config/scripts/`.
2. MCP tool calls `POST /scenes/reload` or `POST /automations/reload`.
3. MCP tool reads structured response — success or detailed errors.
4. If errors, MCP tool corrects the file and retries.
5. MCP tool can execute a scene immediately via `POST /scenes/{id}/execute` to
   confirm behavior.

This loop works without a Rust toolchain and without process restart, making
it suitable for:

- end users editing Lua via MCP-enabled editors
- AI assistants iterating on automation logic
- containerised deployments where restarts are disruptive

---

## Recommended First Deliverable

Implement **Phase 1** only (manual reload for scenes + automations) as a single
focused change:

- Minimal risk to existing behavior.
- Immediate UX improvement for Lua authoring workflows.
- Clean foundation for scripts reload and optional file-watch mode.
- Fully testable without adding new dependencies.
