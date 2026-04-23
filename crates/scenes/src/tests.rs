#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use homecmdr_core::adapter::Adapter;
    use homecmdr_core::bus::EventBus;
    use homecmdr_core::command::DeviceCommand;
    use homecmdr_core::model::{AttributeValue, Device, DeviceId, DeviceKind, Metadata};
    use homecmdr_core::registry::DeviceRegistry;
    use homecmdr_core::runtime::{Runtime, RuntimeConfig};
    use tokio::time::{sleep, timeout, Duration};

    use crate::catalog::SceneCatalog;
    use crate::runner::SceneRunner;
    use crate::types::SceneRunOutcome;

    struct CommandAdapter;

    #[async_trait::async_trait]
    impl Adapter for CommandAdapter {
        fn name(&self) -> &str {
            "test"
        }

        async fn run(&self, _registry: DeviceRegistry, _bus: EventBus) -> Result<()> {
            std::future::pending::<()>().await;
            Ok(())
        }

        async fn command(
            &self,
            device_id: &DeviceId,
            command: DeviceCommand,
            registry: DeviceRegistry,
        ) -> Result<bool> {
            if device_id.0 != "test:device" {
                return Ok(false);
            }

            let mut device = registry.get(device_id).expect("test device exists");
            device.attributes.insert(
                command.capability,
                command.value.expect("test command must include value"),
            );
            registry
                .upsert(device)
                .await
                .expect("registry update succeeds");
            Ok(true)
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{unique}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn write_scene(dir: &Path, name: &str, source: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, source).expect("write scene file");
        path
    }

    fn sample_device(id: &str) -> Device {
        Device {
            id: DeviceId(id.to_string()),
            room_id: None,
            kind: DeviceKind::Light,
            attributes: HashMap::from([(
                "power".to_string(),
                AttributeValue::Text("off".to_string()),
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

    fn make_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::new(
            vec![Box::new(CommandAdapter)],
            RuntimeConfig {
                event_bus_capacity: 16,
            },
        ))
    }

    #[test]
    fn loads_valid_scene_catalog() {
        let dir = temp_dir("homecmdr-scenes");
        write_scene(
            &dir,
            "video.lua",
            r#"return {
                id = "video",
                name = "Video",
                execute = function(ctx)
                end
            }"#,
        );

        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        assert_eq!(catalog.summaries().len(), 1);
        assert_eq!(catalog.summaries()[0].id, "video");
    }

    #[test]
    fn rejects_scene_without_execute() {
        let dir = temp_dir("homecmdr-scenes");
        write_scene(
            &dir,
            "broken.lua",
            r#"return {
                id = "video",
                name = "Video"
            }"#,
        );

        let error = SceneCatalog::load_from_directory(&dir, None)
            .err()
            .expect("missing execute should fail");
        assert!(error
            .to_string()
            .contains("missing function field 'execute'"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn executes_scene_commands_against_runtime() {
        let dir = temp_dir("homecmdr-scenes");
        write_scene(
            &dir,
            "set-brightness.lua",
            r#"return {
                id = "set_brightness",
                name = "Set Brightness",
                execute = function(ctx)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 42,
                    })
                end
            }"#,
        );

        let runtime = make_runtime();
        runtime
            .registry()
            .upsert(sample_device("test:device"))
            .await
            .expect("test device exists");

        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        let results = catalog
            .execute("set_brightness", runtime.clone())
            .expect("scene executes")
            .expect("scene exists");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "ok");
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("updated device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(42))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn executes_scene_using_required_script_module() {
        let dir = temp_dir("homecmdr-scenes");
        let scripts_dir = temp_dir("homecmdr-scripts");
        write_scene(
            &dir,
            "set-brightness.lua",
            r#"local helpers = require("lighting.helpers")

            return {
                id = "set_brightness",
                name = "Set Brightness",
                execute = function(ctx)
                    helpers.set_brightness(ctx, "test:device", 33)
                end
            }"#,
        );
        fs::create_dir_all(scripts_dir.join("lighting")).expect("create scripts namespace dir");
        fs::write(
            scripts_dir.join("lighting/helpers.lua"),
            r#"local M = {}

            function M.set_brightness(ctx, device_id, value)
                ctx:command(device_id, {
                    capability = "brightness",
                    action = "set",
                    value = value,
                })
            end

            return M"#,
        )
        .expect("write helper script");

        let runtime = make_runtime();
        runtime
            .registry()
            .upsert(sample_device("test:device"))
            .await
            .expect("test device exists");

        let catalog = SceneCatalog::load_from_directory(&dir, Some(scripts_dir))
            .expect("scene catalog loads");
        let results = catalog
            .execute("set_brightness", runtime.clone())
            .expect("scene executes")
            .expect("scene exists");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "ok");
        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("updated device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(33))
        );
    }

    // ── execution mode tests ──────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn single_mode_rejects_concurrent_scene_execution() {
        let dir = temp_dir("homecmdr-scenes");
        write_scene(
            &dir,
            "single.lua",
            r#"return {
                id = "single_scene",
                name = "Single Scene",
                mode = "single",
                execute = function(ctx)
                    ctx:sleep(0.2)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 5,
                    })
                end
            }"#,
        );

        let runtime = make_runtime();
        runtime
            .registry()
            .upsert(sample_device("test:device"))
            .await
            .expect("device upserted");

        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        let runner = SceneRunner::new(catalog);

        // spawn first execution (will block ~200 ms inside Lua sleep)
        let runner1 = runner.clone();
        let runtime1 = runtime.clone();
        let handle1 = tokio::spawn(async move { runner1.execute("single_scene", runtime1).await });

        // let the first invocation enter Lua and start sleeping
        sleep(Duration::from_millis(20)).await;

        // second invocation should be dropped
        let outcome2 = runner
            .execute("single_scene", runtime.clone())
            .await
            .expect("second execute does not error");

        let outcome1 = handle1
            .await
            .expect("task joins")
            .expect("first execute ok");

        assert!(
            matches!(outcome1, SceneRunOutcome::Completed(_)),
            "first invocation should complete"
        );
        assert!(
            matches!(outcome2, SceneRunOutcome::Dropped),
            "second invocation should be dropped"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_mode_allows_concurrent_executions() {
        let dir = temp_dir("homecmdr-scenes");
        write_scene(
            &dir,
            "parallel.lua",
            r#"return {
                id = "parallel_scene",
                name = "Parallel Scene",
                mode = { type = "parallel", max = 2 },
                execute = function(ctx)
                    ctx:sleep(0.1)
                end
            }"#,
        );

        let runtime = make_runtime();
        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        let runner = SceneRunner::new(catalog);

        let runner1 = runner.clone();
        let runtime1 = runtime.clone();
        let handle1 =
            tokio::spawn(async move { runner1.execute("parallel_scene", runtime1).await });

        let runner2 = runner.clone();
        let runtime2 = runtime.clone();
        let handle2 =
            tokio::spawn(async move { runner2.execute("parallel_scene", runtime2).await });

        let (outcome1, outcome2) =
            tokio::join!(async { handle1.await.expect("task1 joins") }, async {
                handle2.await.expect("task2 joins")
            },);

        assert!(
            matches!(
                outcome1.expect("outcome1 ok"),
                SceneRunOutcome::Completed(_)
            ),
            "first parallel invocation should complete"
        );
        assert!(
            matches!(
                outcome2.expect("outcome2 ok"),
                SceneRunOutcome::Completed(_)
            ),
            "second parallel invocation should complete"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sleep_in_scene_works() {
        let dir = temp_dir("homecmdr-scenes");
        write_scene(
            &dir,
            "sleepy.lua",
            r#"return {
                id = "sleepy_scene",
                name = "Sleepy Scene",
                execute = function(ctx)
                    ctx:sleep(0.05)
                    ctx:command("test:device", {
                        capability = "brightness",
                        action = "set",
                        value = 77,
                    })
                end
            }"#,
        );

        let runtime = make_runtime();
        runtime
            .registry()
            .upsert(sample_device("test:device"))
            .await
            .expect("device upserted");

        let catalog = SceneCatalog::load_from_directory(&dir, None).expect("scene catalog loads");
        let runner = SceneRunner::new(catalog);

        let outcome = timeout(
            Duration::from_secs(2),
            runner.execute("sleepy_scene", runtime.clone()),
        )
        .await
        .expect("scene completes within timeout")
        .expect("execute ok");

        match outcome {
            SceneRunOutcome::Completed(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].status, "ok");
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        assert_eq!(
            runtime
                .registry()
                .get(&DeviceId("test:device".to_string()))
                .expect("device exists")
                .attributes
                .get("brightness"),
            Some(&AttributeValue::Integer(77))
        );
    }
}
