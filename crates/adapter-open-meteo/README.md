# adapter-open-meteo

Open-Meteo weather adapter for HomeCmdr.

## What It Does

This adapter polls the Open-Meteo HTTP API and publishes weather devices into the runtime registry.

Current canonical devices:

- `open_meteo:temperature_outdoor`
- `open_meteo:wind_speed`
- `open_meteo:wind_direction`

All devices are `DeviceKind::Sensor`.

## Canonical Capabilities Used

- `temperature_outdoor`
- `wind_speed`
- `wind_direction`

This adapter is read-only and does not implement device commands.

## Config

Example:

```toml
[adapters.open_meteo]
enabled = true
latitude = 51.5
longitude = -0.1
poll_interval_secs = 90
```

Optional internal/test field:

- `base_url`

Validation:

- `poll_interval_secs >= 60`

## Implementation Notes

- factory registration is in `src/lib.rs`
- state is fetched from `/v1/forecast?current_weather=true`
- `updated_at` is preserved when the state is identical
- `room_id` is preserved across refreshes

## Testing

Run:

```bash
cargo test -p adapter-open-meteo
```

This crate includes tests for:

- successful polling normalization
- retry/recovery behavior
- stable `updated_at` with refreshed `last_seen`
- factory config validation
