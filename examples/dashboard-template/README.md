# HomeCmdr Dashboard — Reference Example

This directory contains a fully functional reference dashboard for the HomeCmdr API.

It is an **external client** — intentionally separate from the API binary. The project ships the API; you own the dashboard.

## File Structure

```
examples/dashboard-template/
├── index.html              Entry point and all HTML templates
├── css/
│   ├── base.css            Design tokens (colours, spacing, radius, typography)
│   ├── layout.css          App shell, header, nav, responsive grid
│   └── components.css      Cards, buttons, sliders, badges, event feed
├── js/
│   ├── utils.js            Formatting helpers and URL utilities
│   ├── api.js              Authenticated HTTP client (createApiClient)
│   ├── websocket.js        WebSocket manager with exponential-backoff reconnect
│   └── app.js              Alpine.js component registration (homeCmdrApp)
└── README.md               This file
```

No build step. No npm. No bundler. Open the folder, serve it, done.

## Prerequisites

- The HomeCmdr API running (`cargo run -p api`)
- Any static file server (Python, Node, Caddy, nginx, etc.)

## Running the Dashboard

### Step 1 — Start the API

```bash
cargo run -p api
```

The API binds to `http://127.0.0.1:3001` by default (see `config/default.toml`).

### Step 2 — Enable CORS for the dashboard origin

The dashboard is served from a different origin than the API, so you must allow it.
Add your dashboard origin to `config/default.toml`:

```toml
[api.cors]
enabled = true
allowed_origins = ["http://127.0.0.1:8080"]
```

Restart the API after changing config.

### Step 3 — Serve the dashboard

From the **repository root**:

```bash
python -m http.server 8080
```

Or with Node:

```bash
npx serve . --port 8080
```

### Step 4 — Open the dashboard

```
http://127.0.0.1:8080/examples/dashboard-template/
```

You will be prompted for the API base URL and a bearer token.

**Tip:** pass them in the URL to skip the setup screen on every reload:

```
http://127.0.0.1:8080/examples/dashboard-template/?api=http://127.0.0.1:3001&token=your-key
```

The token is stored in `localStorage` after the first successful connection.

## What the Dashboard Shows

| Tab | Content |
|-----|---------|
| **Devices** | All devices grouped by room. Shows power toggle, brightness slider, colour temperature slider, and attribute badges depending on the capabilities each device exposes. |
| **Weather** | Read-only sensor cards from the `open_meteo` adapter (ships with the project). Temperature, wind speed, wind direction. |
| **Scenes** | All Lua scenes loaded by the API. Click or press Run to execute. |
| **Events** | Live WebSocket event feed. Newest events at the top. |

## API Contract Used

| Purpose | Endpoint |
|---------|----------|
| Load rooms | `GET /rooms` |
| Load all devices | `GET /devices` |
| Load scenes | `GET /scenes` |
| Send a device command | `POST /devices/{id}/command` |
| Execute a scene | `POST /scenes/{id}/execute` |
| Live event stream | `WS /events?token=...` |

Full API reference: `config/docs/api_reference.md`

## Extending the Dashboard

### Add a new sensor card (Weather tab)

1. Enable the device in the adapter config in `config/default.toml`
2. Add its ID to `WEATHER_DEVICE_IDS` in `js/app.js`
3. Add a card block in the Weather tab section of `index.html` following the existing pattern

### Add a new device control

1. Add a command method to the returned object in `js/app.js` (see `togglePower`, `setBrightness`)
2. Add a corresponding `sendCommand` call via `js/api.js`
3. Add an `x-if` control block inside the device card template in `index.html`
4. Style it in `css/components.css`

### Change the colour scheme

All colours are CSS custom properties in `css/base.css` under `:root`. Change the values there — no other file needs to change.

### Add a new tab

1. Add a nav button in the `<nav class="app-nav">` block in `index.html`
2. Add a `<div role="tabpanel" x-show="activeTab === 'your-tab'">` section
3. Add any new state / data loading in `js/app.js`

## Guidance for AI / MCP Agents

This template is designed to be readable and extendable by agentic tooling.

Key facts:
- All API calls go through `createApiClient()` in `js/api.js` — add new endpoints there
- All state lives in the Alpine component in `js/app.js` — add new properties and methods there
- Card HTML lives in `index.html` — find the relevant tab section and add a new `<template x-for>` or `<template x-if>` block
- All CSS custom properties (colours, spacing) are in `css/base.css` `:root` — safe to modify without touching layout or component files

To discover what devices and capabilities are available on a running instance:
```
GET /devices        — full device list with attributes
GET /rooms          — room list
GET /adapters       — adapter status and config
```

See `config/docs/dashboard_template_guide.md` for the full guide.
