use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use homecmdr_core::invoke::InvokeRequest;
use homecmdr_core::model::{AttributeValue, DeviceId};
use homecmdr_core::runtime::Runtime;
use mlua::{Table, UserData, UserDataMethods, Value};
use tokio::runtime::Handle;
use tokio::task::block_in_place;

use crate::convert::{
    attribute_to_lua_value, device_to_attribute_value, group_to_attribute_value,
    lua_table_to_command, lua_value_to_attribute, room_to_attribute_value,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExecutionResult {
    pub target: String,
    pub status: &'static str,
    pub message: Option<String>,
}

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

#[derive(Clone)]
pub struct LuaExecutionContext {
    runtime: Arc<Runtime>,
    execution_results: ExecutionResults,
}

impl LuaExecutionContext {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            execution_results: ExecutionResults::default(),
        }
    }

    pub fn into_results(self) -> Vec<CommandExecutionResult> {
        self.execution_results.take()
    }
}

impl UserData for LuaExecutionContext {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
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

        methods.add_method("get_device", |lua, this, device_id: String| {
            let Some(device) = this.runtime.registry().get(&DeviceId(device_id)) else {
                return Ok(Value::Nil);
            };

            attribute_to_lua_value(&lua, device_to_attribute_value(&device))
        });

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
    }
}
