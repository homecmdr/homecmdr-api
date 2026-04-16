# Smart Home

Smart-home automation platform implemented as a Rust workspace with a core runtime, an Open-Meteo adapter, and an HTTP/WebSocket API.

## Prerequisites

- Rust stable toolchain with `cargo`
- No extra system packages are required for the MVP workspace
- The Open-Meteo adapter uses `reqwest` with `rustls-tls`, so OpenSSL is not required

## Run

```bash
cargo run -p api -- --config config/default.toml
```

The API binds to `127.0.0.1:3000`.

## Configuration

Default configuration file: `config/default.toml`

```toml
[runtime]
event_bus_capacity = 1024

[logging]
level = "info"

[adapters.open_meteo]
enabled = true
latitude = 51.5
longitude = -0.1
poll_interval_secs = 300
```

Config reference:

- `runtime.event_bus_capacity: usize = 1024`
  Capacity of the broadcast event bus.
- `logging.level: string = "info"`
  One of `trace`, `debug`, `info`, `warn`, `error`.
- `adapters.open_meteo.enabled: bool = true`
  Enables the Open-Meteo adapter.
- `adapters.open_meteo.latitude: f64 = 51.5`
  Forecast latitude.
- `adapters.open_meteo.longitude: f64 = -0.1`
  Forecast longitude.
- `adapters.open_meteo.poll_interval_secs: u64 = 300`
  Poll interval in seconds. Must be `>= 60`.

## API

All responses are JSON.

### HTTP Endpoints

- `GET /health`

Response:

```json
{"status":"ok"}
```

- `GET /adapters`

Response:

```json
[{"name":"open_meteo","status":"running"}]
```

- `GET /devices`

Response: array of `Device` objects.

- `GET /devices/{id}`

Response: a single `Device` object or `404`.

- `POST /devices/{id}/command`

Response: `501 Not Implemented`.

Error format:

```json
{"error":"<message>"}
```

### Device Schema

Example device:

```json
{
  "id": "open_meteo:temperature_outdoor",
  "kind": "sensor",
  "attributes": {
    "temperature_outdoor": {
      "value": 18.25,
      "unit": "celsius"
    }
  },
  "metadata": {
    "source": "open_meteo",
    "location": "outdoor",
    "accuracy": null,
    "vendor_specific": {}
  },
  "updated_at": "2026-04-16T00:00:00Z"
}
```

### WebSocket

- `GET /events`

Frames are JSON objects.

Device update frame:

```json
{"type":"device.state_changed","id":"open_meteo:temperature_outdoor","state":{"temperature_outdoor":{"value":18.25,"unit":"celsius"}}}
```

Lag/error frame:

```json
{"type":"system.error","message":"subscriber lagged, events dropped"}
```

Other event frame types currently emitted:

- `adapter.started`
- `device.removed`
- `system.error`

## Open-Meteo Devices

The current Open-Meteo adapter publishes:

- `open_meteo:temperature_outdoor` with `temperature_outdoor: {value, unit}`
- `open_meteo:wind_speed` with `wind_speed: {value, unit}`
- `open_meteo:wind_direction` with `wind_direction: integer`

Canonical weather/outdoor capabilities defined in `crates/core/src/capability.rs`:

- `temperature_outdoor`
- `temperature_apparent`
- `temperature_high`
- `temperature_low`
- `wind_speed`
- `wind_direction`
- `wind_gust`
- `rainfall`
- `uv_index`
- `weather_condition`
- `cloud_coverage`
- `visibility`

All Open-Meteo devices use:

- `kind = sensor`
- `metadata.source = "open_meteo"`
- `metadata.location = "outdoor"`

## Implementing a New Adapter

Every adapter must implement `smart_home_core::adapter::Adapter` and follow these rules:

1. All device IDs must be namespaced as `"{adapter_name}:{vendor_id}"`.
2. Adapters must not share state with other adapters.
3. Adapters must publish `Event::AdapterStarted` at the beginning of `run`.
4. Adapters must publish `Event::SystemError` on non-recoverable failures before returning `Err`.
5. Reconnection backoff must be exponential with a maximum of 60 seconds.

## Verification

Run the workspace checks from `smart-home/`:

```bash
cargo check --workspace
cargo clippy --workspace -- -D warnings
cargo test --workspace
```
