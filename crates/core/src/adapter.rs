use crate::bus::EventBus;
use crate::command::DeviceCommand;
use crate::config::AdapterConfig;
use crate::model::DeviceId;
use crate::registry::DeviceRegistry;

#[async_trait::async_trait]
pub trait Adapter: Send + Sync + 'static {
    /// Unique name for this adapter instance (e.g. "open_meteo", "zigbee2mqtt").
    fn name(&self) -> &str;

    /// Start the adapter. Must not return until the adapter is stopped.
    /// Implementations must handle internal reconnection without propagating
    /// transient errors to the caller.
    ///
    /// Adapter design rules:
    /// - All device IDs must be namespaced as `"{adapter_name}:{vendor_id}"`.
    /// - Adapters must not share state with other adapters.
    /// - Adapters must publish `Event::AdapterStarted` at the beginning of `run`.
    /// - Adapters must publish `Event::SystemError` on non-recoverable failures before returning `Err`.
    /// - Reconnection backoff must be exponential with a maximum of 60 seconds.
    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> anyhow::Result<()>;

    /// Handle a command for a device owned by this adapter.
    ///
    /// Returns `Ok(true)` when the command was applied, `Ok(false)` when the
    /// adapter does not support commands for that device, and `Err(...)` when
    /// the command was recognized but invalid or failed.
    async fn command(
        &self,
        _device_id: &DeviceId,
        _command: DeviceCommand,
        _registry: DeviceRegistry,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
}

pub trait AdapterFactory: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn build(&self, config: AdapterConfig) -> anyhow::Result<Option<Box<dyn Adapter>>>;
}

pub struct RegisteredAdapterFactory {
    pub factory: &'static dyn AdapterFactory,
}

inventory::collect!(RegisteredAdapterFactory);

pub fn registered_adapter_factories() -> impl Iterator<Item = &'static dyn AdapterFactory> {
    inventory::iter::<RegisteredAdapterFactory>
        .into_iter()
        .map(|registration| registration.factory)
}
