use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use homecmdr_core::model::{AttributeValue, DeviceId, RoomId};
use homecmdr_core::runtime::Runtime;

use crate::events::attribute_value_to_f64;
use crate::schedule::solar_event_time;
use crate::types::{
    Automation, Condition, SolarConditionPoint, SolarEventKind, ThresholdCondition, TriggerContext,
};

pub(crate) async fn first_failed_condition(
    automation: &Automation,
    runtime: &Runtime,
    event: &AttributeValue,
    now: DateTime<Utc>,
    trigger_context: TriggerContext,
) -> Option<String> {
    for (index, condition) in automation.conditions.iter().enumerate() {
        if let Err(error) =
            evaluate_condition(condition, runtime, event, now, trigger_context).await
        {
            return Some(format!("condition {} failed: {error}", index + 1));
        }
    }
    None
}

pub(crate) async fn evaluate_condition(
    condition: &Condition,
    runtime: &Runtime,
    _event: &AttributeValue,
    now: DateTime<Utc>,
    trigger_context: TriggerContext,
) -> Result<()> {
    match condition {
        Condition::DeviceState {
            device_id,
            attribute,
            equals,
            threshold,
        } => {
            let device = runtime
                .registry()
                .get(&DeviceId(device_id.clone()))
                .with_context(|| format!("device '{device_id}' not found"))?;
            let value = device
                .attributes
                .get(attribute)
                .with_context(|| format!("attribute '{attribute}' missing on '{device_id}'"))?;
            if !condition_value_matches(value, equals.as_ref(), threshold.as_ref()) {
                bail!("device_state did not match")
            }
            Ok(())
        }
        Condition::Presence {
            device_id,
            attribute,
            equals,
        } => {
            let device = runtime
                .registry()
                .get(&DeviceId(device_id.clone()))
                .with_context(|| format!("device '{device_id}' not found"))?;
            let value = device
                .attributes
                .get(attribute)
                .with_context(|| format!("attribute '{attribute}' missing on '{device_id}'"))?;
            if value != equals {
                bail!("presence did not match")
            }
            Ok(())
        }
        Condition::TimeWindow { start, end } => {
            let timezone = trigger_context
                .timezone
                .with_context(|| "automation timezone is not configured")?;
            let local_time = now.with_timezone(&timezone).time();
            let in_window = if start <= end {
                local_time >= *start && local_time <= *end
            } else {
                local_time >= *start || local_time <= *end
            };
            if !in_window {
                bail!("time_window did not match")
            }
            Ok(())
        }
        Condition::RoomState {
            room_id,
            min_devices,
            max_devices,
        } => {
            let count = runtime
                .registry()
                .list_devices_in_room(&RoomId(room_id.clone()))
                .len();
            if let Some(min_devices) = min_devices {
                if count < *min_devices {
                    bail!("room_state below min_devices")
                }
            }
            if let Some(max_devices) = max_devices {
                if count > *max_devices {
                    bail!("room_state above max_devices")
                }
            }
            Ok(())
        }
        Condition::SunPosition { after, before } => {
            let date = now.date_naive();
            if let Some(after) = after {
                let after_time = solar_condition_time(date, after, trigger_context)
                    .with_context(|| "sun_position after point could not be resolved")?;
                if now < after_time {
                    bail!("sun_position after point not reached")
                }
            }
            if let Some(before) = before {
                let before_time = solar_condition_time(date, before, trigger_context)
                    .with_context(|| "sun_position before point could not be resolved")?;
                if now > before_time {
                    bail!("sun_position before point passed")
                }
            }
            Ok(())
        }
    }
}

fn condition_value_matches(
    value: &AttributeValue,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdCondition>,
) -> bool {
    if let Some(expected) = equals {
        return value == expected;
    }
    if let Some(threshold) = threshold {
        let Some(number) = attribute_value_to_f64(value) else {
            return false;
        };
        if let Some(above) = threshold.above {
            if number <= above {
                return false;
            }
        }
        if let Some(below) = threshold.below {
            if number >= below {
                return false;
            }
        }
    }
    true
}

fn solar_condition_time(
    date: NaiveDate,
    point: &SolarConditionPoint,
    trigger_context: TriggerContext,
) -> Option<DateTime<Utc>> {
    let sunrise = matches!(point.event, SolarEventKind::Sunrise);
    solar_event_time(
        date,
        trigger_context.latitude?,
        trigger_context.longitude?,
        sunrise,
        point.offset_mins,
    )
}

pub(crate) fn parse_conditions(module: &mlua::Table, path: &Path) -> Result<Vec<Condition>> {
    let conditions = module
        .get::<Option<mlua::Table>>("conditions")
        .map_err(|error| {
            anyhow::anyhow!(
                "automation file {} has invalid optional field 'conditions': {error}",
                path.display()
            )
        })?;
    let Some(conditions) = conditions else {
        return Ok(Vec::new());
    };

    let mut parsed = Vec::new();
    for value in conditions.sequence_values::<mlua::Value>() {
        let value = value.map_err(|error| anyhow::anyhow!(error.to_string()))?;
        parsed.push(parse_condition(value, path)?);
    }
    Ok(parsed)
}

fn parse_condition(value: mlua::Value, path: &Path) -> Result<Condition> {
    let mlua::Value::Table(table) = value else {
        bail!(
            "automation file {} condition entries must be tables",
            path.display()
        );
    };
    let condition_type = table.get::<String>("type").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} condition is missing string field 'type': {error}",
            path.display()
        )
    })?;

    match condition_type.as_str() {
        "device_state" => {
            let device_id = table.get::<String>("device_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} device_state condition requires 'device_id': {error}",
                    path.display()
                )
            })?;
            let attribute = table.get::<String>("attribute").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} device_state condition requires 'attribute': {error}",
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
                        "automation file {} device_state condition field 'equals' is invalid: {error}",
                        path.display()
                    ))
                }
            };
            let threshold = parse_threshold_condition(&table, path, "device_state")?;
            validate_condition_value_match(path, "device_state", equals.as_ref(), threshold.as_ref())?;
            Ok(Condition::DeviceState {
                device_id,
                attribute,
                equals,
                threshold,
            })
        }
        "presence" => {
            let device_id = table.get::<String>("device_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} presence condition requires 'device_id': {error}",
                    path.display()
                )
            })?;
            let attribute = table
                .get::<Option<String>>("attribute")
                .map_err(|error| {
                    anyhow::anyhow!(
                        "automation file {} presence condition field 'attribute' is invalid: {error}",
                        path.display()
                    )
                })?
                .unwrap_or_else(|| "presence".to_string());
            let equals = match table.get::<mlua::Value>("equals") {
                Ok(mlua::Value::Nil) => AttributeValue::Bool(true),
                Ok(value) => homecmdr_lua_host::lua_value_to_attribute(value)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?,
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "automation file {} presence condition field 'equals' is invalid: {error}",
                        path.display()
                    ))
                }
            };
            Ok(Condition::Presence {
                device_id,
                attribute,
                equals,
            })
        }
        "time_window" => {
            let start = parse_clock_time(
                &table.get::<String>("start").map_err(|error| {
                    anyhow::anyhow!(
                        "automation file {} time_window condition requires 'start': {error}",
                        path.display()
                    )
                })?,
                path,
                "start",
            )?;
            let end = parse_clock_time(
                &table.get::<String>("end").map_err(|error| {
                    anyhow::anyhow!(
                        "automation file {} time_window condition requires 'end': {error}",
                        path.display()
                    )
                })?,
                path,
                "end",
            )?;
            Ok(Condition::TimeWindow { start, end })
        }
        "room_state" => {
            let room_id = table.get::<String>("room_id").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} room_state condition requires 'room_id': {error}",
                    path.display()
                )
            })?;
            let min_devices = table.get::<Option<usize>>("min_devices").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} room_state condition field 'min_devices' is invalid: {error}",
                    path.display()
                )
            })?;
            let max_devices = table.get::<Option<usize>>("max_devices").map_err(|error| {
                anyhow::anyhow!(
                    "automation file {} room_state condition field 'max_devices' is invalid: {error}",
                    path.display()
                )
            })?;
            if min_devices.is_none() && max_devices.is_none() {
                bail!(
                    "automation file {} room_state condition requires 'min_devices' or 'max_devices'",
                    path.display()
                );
            }
            Ok(Condition::RoomState {
                room_id,
                min_devices,
                max_devices,
            })
        }
        "sun_position" => {
            let after = parse_solar_condition_point(&table, path, "after")?;
            let before = parse_solar_condition_point(&table, path, "before")?;
            if after.is_none() && before.is_none() {
                bail!(
                    "automation file {} sun_position condition requires 'after' or 'before'",
                    path.display()
                );
            }
            Ok(Condition::SunPosition { after, before })
        }
        _ => bail!(
            "automation file {} has unsupported condition type '{}'; supported types are device_state, presence, time_window, room_state, and sun_position",
            path.display(),
            condition_type
        ),
    }
}

fn parse_threshold_condition(
    table: &mlua::Table,
    path: &Path,
    condition_type: &str,
) -> Result<Option<ThresholdCondition>> {
    let above = table.get::<Option<f64>>("above").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} {} condition field 'above' is invalid: {error}",
            path.display(),
            condition_type
        )
    })?;
    let below = table.get::<Option<f64>>("below").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} {} condition field 'below' is invalid: {error}",
            path.display(),
            condition_type
        )
    })?;

    if above.is_none() && below.is_none() {
        return Ok(None);
    }

    Ok(Some(ThresholdCondition { above, below }))
}

fn validate_condition_value_match(
    path: &Path,
    condition_type: &str,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdCondition>,
) -> Result<()> {
    if equals.is_some() && threshold.is_some() {
        bail!(
            "automation file {} {} condition cannot combine 'equals' with 'above'/'below'",
            path.display(),
            condition_type
        );
    }
    if equals.is_none() && threshold.is_none() {
        bail!(
            "automation file {} {} condition requires 'equals' or 'above'/'below'",
            path.display(),
            condition_type
        );
    }
    Ok(())
}

fn parse_clock_time(value: &str, path: &Path, field: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(value, "%H:%M").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} time value '{}' for '{}' is invalid: {}",
            path.display(),
            value,
            field,
            error
        )
    })
}

fn parse_solar_condition_point(
    table: &mlua::Table,
    path: &Path,
    prefix: &str,
) -> Result<Option<SolarConditionPoint>> {
    let Some(event_name) = table
        .get::<Option<String>>(prefix)
        .map_err(|error| {
            anyhow::anyhow!(
                "automation file {} sun_position condition field '{}' is invalid: {error}",
                path.display(),
                prefix
            )
        })?
    else {
        return Ok(None);
    };

    let offset_field = format!("{}_offset_mins", prefix);
    let offset_mins = table
        .get::<Option<i64>>(offset_field.as_str())
        .map_err(|error| {
            anyhow::anyhow!(
                "automation file {} sun_position condition field '{}' is invalid: {error}",
                path.display(),
                offset_field
            )
        })?
        .unwrap_or(0);

    let event = match event_name.as_str() {
        "sunrise" => SolarEventKind::Sunrise,
        "sunset" => SolarEventKind::Sunset,
        other => {
            bail!(
                "automation file {} sun_position condition field '{}' has unsupported value '{}'; supported values are sunrise and sunset",
                path.display(),
                prefix,
                other
            )
        }
    };

    Ok(Some(SolarConditionPoint { event, offset_mins }))
}
