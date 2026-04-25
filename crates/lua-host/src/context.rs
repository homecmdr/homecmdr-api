// This module defines the `ctx` object that every Lua script receives.
// When you write `ctx:command(...)` or `ctx:get_device(...)` in a scene or
// automation, those calls land here.  Think of it as the control panel that
// bridges your Lua code and the rest of HomeCmdr.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use homecmdr_core::invoke::InvokeRequest;
use homecmdr_core::model::{AttributeValue, DeviceId, PersonId};
use homecmdr_core::person_registry::PersonRegistry;
use homecmdr_core::runtime::Runtime;
use mlua::{Table, UserData, UserDataMethods, Value};
use tokio::runtime::Handle;
use tokio::task::block_in_place;

use crate::convert::{
    attribute_to_lua_value, device_to_attribute_value, group_to_attribute_value,
    lua_table_to_command, lua_value_to_attribute, person_to_attribute_value,
    room_to_attribute_value,
};

// The outcome of a single `ctx:command(...)` or `ctx:command_group(...)`
// call — which device was targeted, whether it worked, and an optional
// human-readable message when something went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExecutionResult {
    pub target: String,
    pub status: &'static str,
    pub message: Option<String>,
}

// A small interior-mutable list that collects every command result while the
// script is running.  Using Rc<RefCell<…>> keeps it cheap to clone alongside
// the context (Lua needs the context to be Clone).
#[derive(Clone, Default)]
struct ExecutionResults(Rc<RefCell<Vec<CommandExecutionResult>>>);

impl ExecutionResults {
    fn push(&self, result: CommandExecutionResult) {
        self.0.borrow_mut().push(result);
    }

    fn take(&self) -> Vec<CommandExecutionResult> {
        self.0.borrow().clone()
    }
}

// The context object handed to Lua scripts.  It holds a reference to the
// live HomeCmdr runtime (for devices, commands, plugins) and an optional
// reference to the person registry (for presence-based automations).
#[derive(Clone)]
pub struct LuaExecutionContext {
    runtime: Arc<Runtime>,
    execution_results: ExecutionResults,
    person_registry: Option<Arc<PersonRegistry>>,
}

impl LuaExecutionContext {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            execution_results: ExecutionResults::default(),
            person_registry: None,
        }
    }

    // Attach person-tracking so scripts can call ctx:get_person / ctx:any_person_home etc.
    pub fn with_person_registry(mut self, person_registry: Arc<PersonRegistry>) -> Self {
        self.person_registry = Some(person_registry);
        self
    }

    // Called after the script finishes to collect all the command outcomes.
    pub fn into_results(self) -> Vec<CommandExecutionResult> {
        self.execution_results.take()
    }
}

// Everything below is what Lua scripts can actually call on `ctx`.
// Each `add_method` registers one method by name.
impl UserData for LuaExecutionContext {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // ctx:command("device:id", { capability = "power", action = "on" })
        // Sends a single command to one device and records whether it worked.
        methods.add_method(
            "command",
            |_, this, (device_id, command): (String, Table)| {
                let command = lua_table_to_command(&command)?;
                command.validate().map_err(mlua::Error::external)?;

                let result = match block_in_place(|| {
                    Handle::current().block_on(
                        this.runtime
                            .command_device(&DeviceId(device_id.clone()), command),
                    )
                }) {
                    Ok(true) => CommandExecutionResult {
                        target: device_id,
                        status: "ok",
                        message: None,
                    },
                    Ok(false) => CommandExecutionResult {
                        target: device_id,
                        status: "unsupported",
                        message: Some("device commands are not implemented".to_string()),
                    },
                    Err(error) => CommandExecutionResult {
                        target: device_id,
                        status: "error",
                        message: Some(error.to_string()),
                    },
                };
                this.execution_results.push(result);

                Ok(())
            },
        );

        // ctx:invoke("plugin:action", { ... })
        // Calls a plugin and returns its response as a Lua value.
        // Used for things like asking an LLM a question or querying a web API.
        methods.add_method("invoke", |lua, this, (target, payload): (String, Value)| {
            let payload = lua_value_to_attribute(payload)?;
            let response = block_in_place(|| {
                Handle::current().block_on(this.runtime.invoke(InvokeRequest {
                    target: target.clone(),
                    payload,
                }))
            })
            .map_err(mlua::Error::external)?
            .ok_or_else(|| {
                mlua::Error::external(format!("invoke target '{target}' is not supported"))
            })?;

            attribute_to_lua_value(&lua, response.value)
        });

        // ctx:get_device("device:id")
        // Returns a table with the device's id, kind, room, attributes, etc.
        // Returns nil if the device isn't registered.
        methods.add_method("get_device", |lua, this, device_id: String| {
            let Some(device) = this.runtime.registry().get(&DeviceId(device_id)) else {
                return Ok(Value::Nil);
            };

            attribute_to_lua_value(&lua, device_to_attribute_value(&device))
        });

        // ctx:list_devices()
        // Returns an array of all known devices.
        methods.add_method("list_devices", |lua, this, (): ()| {
            let devices = this
                .runtime
                .registry()
                .list()
                .into_iter()
                .map(|device| device_to_attribute_value(&device))
                .collect();

            attribute_to_lua_value(&lua, AttributeValue::Array(devices))
        });

        // ctx:get_room("room_id")
        // Returns a table with the room's id and name, or nil if not found.
        methods.add_method("get_room", |lua, this, room_id: String| {
            let Some(room) = this
                .runtime
                .registry()
                .get_room(&homecmdr_core::model::RoomId(room_id))
            else {
                return Ok(Value::Nil);
            };

            attribute_to_lua_value(&lua, room_to_attribute_value(&room))
        });

        // ctx:list_rooms()
        // Returns an array of all rooms.
        methods.add_method("list_rooms", |lua, this, (): ()| {
            let rooms = this
                .runtime
                .registry()
                .list_rooms()
                .into_iter()
                .map(|room| room_to_attribute_value(&room))
                .collect();

            attribute_to_lua_value(&lua, AttributeValue::Array(rooms))
        });

        // ctx:list_room_devices("room_id")
        // Returns an array of every device that belongs to the given room.
        methods.add_method("list_room_devices", |lua, this, room_id: String| {
            let devices = this
                .runtime
                .registry()
                .list_devices_in_room(&homecmdr_core::model::RoomId(room_id))
                .into_iter()
                .map(|device| device_to_attribute_value(&device))
                .collect();

            attribute_to_lua_value(&lua, AttributeValue::Array(devices))
        });

        // ctx:get_group("group_id")
        // Returns a table with the group's id, name, and member device ids.
        // Returns nil if the group doesn't exist.
        methods.add_method("get_group", |lua, this, group_id: String| {
            let Some(group) = this
                .runtime
                .registry()
                .get_group(&homecmdr_core::model::GroupId(group_id))
            else {
                return Ok(Value::Nil);
            };

            attribute_to_lua_value(&lua, group_to_attribute_value(&group))
        });

        // ctx:list_adapter_devices("open_meteo")
        // Returns every device whose id starts with "<adapter>:" — the same
        // as GET /devices?adapter=open_meteo in the HTTP API.  Useful for
        // reading all sensor values from one adapter in a single call.
        // Returns an empty array if no devices match (no error for unknown names).
        methods.add_method("list_adapter_devices", |lua, this, adapter: String| {
            let devices = this
                .runtime
                .registry()
                .list_devices_for_adapter(&adapter)
                .into_iter()
                .map(|device| device_to_attribute_value(&device))
                .collect();

            attribute_to_lua_value(&lua, AttributeValue::Array(devices))
        });

        // ctx:get_devices({ "open_meteo:humidity", "open_meteo:temperature" })
        // Fetches a specific set of devices by id in one call, preserving the
        // order you requested.  Duplicate ids are silently collapsed and unknown
        // ids are omitted — the same behaviour as GET /devices?ids=...&ids=...
        methods.add_method("get_devices", |lua, this, ids: Vec<String>| {
            let mut seen = std::collections::HashSet::new();
            let devices: Vec<AttributeValue> = ids
                .into_iter()
                .filter(|id| seen.insert(id.clone()))
                .filter_map(|id| {
                    this.runtime
                        .registry()
                        .get(&DeviceId(id))
                        .map(|d| device_to_attribute_value(&d))
                })
                .collect();

            attribute_to_lua_value(&lua, AttributeValue::Array(devices))
        });

        // ctx:list_groups()
        // Returns an array of all device groups.
        methods.add_method("list_groups", |lua, this, (): ()| {
            let groups = this
                .runtime
                .registry()
                .list_groups()
                .into_iter()
                .map(|group| group_to_attribute_value(&group))
                .collect();

            attribute_to_lua_value(&lua, AttributeValue::Array(groups))
        });

        // ctx:list_group_devices("group_id")
        // Returns an array of every device that belongs to the given group.
        methods.add_method("list_group_devices", |lua, this, group_id: String| {
            let devices = this
                .runtime
                .registry()
                .list_devices_in_group(&homecmdr_core::model::GroupId(group_id))
                .into_iter()
                .map(|device| device_to_attribute_value(&device))
                .collect();

            attribute_to_lua_value(&lua, AttributeValue::Array(devices))
        });

        // ctx:command_group("group_id", { capability = "power", action = "off" })
        // Sends the same command to every device in a group in one call.
        // Errors immediately if the group doesn't exist.
        methods.add_method(
            "command_group",
            |_, this, (group_id, command): (String, Table)| {
                let command = lua_table_to_command(&command)?;
                command.validate().map_err(mlua::Error::external)?;

                let registry_group_id = homecmdr_core::model::GroupId(group_id.clone());
                if this
                    .runtime
                    .registry()
                    .get_group(&registry_group_id)
                    .is_none()
                {
                    return Err(mlua::Error::external(format!(
                        "group '{group_id}' not found"
                    )));
                }

                let devices = this
                    .runtime
                    .registry()
                    .list_devices_in_group(&registry_group_id);

                // Fan the command out to every member and record each result.
                for device in devices {
                    let device_id_str = device.id.0.clone();
                    let result = match block_in_place(|| {
                        Handle::current()
                            .block_on(this.runtime.command_device(&device.id, command.clone()))
                    }) {
                        Ok(true) => CommandExecutionResult {
                            target: device_id_str,
                            status: "ok",
                            message: None,
                        },
                        Ok(false) => CommandExecutionResult {
                            target: device_id_str,
                            status: "unsupported",
                            message: Some("device commands are not implemented".to_string()),
                        },
                        Err(error) => CommandExecutionResult {
                            target: device_id_str,
                            status: "error",
                            message: Some(error.to_string()),
                        },
                    };
                    this.execution_results.push(result);
                }

                Ok(())
            },
        );

        // ctx:log("info", "message", { optional = "fields" })
        // Writes a structured log entry from inside a script.
        // Level must be one of: trace, debug, info, warn, error.
        methods.add_method(
            "log",
            |_, _, (level, message, fields): (String, String, Option<Value>)| {
                let fields = match fields {
                    Some(Value::Nil) | None => None,
                    Some(value) => Some(lua_value_to_attribute(value)?),
                };

                match level.as_str() {
                    "trace" => tracing::trace!(message = %message, fields = ?fields, "lua log"),
                    "debug" => tracing::debug!(message = %message, fields = ?fields, "lua log"),
                    "info" => tracing::info!(message = %message, fields = ?fields, "lua log"),
                    "warn" => tracing::warn!(message = %message, fields = ?fields, "lua log"),
                    "error" => tracing::error!(message = %message, fields = ?fields, "lua log"),
                    _ => {
                        return Err(mlua::Error::external(format!(
                            "unsupported log level '{level}'; expected trace, debug, info, warn, or error"
                        )))
                    }
                }

                Ok(())
            },
        );

        // ctx:sleep(seconds)
        // Pauses the script for the given number of seconds (max 3600).
        // Useful for adding a delay between two actions in the same script.
        methods.add_method("sleep", |_, _, secs: f64| {
            if !(0.0..=3600.0).contains(&secs) {
                return Err(mlua::Error::external(
                    "sleep: seconds must be between 0 and 3600",
                ));
            }
            block_in_place(|| {
                Handle::current()
                    .block_on(tokio::time::sleep(std::time::Duration::from_secs_f64(secs)))
            });
            Ok(())
        });

        // ctx:get_person("person_id")
        // Returns a table with the person's id, name, presence state, and location.
        // Returns nil if no person registry is configured or the id isn't found.
        methods.add_method("get_person", |lua, this, person_id: String| {
            let Some(registry) = &this.person_registry else {
                return Ok(Value::Nil);
            };
            let person = block_in_place(|| {
                Handle::current().block_on(registry.get_person(&PersonId(person_id)))
            });
            match person {
                Some(p) => attribute_to_lua_value(&lua, person_to_attribute_value(&p)),
                None => Ok(Value::Nil),
            }
        });

        // ctx:list_persons()
        // Returns an array of all tracked people.  Empty array if no registry.
        methods.add_method("list_persons", |lua, this, (): ()| {
            let Some(registry) = &this.person_registry else {
                return attribute_to_lua_value(&lua, AttributeValue::Array(Vec::new()));
            };
            let persons = block_in_place(|| Handle::current().block_on(registry.list_persons()));
            let values = persons
                .into_iter()
                .map(|p| person_to_attribute_value(&p))
                .collect();
            attribute_to_lua_value(&lua, AttributeValue::Array(values))
        });

        // ctx:all_persons_away()
        // Returns true only when every tracked person is marked away.
        // Handy for "nobody is home" automations.
        methods.add_method("all_persons_away", |_, this, (): ()| {
            let Some(registry) = &this.person_registry else {
                return Ok(false);
            };
            Ok(block_in_place(|| {
                Handle::current().block_on(registry.all_persons_away())
            }))
        });

        // ctx:any_person_home()
        // Returns true when at least one tracked person is home.
        methods.add_method("any_person_home", |_, this, (): ()| {
            let Some(registry) = &this.person_registry else {
                return Ok(false);
            };
            Ok(block_in_place(|| {
                Handle::current().block_on(registry.any_person_home())
            }))
        });
    }
}
