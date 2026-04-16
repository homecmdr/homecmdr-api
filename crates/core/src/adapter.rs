use crate::bus::EventBus;
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
}
