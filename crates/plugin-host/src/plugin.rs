/// Internal wasmtime component model bindings and plugin execution.
///
/// Everything in this module is crate-private.  Public surface is exposed
/// through `WasmAdapter` / `WasmAdapterFactory` in `adapter.rs`.
use anyhow::{Context, Result};
use std::path::Path;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// ---------------------------------------------------------------------------
// wasmtime component model bindings generated from our WIT definition.
// Placed in a submodule so that the generated `Adapter` struct does not
// shadow `homecmdr_core::adapter::Adapter`.
// ---------------------------------------------------------------------------
mod bindings {
    wasmtime::component::bindgen!({
        path: "wit/homecmdr-plugin.wit",
        world: "adapter",
    });
}

// Types live in `homecmdr::plugin::types` (the shared `interface types` from WIT).
pub use bindings::homecmdr::plugin::types::CommandResult;
pub use bindings::homecmdr::plugin::types::DeviceUpdate;

// ---------------------------------------------------------------------------
// Host state: owns WASI context + HTTP client.
// ---------------------------------------------------------------------------

struct PluginHost {
    table: ResourceTable,
    wasi: WasiCtx,
    /// Pure-sync HTTP agent (ureq) — no internal Tokio runtime, safe to drop anywhere.
    http: ureq::Agent,
    /// Plugin name for log attribution (best-effort, filled in after init).
    plugin_name: String,
}

impl WasiView for PluginHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// ---------------------------------------------------------------------------
// Implement the host-provided imports that the WIT world declares.
// ---------------------------------------------------------------------------

impl bindings::homecmdr::plugin::host_http::Host for PluginHost {
    fn get(&mut self, url: String) -> Result<String, String> {
        self.http
            .get(&url)
            .call()
            .map_err(|e| e.to_string())?
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())
    }

    fn post(&mut self, url: String, content_type: String, body: String) -> Result<String, String> {
        self.http
            .post(&url)
            .content_type(&content_type)
            .send(&body)
            .map_err(|e: ureq::Error| e.to_string())?
            .body_mut()
            .read_to_string()
            .map_err(|e: ureq::Error| e.to_string())
    }

    fn put(&mut self, url: String, content_type: String, body: String) -> Result<String, String> {
        self.http
            .put(&url)
            .content_type(&content_type)
            .send(&body)
            .map_err(|e: ureq::Error| e.to_string())?
            .body_mut()
            .read_to_string()
            .map_err(|e: ureq::Error| e.to_string())
    }
}

impl bindings::homecmdr::plugin::host_log::Host for PluginHost {
    fn log(&mut self, level: String, message: String) {
        let plugin = &self.plugin_name;
        match level.as_str() {
            "error" => tracing::error!(plugin = plugin.as_str(), "{}", message),
            "warn" => tracing::warn!(plugin = plugin.as_str(), "{}", message),
            "info" => tracing::info!(plugin = plugin.as_str(), "{}", message),
            "debug" => tracing::debug!(plugin = plugin.as_str(), "{}", message),
            _ => tracing::trace!(plugin = plugin.as_str(), "{}", message),
        }
    }
}

// The `types` interface also generates a `Host` trait (it is empty since `types`
// only declares records, not functions).
impl bindings::homecmdr::plugin::types::Host for PluginHost {}

// ---------------------------------------------------------------------------
// Build a shared Linker that knows about both WASI and our custom imports.
// ---------------------------------------------------------------------------

fn build_linker(engine: &Engine) -> Result<Linker<PluginHost>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    bindings::Adapter::add_to_linker::<PluginHost, HasSelf<PluginHost>>(&mut linker, |x| x)?;
    Ok(linker)
}

// ---------------------------------------------------------------------------
// WasmPlugin — owns the Store and the instantiated component bindings.
// ---------------------------------------------------------------------------

pub struct WasmPlugin {
    store: Store<PluginHost>,
    bindings: bindings::Adapter,
}

impl WasmPlugin {
    /// Load a WASM component from `wasm_path` and prepare it for use.
    ///
    /// `init()` must be called before any `poll()` / `command()` calls.
    pub fn load(engine: &Engine, wasm_path: &Path) -> Result<Self> {
        let linker = build_linker(engine)?;

        let component = Component::from_file(engine, wasm_path)
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "failed to load WASM component from '{}'",
                    wasm_path.display()
                )
            })?;

        let name_hint = wasm_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let wasi = WasiCtxBuilder::new().inherit_stderr().build();
        let mut store = Store::new(
            engine,
            PluginHost {
                table: ResourceTable::new(),
                wasi,
                http: ureq::agent(),
                plugin_name: name_hint,
            },
        );

        let bindings = bindings::Adapter::instantiate(&mut store, &component, &linker)
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "failed to instantiate WASM plugin from '{}'",
                    wasm_path.display()
                )
            })?;

        Ok(Self { store, bindings })
    }

    #[allow(dead_code)]
    pub fn plugin_name(&mut self) -> Result<String> {
        self.bindings
            .homecmdr_plugin_plugin()
            .call_name(&mut self.store)
            .map_err(anyhow::Error::from)
            .context("failed to call plugin.name()")
    }

    pub fn plugin_init(&mut self, config_json: &str) -> Result<()> {
        self.bindings
            .homecmdr_plugin_plugin()
            .call_init(&mut self.store, config_json)
            .map_err(anyhow::Error::from)
            .context("failed to call plugin.init()")?
            .map_err(|e| anyhow::anyhow!("plugin init failed: {e}"))
    }

    pub fn plugin_poll(&mut self) -> Result<Vec<DeviceUpdate>> {
        self.bindings
            .homecmdr_plugin_plugin()
            .call_poll(&mut self.store)
            .map_err(anyhow::Error::from)
            .context("failed to call plugin.poll()")?
            .map_err(|e| anyhow::anyhow!("plugin poll returned error: {e}"))
    }

    pub fn plugin_command(&mut self, device_id: &str, command_json: &str) -> Result<CommandResult> {
        self.bindings
            .homecmdr_plugin_plugin()
            .call_command(&mut self.store, device_id, command_json)
            .map_err(anyhow::Error::from)
            .context("failed to call plugin.command()")?
            .map_err(|e| anyhow::anyhow!("plugin command returned error: {e}"))
    }
}
