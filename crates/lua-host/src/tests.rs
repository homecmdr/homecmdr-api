#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use homecmdr_core::adapter::Adapter;
    use homecmdr_core::bus::EventBus;
    use homecmdr_core::command::DeviceCommand;
    use homecmdr_core::model::{
        AttributeValue, Device, DeviceGroup, DeviceId, DeviceKind, GroupId, Metadata, Room, RoomId,
    };
    use homecmdr_core::registry::DeviceRegistry;
    use homecmdr_core::runtime::{Runtime, RuntimeConfig};
    use mlua::{Lua, Table};

    use crate::context::LuaExecutionContext;
    use crate::loader::resolve_script_module_path;
    use crate::runtime::{install_execution_hook, DEFAULT_MAX_INSTRUCTIONS};

    fn temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("homecmdr-lua-host-{unique}"));
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
