//! Trigger parsing and classification.
//!
//! Each automation Lua file contains a `trigger` table that describes what
//! should cause the automation to run.  This module parses that table into the
//! internal [`Trigger`] enum and provides small helpers used by the runner to
//! classify triggers.
//!
//! The three trigger categories decide which background loop handles them:
//! - **Event-bus** (`device_state_change`, `weather_state`, `adapter_lifecycle`,
//!   `system_error`) — watched in `triggers/device.rs`
//! - **Interval** — ticked in `triggers/interval.rs`
//! - **Scheduled** (`wall_clock`, `cron`, `sunrise`, `sunset`) — driven by
//!   `triggers/scheduled.rs`

pub(crate) mod device;
pub(crate) mod interval;
pub(crate) mod scheduled;

use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Result};
use chrono::Utc;
use cron::Schedule;
use homecmdr_core::model::AttributeValue;

use crate::types::{
    AdapterLifecycleEvent, Automation, ThresholdTrigger, Trigger,
};

// ── parse_trigger ─────────────────────────────────────────────────────────────
// Reads the `trigger` table from the Lua automation module and converts it into
// the internal `Trigger` enum.  Returns a descriptive error — including the
// file path — for any unknown type or malformed field.

/// Parse the `trigger` value returned by a Lua automation file into the
/// internal [`Trigger`] enum.
pub(crate) fn parse_trigger(value: mlua::Value, path: &Path) -> Result<Trigger> {
    let mlua::Value::Table(table) = value else {
        bail!(
            "automation file {} field 'trigger' must be a table",
            path.display()
        );
    };

    let trigger_type = table.get::<String>("type").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} trigger is missing string field 'type': {error}",
            path.display()
        )
    })?;

    match trigger_type.as_str() {
        "device_state_change" => {
            let device_id = table.get::<String>("device_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} device_state_change trigger requires 'device_id': {error}",
                    path.display()
                )
            })?;
            let attribute = table.get::<Option<String>>("attribute").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'attribute' is invalid: {error}",
                    path.display()
                )
            })?;
            let equals = match table.get::<mlua::Value>("equals") {
                Ok(mlua::Value::Nil) => None,
                Ok(value) => Some(
                    homecmdr_lua_host::lua_value_to_attribute(value)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?,
                ),
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "automation file {} trigger field 'equals' is invalid: {error}",
                        path.display()
                    ))
                }
            };
            let threshold = parse_threshold_trigger(&table, path, "device_state_change")?;
            let debounce_secs = table.get::<Option<u64>>("debounce_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'debounce_secs' is invalid: {error}",
                    path.display()
                )
            })?;
            let duration_secs = table.get::<Option<u64>>("duration_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'duration_secs' is invalid: {error}",
                    path.display()
                )
            })?;

            validate_extended_device_trigger(
                path,
                "device_state_change",
                attribute.as_deref(),
                equals.as_ref(),
                threshold.as_ref(),
                debounce_secs,
                duration_secs,
            )?;

            Ok(Trigger::DeviceStateChange {
                device_id,
                attribute,
                equals,
                threshold,
                debounce_secs,
                duration_secs,
            })
        }
        "weather_state" => {
            let device_id = table.get::<String>("device_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} weather_state trigger requires 'device_id': {error}",
                    path.display()
                )
            })?;
            let attribute = table.get::<String>("attribute").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} weather_state trigger requires string 'attribute': {error}",
                    path.display()
                )
            })?;
            let equals = match table.get::<mlua::Value>("equals") {
                Ok(mlua::Value::Nil) => None,
                Ok(value) => Some(
                    homecmdr_lua_host::lua_value_to_attribute(value)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?,
                ),
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "automation file {} trigger field 'equals' is invalid: {error}",
                        path.display()
                    ))
                }
            };
            let threshold = parse_threshold_trigger(&table, path, "weather_state")?;
            let debounce_secs = table.get::<Option<u64>>("debounce_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'debounce_secs' is invalid: {error}",
                    path.display()
                )
            })?;
            let duration_secs = table.get::<Option<u64>>("duration_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'duration_secs' is invalid: {error}",
                    path.display()
                )
            })?;

            validate_extended_device_trigger(
                path,
                "weather_state",
                Some(attribute.as_str()),
                equals.as_ref(),
                threshold.as_ref(),
                debounce_secs,
                duration_secs,
            )?;

            Ok(Trigger::WeatherState {
                device_id,
                attribute,
                equals,
                threshold,
                debounce_secs,
                duration_secs,
            })
        }
        "adapter_lifecycle" => {
            let adapter = table.get::<Option<String>>("adapter").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'adapter' is invalid: {error}",
                    path.display()
                )
            })?;
            let event = match table.get::<String>("event").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} adapter_lifecycle trigger requires string 'event': {error}",
                    path.display()
                )
            })?.as_str() {
                "started" => AdapterLifecycleEvent::Started,
                other => {
                    bail!(
                        "automation file {} adapter_lifecycle trigger has unsupported event '{}'; supported events are started",
                        path.display(),
                        other
                    )
                }
            };

            Ok(Trigger::AdapterLifecycle { adapter, event })
        }
        "system_error" => {
            let contains = table.get::<Option<String>>("contains").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} trigger field 'contains' is invalid: {error}",
                    path.display()
                )
            })?;

            Ok(Trigger::SystemError { contains })
        }
        "wall_clock" => {
            let hour = table.get::<u32>("hour").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} wall_clock trigger requires integer 'hour': {error}",
                    path.display()
                )
            })?;
            let minute = table.get::<u32>("minute").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} wall_clock trigger requires integer 'minute': {error}",
                    path.display()
                )
            })?;

            if hour > 23 {
                bail!(
                    "automation file {} wall_clock trigger 'hour' must be between 0 and 23",
                    path.display()
                );
            }
            if minute > 59 {
                bail!(
                    "automation file {} wall_clock trigger 'minute' must be between 0 and 59",
                    path.display()
                );
            }

            Ok(Trigger::WallClock { hour, minute })
        }
        "cron" => {
            let expression = table.get::<String>("expression").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} cron trigger requires string 'expression': {error}",
                    path.display()
                )
            })?;
            let schedule = Schedule::from_str(&expression).map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} cron trigger has invalid 'expression': {error}",
                    path.display()
                )
            })?;

            if schedule.after(&Utc::now()).next().is_none() {
                bail!(
                    "automation file {} cron trigger expression does not produce future occurrences",
                    path.display()
                );
            }

            Ok(Trigger::Cron {
                expression,
                schedule,
            })
        }
        "sunrise" => {
            let offset_mins = table
                .get::<Option<i64>>("offset_mins")
                .map_err(|error| {
                    anyhow::anyhow!(
                        "automation file {} sunrise trigger field 'offset_mins' is invalid: {error}",
                        path.display()
                    )
                })?
                .unwrap_or(0);

            Ok(Trigger::Sunrise { offset_mins })
        }
        "sunset" => {
            let offset_mins = table
                .get::<Option<i64>>("offset_mins")
                .map_err(|error| {
                    anyhow::anyhow!(
                        "automation file {} sunset trigger field 'offset_mins' is invalid: {error}",
                        path.display()
                    )
                })?
                .unwrap_or(0);

            Ok(Trigger::Sunset { offset_mins })
        }
        "interval" => {
            let every_secs = table.get::<u64>("every_secs").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} interval trigger requires positive 'every_secs': {error}",
                    path.display()
                )
            })?;
            if every_secs == 0 {
                bail!(
                    "automation file {} interval trigger 'every_secs' must be > 0",
                    path.display()
                );
            }

            Ok(Trigger::Interval { every_secs })
        }
        _ => bail!(
            "automation file {} has unsupported trigger type '{}'; supported types are device_state_change, weather_state, adapter_lifecycle, system_error, wall_clock, cron, sunrise, sunset, and interval",
            path.display(),
            trigger_type
        ),
    }
}

// ── trigger helpers ───────────────────────────────────────────────────────────
// Small classification and parsing utilities shared across this module and the
// runner.

/// Returns `true` if `automation`'s trigger is driven by the event bus rather
/// than a timer.
pub(crate) fn trigger_uses_event_bus(automation: &Automation) -> bool {
    matches!(
        automation.trigger,
        Trigger::DeviceStateChange { .. }
            | Trigger::WeatherState { .. }
            | Trigger::AdapterLifecycle { .. }
            | Trigger::SystemError { .. }
    )
}

/// Return the string name of `trigger`'s type as it appears in Lua files.
pub(crate) fn trigger_type_name(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::DeviceStateChange { .. } => "device_state_change",
        Trigger::WeatherState { .. } => "weather_state",
        Trigger::AdapterLifecycle { .. } => "adapter_lifecycle",
        Trigger::SystemError { .. } => "system_error",
        Trigger::WallClock { .. } => "wall_clock",
        Trigger::Cron { .. } => "cron",
        Trigger::Sunrise { .. } => "sunrise",
        Trigger::Sunset { .. } => "sunset",
        Trigger::Interval { .. } => "interval",
    }
}

/// Parse optional `above` and `below` fields from a trigger table into a
/// [`ThresholdTrigger`].  Returns `None` if neither field is present.
pub(crate) fn parse_threshold_trigger(
    table: &mlua::Table,
    path: &Path,
    trigger_type: &str,
) -> Result<Option<ThresholdTrigger>> {
    let above = table.get::<Option<f64>>("above").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} {} trigger field 'above' is invalid: {error}",
            path.display(),
            trigger_type
        )
    })?;
    let below = table.get::<Option<f64>>("below").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} {} trigger field 'below' is invalid: {error}",
            path.display(),
            trigger_type
        )
    })?;

    if above.is_none() && below.is_none() {
        return Ok(None);
    }

    Ok(Some(ThresholdTrigger { above, below }))
}

/// Validate the combination of `attribute`, `equals`, `threshold`,
/// `debounce_secs`, and `duration_secs` for a device or weather trigger.
/// Returns an error if the combination is not allowed (e.g. threshold without
/// an attribute, or both `equals` and `above`/`below`).
pub(crate) fn validate_extended_device_trigger(
    path: &Path,
    trigger_type: &str,
    attribute: Option<&str>,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdTrigger>,
    debounce_secs: Option<u64>,
    duration_secs: Option<u64>,
) -> Result<()> {
    if threshold.is_some() && equals.is_some() {
        bail!(
            "automation file {} {} trigger cannot combine 'equals' with 'above'/'below'",
            path.display(),
            trigger_type
        );
    }

    if threshold.is_none()
        && equals.is_none()
        && attribute.is_some()
        && debounce_secs.is_none()
        && duration_secs.is_none()
    {
        return Ok(());
    }

    if (threshold.is_some() || debounce_secs.is_some() || duration_secs.is_some())
        && attribute.is_none()
    {
        bail!(
            "automation file {} {} trigger requires 'attribute' when using threshold, debounce, or duration options",
            path.display(),
            trigger_type
        );
    }

    if debounce_secs == Some(0) {
        bail!(
            "automation file {} {} trigger field 'debounce_secs' must be > 0",
            path.display(),
            trigger_type
        );
    }

    if duration_secs == Some(0) {
        bail!(
            "automation file {} {} trigger field 'duration_secs' must be > 0",
            path.display(),
            trigger_type
        );
    }

    Ok(())
}
