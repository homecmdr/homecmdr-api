use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use smart_home_core::command::DeviceCommand;
use smart_home_core::invoke::InvokeRequest;
use smart_home_core::model::{AttributeValue, Device, DeviceId, DeviceKind, Metadata, Room};
use smart_home_core::runtime::Runtime;
use tokio::runtime::Handle;
use tokio::task::block_in_place;

pub const DEFAULT_MAX_INSTRUCTIONS: u64 = 5_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionMode {
    Parallel { max: usize },
    Single,
    Queued { max: usize },
    Restart,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        ExecutionMode::Parallel { max: 8 }
    }
}

/// Parse an execution mode from a Lua value.
///
/// `default_max` is used as the concurrent-execution ceiling when the Lua
/// script specifies `"parallel"` or `"queued"` as a plain string (without a
/// table with an explicit `max` key).  Pass `8` for the historical default.
pub fn parse_execution_mode(value: mlua::Value, default_max: usize) -> Result<ExecutionMode> {
    match value {
        mlua::Value::Nil => Ok(ExecutionMode::Parallel { max: default_max }),
        mlua::Value::String(s) => {
            let mode_str = s.to_str().map_err(|e| anyhow::anyhow!("{e}"))?;
            match mode_str.as_ref() {
                "parallel" => Ok(ExecutionMode::Parallel { max: default_max }),
                "single" => Ok(ExecutionMode::Single),
                "queued" => Ok(ExecutionMode::Queued { max: default_max }),
                "restart" => Ok(ExecutionMode::Restart),
                other => anyhow::bail!(
                    "unknown execution mode '{other}'; expected parallel, single, queued, or restart"
                ),
            }
        }
        mlua::Value::Table(table) => {
            let mode_type: String = table
                .get("type")
                .map_err(|e| anyhow::anyhow!("mode table is missing string field 'type': {e}"))?;
            match mode_type.as_str() {
                "parallel" => {
                    let max = table
                        .get::<Option<usize>>("max")
                        .map_err(|e| anyhow::anyhow!("mode 'max' field is invalid: {e}"))?
                        .unwrap_or(default_max);
                    Ok(ExecutionMode::Parallel { max })
                }
                "single" => Ok(ExecutionMode::Single),
                "queued" => {
                    let max = table
                        .get::<Option<usize>>("max")
                        .map_err(|e| anyhow::anyhow!("mode 'max' field is invalid: {e}"))?
                        .unwrap_or(default_max);
                    Ok(ExecutionMode::Queued { max })
                }
                "restart" => Ok(ExecutionMode::Restart),
                other => anyhow::bail!("unknown execution mode type '{other}'"),
            }
        }
        _ => anyhow::bail!("mode must be a string or table"),
    }
}

pub fn install_execution_hook(lua: &Lua, max_instructions: u64, cancel: Arc<AtomicBool>) {
    let count = Arc::new(AtomicU64::new(0));
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(10_000),
        move |_lua, _debug| {
            if cancel.load(Ordering::Relaxed) {
                return Err(mlua::Error::runtime("execution cancelled"));
            }
            let new_count = count.fetch_add(10_000, Ordering::Relaxed) + 10_000;
            if new_count >= max_instructions {
                return Err(mlua::Error::runtime("execution compute limit exceeded"));
            }
            Ok(mlua::VmState::Continue)
        },
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExecutionResult {
    pub target: String,
    pub status: &'static str,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LuaRuntimeOptions {
    pub scripts_root: Option<PathBuf>,
    pub max_instructions: u64,
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for LuaRuntimeOptions {
    fn default() -> Self {
        Self {
            scripts_root: None,
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
            cancel: None,
        }
    }
}

#[derive(Clone)]
pub struct LuaExecutionContext {
    runtime: Arc<Runtime>,
    execution_results: ExecutionResults,
}

#[derive(Clone, Default)]
struct ExecutionResults(Rc<RefCell<Vec<CommandExecutionResult>>>);

#[derive(Clone)]
struct ScriptLoader {
    root: PathBuf,
}

impl ExecutionResults {
    fn push(&self, result: CommandExecutionResult) {
        self.0.borrow_mut().push(result);
    }

    fn take(&self) -> Vec<CommandExecutionResult> {
        self.0.borrow().clone()
    }
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
                .get_room(&smart_home_core::model::RoomId(room_id))
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
                .list_devices_in_room(&smart_home_core::model::RoomId(room_id))
                .into_iter()
                .map(|device| device_to_attribute_value(&device))
                .collect();

            attribute_to_lua_value(&lua, AttributeValue::Array(devices))
        });

        methods.add_method("get_group", |lua, this, group_id: String| {
            let Some(group) = this
                .runtime
                .registry()
                .get_group(&smart_home_core::model::GroupId(group_id))
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
                .list_devices_in_group(&smart_home_core::model::GroupId(group_id))
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

                let registry_group_id = smart_home_core::model::GroupId(group_id.clone());
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

fn device_to_attribute_value(device: &Device) -> AttributeValue {
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

fn room_to_attribute_value(room: &Room) -> AttributeValue {
    AttributeValue::Object(HashMap::from([
        ("id".to_string(), AttributeValue::Text(room.id.0.clone())),
        ("name".to_string(), AttributeValue::Text(room.name.clone())),
    ]))
}

fn group_to_attribute_value(group: &smart_home_core::model::DeviceGroup) -> AttributeValue {
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

fn metadata_to_attribute_value(metadata: &Metadata) -> AttributeValue {
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

fn json_value_to_attribute_value(value: &serde_json::Value) -> AttributeValue {
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

fn device_kind_name(kind: &DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Sensor => "sensor",
        DeviceKind::Light => "light",
        DeviceKind::Switch => "switch",
        DeviceKind::Virtual => "virtual",
    }
}

impl ScriptLoader {
    fn install(&self, lua: &Lua) -> Result<()> {
        let package: Table = lua
            .globals()
            .get("package")
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let loader = self.clone();

        let searcher = lua
            .create_function(move |lua, module_name: String| {
                match loader.load_module(lua, &module_name) {
                    Ok(value) => {
                        let loaded = value;
                        let module_loader = lua.create_function(move |_, ()| Ok(loaded.clone()))?;
                        Ok(Value::Function(module_loader))
                    }
                    Err(error) => Ok(Value::String(lua.create_string(&error.to_string())?)),
                }
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        let searchers: Table = package
            .get::<Table>("searchers")
            .or_else(|_| package.get::<Table>("loaders"))
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        let len = searchers.raw_len();
        for index in (2..=len).rev() {
            let value: Value = searchers
                .raw_get(index)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            searchers
                .raw_set(index + 1, value)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        }
        searchers
            .raw_set(2, searcher)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        Ok(())
    }

    fn load_module(&self, lua: &Lua, module_name: &str) -> mlua::Result<Value> {
        let module_path =
            resolve_script_module_path(&self.root, module_name).map_err(mlua::Error::external)?;
        let source = fs::read_to_string(&module_path).map_err(mlua::Error::external)?;
        let chunk_name = format!("@{}", module_path.display());

        lua.load(&source)
            .set_name(&chunk_name)
            .eval::<Value>()
            .map_err(|error| {
                mlua::Error::external(format!(
                    "failed to load script module '{}': {}",
                    module_name, error
                ))
            })
    }
}

pub fn prepare_lua(lua: &Lua, options: &LuaRuntimeOptions) -> Result<()> {
    if let Some(root) = &options.scripts_root {
        ScriptLoader { root: root.clone() }.install(lua)?;
    }

    if let Some(cancel) = &options.cancel {
        install_execution_hook(lua, options.max_instructions, cancel.clone());
    }

    Ok(())
}

pub fn evaluate_module(
    lua: &Lua,
    source: &str,
    path_name: &str,
    options: &LuaRuntimeOptions,
) -> Result<Table> {
    prepare_lua(lua, options)?;

    let value = lua
        .load(source)
        .set_name(path_name)
        .eval::<Value>()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    match value {
        Value::Table(table) => Ok(table),
        _ => anyhow::bail!("lua module must return a table"),
    }
}

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

pub fn attribute_to_lua_value(lua: &Lua, value: AttributeValue) -> mlua::Result<Value> {
    match value {
        AttributeValue::Null => Ok(Value::Nil),
        AttributeValue::Bool(value) => Ok(Value::Boolean(value)),
        AttributeValue::Integer(value) => Ok(Value::Integer(value)),
        AttributeValue::Float(value) => Ok(Value::Number(value)),
        AttributeValue::Text(value) => Ok(Value::String(lua.create_string(&value)?)),
        AttributeValue::Array(values) => {
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

fn resolve_script_module_path(root: &Path, module_name: &str) -> Result<PathBuf> {
    if module_name.trim().is_empty() {
        anyhow::bail!("script module name must not be empty");
    }

    let mut path = root.to_path_buf();
    for part in module_name.split('.') {
        if part.is_empty() {
            anyhow::bail!("script module name '{module_name}' is invalid");
        }

        let component_path = Path::new(part);
        if component_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            anyhow::bail!("script module name '{module_name}' is invalid");
        }

        path.push(part);
    }
    path.set_extension("lua");

    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to access scripts directory {}", root.display()))?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("script module '{}' was not found", module_name))?;

    if !canonical_path.starts_with(&canonical_root) {
        anyhow::bail!(
            "script module '{}' is outside the scripts directory",
            module_name
        );
    }

    Ok(canonical_path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use smart_home_core::adapter::Adapter;
    use smart_home_core::bus::EventBus;
    use smart_home_core::command::DeviceCommand;
    use smart_home_core::model::{
        AttributeValue, Device, DeviceGroup, DeviceId, DeviceKind, GroupId, Metadata, Room, RoomId,
    };
    use smart_home_core::registry::DeviceRegistry;
    use smart_home_core::runtime::{Runtime, RuntimeConfig};

    fn temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("smart-home-lua-host-{unique}"));
        fs::create_dir_all(&path).expect("create temp scripts dir");
        path
    }

    #[test]
    fn resolves_namespaced_script_module_paths() {
        let root = temp_dir();
        fs::create_dir_all(root.join("vision")).expect("create nested dir");
        fs::write(root.join("vision/ollama.lua"), "return {} ").expect("write script");

        let resolved = resolve_script_module_path(&root, "vision.ollama").expect("resolve path");
        assert!(resolved.ends_with("vision/ollama.lua"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn context_can_read_devices_and_rooms_from_registry() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        runtime
            .registry()
            .upsert_room(Room {
                id: RoomId("kitchen".to_string()),
                name: "Kitchen".to_string(),
            })
            .await;
        runtime
            .registry()
            .upsert(Device {
                id: DeviceId("test:device".to_string()),
                room_id: Some(RoomId("kitchen".to_string())),
                kind: DeviceKind::Light,
                attributes: HashMap::from([(
                    "brightness".to_string(),
                    AttributeValue::Integer(25),
                )]),
                metadata: Metadata {
                    source: "test".to_string(),
                    accuracy: Some(0.9),
                    vendor_specific: HashMap::from([(
                        "label".to_string(),
                        serde_json::json!("Desk Lamp"),
                    )]),
                },
                updated_at: chrono::Utc::now(),
                last_seen: chrono::Utc::now(),
            })
            .await
            .expect("device upsert succeeds");

        let lua = Lua::new();
        lua.globals()
            .set("ctx", LuaExecutionContext::new(runtime.clone()))
            .expect("ctx is installed");

        let device = lua
            .load(
                r#"
                local device = ctx:get_device("test:device")
                assert(device.id == "test:device")
                assert(device.room_id == "kitchen")
                assert(device.attributes.brightness == 25)
                assert(device.metadata.vendor_specific.label == "Desk Lamp")
                return device
                "#,
            )
            .eval::<Table>()
            .expect("device query succeeds");
        assert_eq!(device.get::<String>("kind").expect("device kind"), "light");

        let room_devices = lua
            .load(
                r#"
                local rooms = ctx:list_rooms()
                assert(#rooms == 1)
                local devices = ctx:list_room_devices("kitchen")
                assert(#devices == 1)
                return devices
                "#,
            )
            .eval::<Table>()
            .expect("room query succeeds");
        let first_device: Table = room_devices.get(1).expect("first room device");
        assert_eq!(
            first_device.get::<String>("id").expect("device id"),
            "test:device"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn context_log_accepts_structured_fields() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let lua = Lua::new();
        lua.globals()
            .set("ctx", LuaExecutionContext::new(runtime))
            .expect("ctx is installed");

        lua.load(
            r#"
            ctx:log("info", "automation checkpoint", {
                automation_id = "test",
                attempts = 2,
            })
            "#,
        )
        .exec()
        .expect("structured log call succeeds");
    }

    struct AnyTestAdapter;

    #[async_trait::async_trait]
    impl Adapter for AnyTestAdapter {
        fn name(&self) -> &str {
            "test"
        }

        async fn run(&self, _registry: DeviceRegistry, _bus: EventBus) -> anyhow::Result<()> {
            std::future::pending::<()>().await;
            Ok(())
        }

        async fn command(
            &self,
            device_id: &DeviceId,
            command: DeviceCommand,
            registry: DeviceRegistry,
        ) -> anyhow::Result<bool> {
            let mut device = registry.get(device_id).expect("device exists");
            // Store the capability as an attribute so tests can verify dispatch.
            let value = command
                .value
                .unwrap_or(AttributeValue::Text(command.action.clone()));
            device.attributes.insert(command.capability, value);
            registry
                .upsert(device)
                .await
                .expect("registry update succeeds");
            Ok(true)
        }
    }

    fn bare_device(id: &str) -> Device {
        Device {
            id: DeviceId(id.to_string()),
            room_id: None,
            kind: DeviceKind::Light,
            attributes: HashMap::from([(
                "power".to_string(),
                AttributeValue::Text("on".to_string()),
            )]),
            metadata: Metadata {
                source: "test".to_string(),
                accuracy: None,
                vendor_specific: HashMap::new(),
            },
            updated_at: chrono::Utc::now(),
            last_seen: chrono::Utc::now(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn command_group_fans_out_to_all_members() {
        let runtime = Arc::new(Runtime::new(
            vec![Box::new(AnyTestAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));

        runtime
            .registry()
            .upsert(bare_device("test:left"))
            .await
            .expect("left device upserted");
        runtime
            .registry()
            .upsert(bare_device("test:right"))
            .await
            .expect("right device upserted");
        runtime
            .registry()
            .upsert_group(DeviceGroup {
                id: GroupId("bedroom_lamps".to_string()),
                name: "Bedroom Lamps".to_string(),
                members: vec![
                    DeviceId("test:left".to_string()),
                    DeviceId("test:right".to_string()),
                ],
            })
            .await
            .expect("group upserted");

        let lua = Lua::new();
        let ctx = LuaExecutionContext::new(runtime.clone());
        lua.globals()
            .set("ctx", ctx.clone())
            .expect("ctx is installed");

        lua.load(
            r#"
            ctx:command_group("bedroom_lamps", {
                capability = "power",
                action = "off",
            })
            "#,
        )
        .exec()
        .expect("command_group executes without error");

        let results = ctx.into_results();
        assert_eq!(results.len(), 2, "one result per group member");
        assert!(
            results.iter().all(|r| r.status == "ok"),
            "all members should report ok: {results:?}"
        );
        assert!(
            results.iter().any(|r| r.target == "test:left"),
            "left device should appear in results"
        );
        assert!(
            results.iter().any(|r| r.target == "test:right"),
            "right device should appear in results"
        );

        // Verify commands were actually applied to device attributes.
        for id in ["test:left", "test:right"] {
            let device = runtime
                .registry()
                .get(&DeviceId(id.to_string()))
                .expect("device exists after command");
            assert_eq!(
                device.attributes.get("power"),
                Some(&AttributeValue::Text("off".to_string())),
                "{id} power attribute should be off"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn command_group_errors_on_missing_group() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let lua = Lua::new();
        lua.globals()
            .set("ctx", LuaExecutionContext::new(runtime))
            .expect("ctx is installed");

        let result = lua
            .load(
                r#"
                ctx:command_group("nonexistent_group", {
                    capability = "power",
                    action = "off",
                })
                "#,
            )
            .exec();

        assert!(result.is_err(), "missing group should produce a Lua error");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("nonexistent_group"),
            "error message should name the missing group"
        );
    }

    // ── hook / sleep tests ────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn sleep_pauses_execution_without_consuming_instruction_budget() {
        // Use a very low instruction budget so that if sleep were to count
        // instructions the hook would fire before the sleep finishes.
        let cancel = Arc::new(AtomicBool::new(false));
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let lua = Lua::new();
        lua.globals()
            .set("ctx", LuaExecutionContext::new(runtime))
            .expect("ctx is installed");
        install_execution_hook(&lua, 20_000, cancel);

        let start = std::time::Instant::now();
        let result = lua.load("ctx:sleep(0.05)").exec();
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "sleep should not trigger the compute limit: {result:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(40),
            "sleep should have paused for at least ~50 ms, got {elapsed:?}"
        );
    }

    #[test]
    fn compute_limit_fires_on_infinite_loop() {
        let cancel = Arc::new(AtomicBool::new(false));
        let lua = Lua::new();
        install_execution_hook(&lua, 100_000, cancel);

        let result = lua.load("while true do end").exec();
        assert!(result.is_err(), "infinite loop should be killed");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("compute limit exceeded"),
            "error should mention compute limit; got: {msg}"
        );
    }

    #[test]
    fn cancellation_token_kills_execution() {
        let cancel = Arc::new(AtomicBool::new(true)); // pre-cancelled
        let lua = Lua::new();
        install_execution_hook(&lua, DEFAULT_MAX_INSTRUCTIONS, cancel);

        let result = lua.load("while true do end").exec();
        assert!(result.is_err(), "pre-cancelled execution should be killed");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("execution cancelled"),
            "error should mention cancellation; got: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sleep_validates_negative_and_over_limit() {
        let runtime = Arc::new(Runtime::new(
            Vec::new(),
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ));
        let lua = Lua::new();
        lua.globals()
            .set("ctx", LuaExecutionContext::new(runtime))
            .expect("ctx is installed");

        let neg = lua.load("ctx:sleep(-1)").exec();
        assert!(neg.is_err(), "negative sleep should fail");
        assert!(
            neg.unwrap_err().to_string().contains("0 and 3600"),
            "error should mention range"
        );

        let over = lua.load("ctx:sleep(3601)").exec();
        assert!(over.is_err(), "sleep > 3600 should fail");
        assert!(
            over.unwrap_err().to_string().contains("0 and 3600"),
            "error should mention range"
        );
    }
}
