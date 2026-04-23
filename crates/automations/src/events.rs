use std::collections::HashMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use homecmdr_core::event::Event;
use homecmdr_core::model::{AttributeValue, Attributes, DeviceId};
use homecmdr_core::registry::DeviceRegistry;

use crate::types::{AdapterLifecycleEvent, Automation, ThresholdTrigger, Trigger, TriggerContext};

// ── Attribute helpers ─────────────────────────────────────────────────────────

pub(crate) fn attribute_value_to_f64(value: &AttributeValue) -> Option<f64> {
    match value {
        AttributeValue::Integer(value) => Some(*value as f64),
        AttributeValue::Float(value) => Some(*value),
        AttributeValue::Object(fields) => match fields.get("value") {
            Some(AttributeValue::Integer(value)) => Some(*value as f64),
            Some(AttributeValue::Float(value)) => Some(*value),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn attribute_matches(
    value: &AttributeValue,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdTrigger>,
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

pub(crate) fn crossed_threshold(
    previous_value: Option<&AttributeValue>,
    current_value: &AttributeValue,
    threshold: &ThresholdTrigger,
) -> bool {
    let Some(previous_number) = previous_value.and_then(attribute_value_to_f64) else {
        return false;
    };
    let Some(current_number) = attribute_value_to_f64(current_value) else {
        return false;
    };

    if let Some(above) = threshold.above {
        if previous_number <= above && current_number > above {
            return true;
        }
    }
    if let Some(below) = threshold.below {
        if previous_number >= below && current_number < below {
            return true;
        }
    }

    false
}

// ── Event builders ────────────────────────────────────────────────────────────

pub(crate) fn automation_event_from_runtime_event(
    automation: &Automation,
    event: &Event,
) -> Option<AttributeValue> {
    match (&automation.trigger, event) {
        (
            Trigger::DeviceStateChange {
                device_id,
                attribute,
                equals,
                threshold,
                debounce_secs,
                duration_secs,
            },
            Event::DeviceStateChanged {
                id,
                attributes,
                previous_attributes,
            },
        ) => device_state_event_from_change(
            "device_state_change",
            device_id,
            attribute.as_deref(),
            equals.as_ref(),
            threshold.as_ref(),
            *debounce_secs,
            *duration_secs,
            &id.0,
            attributes,
            previous_attributes,
            None,
        ),
        (
            Trigger::WeatherState {
                device_id,
                attribute,
                equals,
                threshold,
                debounce_secs,
                duration_secs,
            },
            Event::DeviceStateChanged {
                id,
                attributes,
                previous_attributes,
            },
        ) => device_state_event_from_change(
            "weather_state",
            device_id,
            Some(attribute.as_str()),
            equals.as_ref(),
            threshold.as_ref(),
            *debounce_secs,
            *duration_secs,
            &id.0,
            attributes,
            previous_attributes,
            None,
        ),
        (
            Trigger::AdapterLifecycle { adapter, event },
            Event::AdapterStarted {
                adapter: started_adapter,
            },
        ) => {
            if *event != AdapterLifecycleEvent::Started {
                return None;
            }
            if adapter
                .as_deref()
                .is_some_and(|expected| expected != started_adapter)
            {
                return None;
            }

            Some(adapter_started_event(started_adapter))
        }
        (Trigger::SystemError { contains }, Event::SystemError { message }) => {
            if contains
                .as_deref()
                .is_some_and(|needle| !message.contains(needle))
            {
                return None;
            }

            Some(system_error_event(message))
        }
        _ => None,
    }
}

pub(crate) fn automation_event_from_registry_snapshot(
    automation: &Automation,
    registry: &DeviceRegistry,
    skipped: u64,
) -> Option<AttributeValue> {
    match &automation.trigger {
        Trigger::DeviceStateChange {
            device_id,
            attribute,
            equals,
            threshold,
            debounce_secs,
            duration_secs,
        } => {
            let device = registry.get(&DeviceId(device_id.clone()))?;
            device_state_event_from_snapshot(
                "device_state_change",
                &device.id.0,
                &device.attributes,
                attribute.as_deref(),
                equals.as_ref(),
                threshold.as_ref(),
                *debounce_secs,
                *duration_secs,
                skipped,
            )
        }
        Trigger::WeatherState {
            device_id,
            attribute,
            equals,
            threshold,
            debounce_secs,
            duration_secs,
        } => {
            let device = registry.get(&DeviceId(device_id.clone()))?;
            device_state_event_from_snapshot(
                "weather_state",
                &device.id.0,
                &device.attributes,
                Some(attribute.as_str()),
                equals.as_ref(),
                threshold.as_ref(),
                *debounce_secs,
                *duration_secs,
                skipped,
            )
        }
        _ => None,
    }
}

fn device_state_event_from_change(
    event_type: &str,
    expected_device_id: &str,
    attribute_name: Option<&str>,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdTrigger>,
    debounce_secs: Option<u64>,
    duration_secs: Option<u64>,
    actual_device_id: &str,
    attributes: &Attributes,
    previous_attributes: &Attributes,
    recovered_skipped: Option<u64>,
) -> Option<AttributeValue> {
    if actual_device_id != expected_device_id {
        return None;
    }

    if let Some(attribute_name) = attribute_name {
        let value = attributes.get(attribute_name)?;
        let previous_value = previous_attributes.get(attribute_name).cloned();
        let matches_now = attribute_matches(value, equals, threshold);
        let matched_before = previous_value
            .as_ref()
            .is_some_and(|previous| attribute_matches(previous, equals, threshold));

        if duration_secs.is_some() && !matches_now {
            return None;
        }

        if duration_secs.is_none() {
            if debounce_secs.is_some() {
                if !matches_now || matched_before {
                    return None;
                }
            } else if threshold.is_some() {
                if !crossed_threshold(previous_value.as_ref(), value, threshold?) {
                    return None;
                }
            } else if !matches_now {
                return None;
            }
        }

        return Some(device_state_change_event(
            event_type,
            actual_device_id,
            attributes,
            Some(attribute_name),
            Some(value.clone()),
            previous_value,
            debounce_secs,
            duration_secs,
            threshold,
            recovered_skipped,
        ));
    }

    Some(device_state_change_event(
        event_type,
        actual_device_id,
        attributes,
        None,
        None,
        None,
        debounce_secs,
        duration_secs,
        threshold,
        recovered_skipped,
    ))
}

fn device_state_event_from_snapshot(
    event_type: &str,
    device_id: &str,
    attributes: &Attributes,
    attribute_name: Option<&str>,
    equals: Option<&AttributeValue>,
    threshold: Option<&ThresholdTrigger>,
    debounce_secs: Option<u64>,
    duration_secs: Option<u64>,
    skipped: u64,
) -> Option<AttributeValue> {
    if let Some(attribute_name) = attribute_name {
        let value = attributes.get(attribute_name)?;
        if !attribute_matches(value, equals, threshold) {
            return None;
        }

        return Some(device_state_change_event(
            event_type,
            device_id,
            attributes,
            Some(attribute_name),
            Some(value.clone()),
            None,
            debounce_secs,
            duration_secs,
            threshold,
            Some(skipped),
        ));
    }

    Some(device_state_change_event(
        event_type,
        device_id,
        attributes,
        None,
        None,
        None,
        debounce_secs,
        duration_secs,
        threshold,
        Some(skipped),
    ))
}

fn device_state_change_event(
    event_type: &str,
    device_id: &str,
    attributes: &Attributes,
    attribute_name: Option<&str>,
    value: Option<AttributeValue>,
    previous_value: Option<AttributeValue>,
    debounce_secs: Option<u64>,
    duration_secs: Option<u64>,
    threshold: Option<&ThresholdTrigger>,
    recovered_skipped: Option<u64>,
) -> AttributeValue {
    let mut event = HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text(event_type.to_string()),
        ),
        (
            "device_id".to_string(),
            AttributeValue::Text(device_id.to_string()),
        ),
        (
            "attributes".to_string(),
            AttributeValue::Object(attributes.clone()),
        ),
    ]);

    if let Some(attribute_name) = attribute_name {
        event.insert(
            "attribute".to_string(),
            AttributeValue::Text(attribute_name.to_string()),
        );
    }

    if let Some(value) = value {
        event.insert("value".to_string(), value);
    }

    if let Some(previous_value) = previous_value {
        event.insert("previous_value".to_string(), previous_value);
    }

    if let Some(debounce_secs) = debounce_secs {
        event.insert(
            "debounce_secs".to_string(),
            AttributeValue::Integer(debounce_secs as i64),
        );
    }

    if let Some(duration_secs) = duration_secs {
        event.insert(
            "duration_secs".to_string(),
            AttributeValue::Integer(duration_secs as i64),
        );
    }

    if let Some(threshold) = threshold {
        let mut threshold_fields = HashMap::new();
        if let Some(above) = threshold.above {
            threshold_fields.insert("above".to_string(), AttributeValue::Float(above));
        }
        if let Some(below) = threshold.below {
            threshold_fields.insert("below".to_string(), AttributeValue::Float(below));
        }
        event.insert(
            "threshold".to_string(),
            AttributeValue::Object(threshold_fields),
        );
    }

    if let Some(skipped) = recovered_skipped {
        event.insert("recovered".to_string(), AttributeValue::Bool(true));
        event.insert(
            "skipped_events".to_string(),
            AttributeValue::Integer(skipped as i64),
        );
    }

    AttributeValue::Object(event)
}

pub(crate) fn adapter_started_event(adapter: &str) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text("adapter_lifecycle".to_string()),
        ),
        (
            "adapter".to_string(),
            AttributeValue::Text(adapter.to_string()),
        ),
        (
            "event".to_string(),
            AttributeValue::Text("started".to_string()),
        ),
    ]))
}

pub(crate) fn system_error_event(message: &str) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        (
            "type".to_string(),
            AttributeValue::Text("system_error".to_string()),
        ),
        (
            "message".to_string(),
            AttributeValue::Text(message.to_string()),
        ),
    ]))
}

pub(crate) fn scheduled_trigger_event(
    trigger: &Trigger,
    scheduled_at: DateTime<Utc>,
    trigger_context: TriggerContext,
) -> AttributeValue {
    let timezone = trigger_context
        .timezone
        .map(|timezone: Tz| timezone.name().to_string())
        .unwrap_or_else(|| "UTC".to_string());
    match trigger {
        Trigger::WallClock { hour, minute } => AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("wall_clock".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(scheduled_at.to_rfc3339()),
            ),
            ("hour".to_string(), AttributeValue::Integer(*hour as i64)),
            (
                "minute".to_string(),
                AttributeValue::Integer(*minute as i64),
            ),
            (
                "timezone".to_string(),
                AttributeValue::Text(timezone.clone()),
            ),
        ])),
        Trigger::Cron { expression, .. } => AttributeValue::Object(HashMap::from([
            ("type".to_string(), AttributeValue::Text("cron".to_string())),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(scheduled_at.to_rfc3339()),
            ),
            (
                "expression".to_string(),
                AttributeValue::Text(expression.clone()),
            ),
            (
                "timezone".to_string(),
                AttributeValue::Text(timezone.clone()),
            ),
        ])),
        Trigger::Sunrise { offset_mins } => AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("sunrise".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(scheduled_at.to_rfc3339()),
            ),
            (
                "offset_mins".to_string(),
                AttributeValue::Integer(*offset_mins),
            ),
            (
                "timezone".to_string(),
                AttributeValue::Text(timezone.clone()),
            ),
        ])),
        Trigger::Sunset { offset_mins } => AttributeValue::Object(HashMap::from([
            (
                "type".to_string(),
                AttributeValue::Text("sunset".to_string()),
            ),
            (
                "scheduled_at".to_string(),
                AttributeValue::Text(scheduled_at.to_rfc3339()),
            ),
            (
                "offset_mins".to_string(),
                AttributeValue::Integer(*offset_mins),
            ),
            ("timezone".to_string(), AttributeValue::Text(timezone)),
        ])),
        _ => AttributeValue::Object(HashMap::from([(
            "scheduled_at".to_string(),
            AttributeValue::Text(scheduled_at.to_rfc3339()),
        )])),
    }
}
