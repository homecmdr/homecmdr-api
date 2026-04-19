# Dashboard Template Guide

This project provides a **HomeCmdr API**. Dashboards are external clients that you build and own.

The reference example lives in `examples/dashboard-template/` and is the canonical starting point for users and AI / MCP agents building dashboards against this API.

## Design Philosophy

The API binary (`crates/api`) deliberately does not serve a dashboard. Keeping the API separate from any frontend means:

- The API surface stays stable and client-agnostic
- Dashboards can be built in any language or framework without touching the backend
- AI / MCP tooling can treat the HTTP + WebSocket contract as the stable interface and generate or modify dashboard code without needing to understand the Rust codebase

## The Reference Example

`examples/dashboard-template/` is a no-build, no-npm single-page dashboard using:

- **Alpine.js** for reactive state management and DOM templating
- **Vanilla CSS** with custom properties for theming
- **No build tooling** — serve the directory as static files

Structure:

```
examples/dashboard-template/
├── index.html              HTML shell + all card templates
├── css/
│   ├── base.css            Design tokens (colours, spacing, type scale)
│   ├── layout.css          App shell, header, responsive grid
│   └── components.css      All component styles
├── js/
│   ├── utils.js            Formatting helpers, URL utilities
│   ├── api.js              HTTP client — createApiClient()
│   ├── websocket.js        WS manager — createWebSocketManager()
│   └── app.js              Alpine component — homeCmdrApp()
└── README.md
```

## Running the Example Locally

Start the API:

```bash
cargo run -p api
```

Enable CORS for the origin you will serve the dashboard from (edit `config/default.toml`):

```toml
[api.cors]
enabled = true
allowed_origins = ["http://127.0.0.1:8080"]
```

Serve the dashboard (from the repository root):

```bash
python -m http.server 8080
```

Open:

```
http://127.0.0.1:8080/examples/dashboard-template/?api=http://127.0.0.1:3001&token=your-key
```

## Authentication

All API routes except `GET /health` and `GET /ready` require a `Bearer` token.

In the dashboard, the token is:
1. Read from the `?token=` query parameter on load, or
2. Entered via the setup screen and stored in `localStorage`

The HTTP client in `js/api.js` injects it as `Authorization: Bearer <token>` on every request. The WebSocket appends it as `?token=<token>` on the connection URL.

## API Contract Used by the Dashboard

### Initial data load

```
GET /rooms
GET /devices
GET /scenes
```

### Device commands

```
POST /devices/{id}/command
Content-Type: application/json

{ "capability": "power",      "action": "on" }
{ "capability": "brightness", "action": "set", "value": 75 }
{ "capability": "color_temperature", "action": "set", "value": { "value": 4000, "unit": "kelvin" } }
```

Capabilities present on a device are determined by the attributes in the `GET /devices` response. Only show controls for capabilities that exist.

### Scene execution

```
POST /scenes/{id}/execute
```

### Live event stream

```
WS /events?token=<token>
```

Event types the dashboard handles:

| Type | Action taken |
|------|-------------|
| `device.state_changed` | Re-fetch all devices |
| `device.added` | Re-fetch all devices |
| `device.removed` | Re-fetch all devices |
| Others | Appended to the Events feed |

## Weather / Sensor Devices

The `open_meteo` adapter ships with the project and provides read-only weather data. Its device IDs are:

- `open_meteo:temperature_outdoor`
- `open_meteo:wind_speed`
- `open_meteo:wind_direction`

Attribute shapes:

| Device | Attribute key | Value shape |
|--------|--------------|-------------|
| `temperature_outdoor` | `temperature_outdoor` | `{ value: float, unit: "celsius" }` |
| `wind_speed` | `wind_speed` | `{ value: float, unit: "km/h" }` |
| `wind_direction` | `wind_direction` | `integer` (degrees) |

## Controllable Devices (Lights, Switches, etc.)

The `elgato_lights` and `zigbee2mqtt` adapters provide controllable devices. No specific devices are bundled with the project — the dashboard discovers them at runtime via `GET /devices` and renders controls based on which capability attributes are present.

| Attribute present | Control shown |
|-------------------|--------------|
| `power` | Power toggle button |
| `brightness` | Slider 1–100 |
| `color_temperature` | Slider 2200–7000 K |

Device IDs follow the namespace format `{adapter_name}:{vendor_id}`.

## Guidance for AI / MCP Agents

When generating or extending a dashboard against this API:

1. Treat `config/docs/api_reference.md` as the authoritative API contract
2. Discover devices and their capabilities with `GET /devices` — do not hardcode capability assumptions except for known adapters
3. Use `GET /devices?ids=id1&ids=id2` (filtered) when the dashboard only needs specific devices
4. Subscribe to `WS /events` for live updates instead of polling
5. Normalise device attributes into local UI state before rendering — avoid computing display values inside templates
6. Keep adapter-specific assumptions isolated to known device IDs and attribute keys (e.g. open_meteo sensor IDs)
7. Add new API calls in `js/api.js`, new state in `js/app.js`, new card HTML in `index.html`, new styles in `css/components.css`

To verify the dashboard is working correctly after changes:
- `GET /health` should return 200
- `GET /devices` with a valid token should return the device list
- The WebSocket at `ws://127.0.0.1:3001/events?token=...` should connect and stream events

## CORS

The API does not allow cross-origin requests by default. For any dashboard served from a different origin, add that origin to `config/default.toml`:

```toml
[api.cors]
enabled = true
allowed_origins = ["http://127.0.0.1:8080"]
```

Origins must be bare (scheme + host + optional port) with no path, query, or trailing slash.
