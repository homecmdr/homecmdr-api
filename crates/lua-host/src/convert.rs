// This module handles the translation layer between Lua values and HomeCmdr's
// internal `AttributeValue` type.
//
// Lua only knows about a handful of types: nil, boolean, number, string, and
// table.  HomeCmdr uses a richer enum (`AttributeValue`) that also carries
// intent (integer vs float, array vs object).  Every value that crosses the
// Lua boundary — going in or coming out — passes through one of the functions
// here.

use std::collections::HashMap;

use homecmdr_core::command::DeviceCommand;
use homecmdr_core::model::{
    AttributeValue, Device, DeviceGroup, DeviceKind, Metadata, Person, PersonState, Room,
};
use mlua::{Table, Value};

// Turns a Lua table like `{ capability = "power", action = "on" }` into a
// typed `DeviceCommand` that the runtime understands.
pub fn lua_table_to_command(table: &Table) -> mlua::Result<DeviceCommand> {
    Ok(DeviceCommand {
        capability: table.get("capability")?,
        action: table.get("action")?,
        value: match table.get::<Value>("value")? {
            Value::Nil => None,
            value => Some(lua_value_to_attribute(value)?),
        },
        transition_secs: match table.get::<Value>("transition_secs")? {
            Value::Nil => None,
            Value::Integer(v) => Some(v as f64),
            Value::Number(v) => Some(v),
            _ => return Err(mlua::Error::external("transition_secs must be a number")),
        },
    })
}

// Converts any Lua value into the equivalent `AttributeValue`.
// Lua tables are inspected to decide whether they look like an ordered array
// (sequential integer keys starting at 1) or a plain key-value object.
pub fn lua_value_to_attribute(value: Value) -> mlua::Result<AttributeValue> {
    match value {
        Value::Nil => Ok(AttributeValue::Null),
        Value::Boolean(value) => Ok(AttributeValue::Bool(value)),
        Value::Integer(value) => Ok(AttributeValue::Integer(value)),
        Value::Number(value) => Ok(AttributeValue::Float(value)),
        Value::String(value) => Ok(AttributeValue::Text(value.to_str()?.to_string())),
        Value::Table(table) => {
            if is_array_table(&table)? {
                let mut values = Vec::new();
                for value in table.sequence_values::<Value>() {
                    values.push(lua_value_to_attribute(value?)?);
                }
                return Ok(AttributeValue::Array(values));
            }

            let mut fields = HashMap::new();
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair?;
                let Value::String(key) = key else {
                    return Err(mlua::Error::external("lua object keys must be strings"));
                };
                fields.insert(key.to_str()?.to_string(), lua_value_to_attribute(value)?);
            }
            Ok(AttributeValue::Object(fields))
        }
        _ => Err(mlua::Error::external(
            "lua values must be nil, boolean, number, string, or table",
        )),
    }
}

// The reverse of `lua_value_to_attribute` — takes an `AttributeValue` coming
// back from the runtime and turns it into something Lua can work with.
pub fn attribute_to_lua_value(lua: &mlua::Lua, value: AttributeValue) -> mlua::Result<Value> {
    match value {
        AttributeValue::Null => Ok(Value::Nil),
        AttributeValue::Bool(value) => Ok(Value::Boolean(value)),
        AttributeValue::Integer(value) => Ok(Value::Integer(value)),
        AttributeValue::Float(value) => Ok(Value::Number(value)),
        AttributeValue::Text(value) => Ok(Value::String(lua.create_string(&value)?)),
        AttributeValue::Array(values) => {
            // Lua arrays use 1-based integer keys.
            let table = lua.create_table()?;
            for (index, value) in values.into_iter().enumerate() {
                table.set(index + 1, attribute_to_lua_value(lua, value)?)?;
            }
            Ok(Value::Table(table))
        }
        AttributeValue::Object(fields) => {
            let table = lua.create_table()?;
            for (key, value) in fields {
                table.set(key, attribute_to_lua_value(lua, value)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

// Returns true if the table looks like a Lua array: every key is a positive
// integer, they start at 1, and there are no gaps.
fn is_array_table(table: &Table) -> mlua::Result<bool> {
    let mut count = 0usize;

    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::Integer(index) = key else {
            return Ok(false);
        };
        if index <= 0 {
            return Ok(false);
        }
        count += 1;
    }

    for expected in 1..=count {
        if table.raw_get::<Value>(expected as i64)? == Value::Nil {
            return Ok(false);
        }
    }

    Ok(true)
}

// The following functions convert HomeCmdr model types into `AttributeValue`
// objects so they can be handed back to Lua scripts as plain tables.

// Converts a Device into a Lua-friendly table with its id, room, kind,
// attributes, metadata, and timestamps.
pub fn device_to_attribute_value(device: &Device) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        ("id".to_string(), AttributeValue::Text(device.id.0.clone())),
        (
            "room_id".to_string(),
            match &device.room_id {
                Some(room_id) => AttributeValue::Text(room_id.0.clone()),
                None => AttributeValue::Null,
            },
        ),
        (
            "kind".to_string(),
            AttributeValue::Text(device_kind_name(&device.kind).to_string()),
        ),
        (
            "attributes".to_string(),
            AttributeValue::Object(device.attributes.clone()),
        ),
        (
            "metadata".to_string(),
            metadata_to_attribute_value(&device.metadata),
        ),
        (
            "updated_at".to_string(),
            AttributeValue::Text(device.updated_at.to_rfc3339()),
        ),
        (
            "last_seen".to_string(),
            AttributeValue::Text(device.last_seen.to_rfc3339()),
        ),
    ]))
}

// Converts a Room into `{ id = "...", name = "..." }`.
pub fn room_to_attribute_value(room: &Room) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        ("id".to_string(), AttributeValue::Text(room.id.0.clone())),
        ("name".to_string(), AttributeValue::Text(room.name.clone())),
    ]))
}

// Converts a DeviceGroup into `{ id, name, members = [...] }`.
// `members` is an array of device id strings.
pub fn group_to_attribute_value(group: &DeviceGroup) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        ("id".to_string(), AttributeValue::Text(group.id.0.clone())),
        ("name".to_string(), AttributeValue::Text(group.name.clone())),
        (
            "members".to_string(),
            AttributeValue::Array(
                group
                    .members
                    .iter()
                    .map(|member| AttributeValue::Text(member.0.clone()))
                    .collect(),
            ),
        ),
    ]))
}

// Converts a Person into a table with id, name, picture, state, and location.
// The `state` field is a compact string like "home", "away", "zone:garden",
// or "room:kitchen" so it's easy to pattern-match in Lua.
pub fn person_to_attribute_value(person: &Person) -> AttributeValue {
    let state_tag = match &person.state {
        PersonState::Home => "home".to_string(),
        PersonState::Away => "away".to_string(),
        PersonState::Unknown => "unknown".to_string(),
        PersonState::Zone { zone_id } => format!("zone:{}", zone_id.0),
        PersonState::Room { room_id } => format!("room:{}", room_id.0),
    };
    AttributeValue::Object(HashMap::from([
        ("id".to_string(), AttributeValue::Text(person.id.0.clone())),
        ("name".to_string(), AttributeValue::Text(person.name.clone())),
        (
            "picture".to_string(),
            match &person.picture {
                Some(url) => AttributeValue::Text(url.clone()),
                None => AttributeValue::Null,
            },
        ),
        ("state".to_string(), AttributeValue::Text(state_tag)),
        (
            "state_source".to_string(),
            match &person.state_source {
                Some(id) => AttributeValue::Text(id.0.clone()),
                None => AttributeValue::Null,
            },
        ),
        (
            "latitude".to_string(),
            match person.latitude {
                Some(v) => AttributeValue::Float(v),
                None => AttributeValue::Null,
            },
        ),
        (
            "longitude".to_string(),
            match person.longitude {
                Some(v) => AttributeValue::Float(v),
                None => AttributeValue::Null,
            },
        ),
        (
            "updated_at".to_string(),
            AttributeValue::Text(person.updated_at.to_rfc3339()),
        ),
    ]))
}

// Converts a device's Metadata into a table containing its source adapter,
// an optional accuracy score, and any vendor-specific extra fields.
pub fn metadata_to_attribute_value(metadata: &Metadata) -> AttributeValue {
    let mut fields = HashMap::from([(
        "source".to_string(),
        AttributeValue::Text(metadata.source.clone()),
    )]);

    fields.insert(
        "accuracy".to_string(),
        match metadata.accuracy {
            Some(value) => AttributeValue::Float(value),
            None => AttributeValue::Null,
        },
    );
    fields.insert(
        "vendor_specific".to_string(),
        AttributeValue::Object(
            metadata
                .vendor_specific
                .iter()
                .map(|(key, value)| (key.clone(), json_value_to_attribute_value(value)))
                .collect(),
        ),
    );

    AttributeValue::Object(fields)
}

// Bridges serde_json values (used in vendor-specific metadata) into the
// AttributeValue type used everywhere else in HomeCmdr.
pub fn json_value_to_attribute_value(value: &serde_json::Value) -> AttributeValue {
    match value {
        serde_json::Value::Null => AttributeValue::Null,
        serde_json::Value::Bool(value) => AttributeValue::Bool(*value),
        serde_json::Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                AttributeValue::Integer(integer)
            } else if let Some(float) = value.as_f64() {
                AttributeValue::Float(float)
            } else {
                AttributeValue::Null
            }
        }
        serde_json::Value::String(value) => AttributeValue::Text(value.clone()),
        serde_json::Value::Array(values) => {
            AttributeValue::Array(values.iter().map(json_value_to_attribute_value).collect())
        }
        serde_json::Value::Object(fields) => AttributeValue::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), json_value_to_attribute_value(value)))
                .collect(),
        ),
    }
}

// Maps DeviceKind enum variants to the plain string names that Lua scripts see.
fn device_kind_name(kind: &DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Sensor => "sensor",
        DeviceKind::Light => "light",
        DeviceKind::Switch => "switch",
        DeviceKind::Virtual => "virtual",
    }
}
