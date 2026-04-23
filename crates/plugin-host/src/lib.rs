pub mod adapter;
pub mod engine;
pub mod ipc_host;
pub mod manager;
pub mod manifest;
mod plugin;

pub use adapter::{WasmAdapter, WasmAdapterFactory};
pub use engine::create_engine;
pub use ipc_host::{IpcAdapterEntry, IpcAdapterHost};
pub use manager::PluginManager;
pub use manifest::PluginManifest;

#[cfg(test)]
mod tests {
    use super::*;
    use plugin::{DeviceUpdate, WasmPlugin};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // ---------------------------------------------------------------------------
    // Minimal synchronous mock HTTP server.
    //
    // Binds to an ephemeral port, serves the queued responses in order (one
    // request → one response), then stops.  Uses a background std::thread so it
    // does not need an async runtime.
    // ---------------------------------------------------------------------------

    struct MockHttpServer {
        addr: std::net::SocketAddr,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MockHttpServer {
        fn start(responses: Vec<(&'static str, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
            let addr = listener.local_addr().expect("local_addr");

            let thread = std::thread::spawn(move || {
                // One connection per queued response, then exit.
                for (status_line, body) in responses {
                    if let Ok((mut stream, _)) = listener.accept() {
                        // Consume the request (we don't care about the content).
                        let mut buf = [0_u8; 4096];
                        let _ = stream.read(&mut buf);

                        let reply = format!(
                            "{}\r\n\
                             content-type: application/json\r\n\
                             content-length: {}\r\n\
                             connection: close\r\n\
                             \r\n{}",
                            status_line,
                            body.len(),
                            body,
                        );
                        let _ = stream.write_all(reply.as_bytes());
                    }
                }
            });

            Self {
                addr,
                thread: Some(thread),
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for MockHttpServer {
        fn drop(&mut self) {
            // The thread exits naturally once all queued responses are served.
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Canonical Open-Meteo mock response body (mirrors the native adapter tests).
    // ---------------------------------------------------------------------------

    const MOCK_BODY: &str = r#"{"current":{"temperature_2m":18.25,"apparent_temperature":16.0,"relative_humidity_2m":62.0,"precipitation":0.0,"cloud_cover":25,"uv_index":3.5,"surface_pressure":1012.0,"wind_speed_10m":11.5,"wind_gusts_10m":18.0,"wind_direction_10m":225.0,"weather_code":2,"is_day":1}}"#;

    /// Returns the path to the compiled WASM binary, which must exist at
    /// `<workspace-root>/config/plugins/open_meteo.wasm`.
    ///
    /// The test is skipped (with a clear message) when the binary is absent so
    /// that a plain `cargo test -p homecmdr-plugin-host` on a clean checkout
    /// does not fail before the WASM guest has been built.
    fn wasm_path() -> Option<std::path::PathBuf> {
        // CARGO_MANIFEST_DIR is `crates/plugin-host`; workspace root is `../..`.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir).join("../../config/plugins/open_meteo.wasm");
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    /// Load the WASM plugin, call `poll()` against a mock server, and verify
    /// that it produces the expected 12 device updates.
    #[test]
    fn wasm_open_meteo_plugin_produces_expected_device_updates() {
        let path = match wasm_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "SKIP: config/plugins/open_meteo.wasm not found — \
                     build the WASM guest first with:\n  \
                     cargo build --release (in plugins/open-meteo/)"
                );
                return;
            }
        };

        let server = MockHttpServer::start(vec![("HTTP/1.1 200 OK", MOCK_BODY)]);

        let engine = create_engine().expect("create wasmtime engine");
        let mut plugin = WasmPlugin::load(&engine, &path).expect("load WASM plugin");

        let config_json = serde_json::json!({
            "latitude": 51.5,
            "longitude": -0.1,
            "base_url": server.base_url(),
        })
        .to_string();

        plugin.plugin_init(&config_json).expect("plugin_init");
        let updates = plugin.plugin_poll().expect("plugin_poll");

        // The plugin should return exactly 12 device updates.
        assert_eq!(
            updates.len(),
            12,
            "expected 12 device updates, got {}",
            updates.len()
        );

        // Helper: find an update by vendor_id.
        let find = |id: &str| -> &DeviceUpdate {
            updates
                .iter()
                .find(|u| u.vendor_id == id)
                .unwrap_or_else(|| panic!("missing device update for '{id}'"))
        };

        // All updates should be sensors.
        for u in &updates {
            assert_eq!(
                u.kind, "sensor",
                "expected kind='sensor' for '{}'",
                u.vendor_id
            );
        }

        // Spot-check a few vendor IDs and attribute keys.
        let temp = find("temperature_outdoor");
        let attrs: serde_json::Value =
            serde_json::from_str(&temp.attributes_json).expect("parse temp attrs");
        assert_eq!(
            attrs["temperature_outdoor"]["value"],
            serde_json::json!(18.25),
            "temperature_outdoor value mismatch"
        );
        assert_eq!(
            attrs["temperature_outdoor"]["unit"], "celsius",
            "temperature_outdoor unit mismatch"
        );

        let wind = find("wind_speed");
        let attrs: serde_json::Value =
            serde_json::from_str(&wind.attributes_json).expect("parse wind attrs");
        assert_eq!(attrs["wind_speed"]["value"], serde_json::json!(11.5));
        assert_eq!(attrs["wind_speed"]["unit"], "km/h");

        let humidity = find("humidity");
        let attrs: serde_json::Value =
            serde_json::from_str(&humidity.attributes_json).expect("parse humidity attrs");
        assert_eq!(attrs["humidity"]["value"], serde_json::json!(62.0));
        assert_eq!(attrs["humidity"]["unit"], "percent");

        let condition = find("weather_condition");
        let attrs: serde_json::Value =
            serde_json::from_str(&condition.attributes_json).expect("parse condition attrs");
        assert_eq!(
            attrs["weather_condition"], "Partly cloudy",
            "WMO code 2 should map to 'Partly cloudy'"
        );

        let is_day = find("is_day");
        let attrs: serde_json::Value =
            serde_json::from_str(&is_day.attributes_json).expect("parse is_day attrs");
        assert_eq!(
            attrs["custom.open_meteo.is_day"],
            serde_json::json!(true),
            "is_day should be true when is_day=1"
        );
    }

    /// Verify that a failed HTTP response causes `plugin_poll()` to return an
    /// `Err`, not a panic or a silent empty list.
    #[test]
    fn wasm_open_meteo_plugin_returns_error_on_http_failure() {
        let path = match wasm_path() {
            Some(p) => p,
            None => {
                eprintln!("SKIP: config/plugins/open_meteo.wasm not found");
                return;
            }
        };

        let server = MockHttpServer::start(vec![(
            "HTTP/1.1 500 Internal Server Error",
            r#"{"error":"temporary"}"#,
        )]);

        let engine = create_engine().expect("create wasmtime engine");
        let mut plugin = WasmPlugin::load(&engine, &path).expect("load WASM plugin");

        let config_json = serde_json::json!({
            "latitude": 51.5,
            "longitude": -0.1,
            "base_url": server.base_url(),
        })
        .to_string();

        plugin.plugin_init(&config_json).expect("plugin_init");
        let result = plugin.plugin_poll();
        assert!(
            result.is_err(),
            "expected Err on HTTP 500, got {:?}",
            result
        );
    }
}
