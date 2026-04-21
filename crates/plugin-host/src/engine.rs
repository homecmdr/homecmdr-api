use anyhow::Result;
use wasmtime::{Config, Engine};

/// Create the shared wasmtime Engine with the component model enabled.
///
/// This is relatively expensive (JIT compilation setup) so it should be created
/// once and reused for every plugin loaded during the process lifetime.
pub fn create_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Ok(Engine::new(&config)?)
}
