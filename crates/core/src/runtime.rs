use std::collections::HashSet;
use std::sync::Arc;

use serde::Deserialize;
use tokio::task::JoinSet;

use crate::adapter::Adapter;
use crate::bus::EventBus;
use crate::command::DeviceCommand;
use crate::event::Event;
use crate::invoke::{InvokeRequest, InvokeResponse};
use crate::model::DeviceId;
use crate::registry::DeviceRegistry;

/// Config knobs forwarded from `[runtime]` in the TOML config.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct RuntimeConfig {
    pub event_bus_capacity: usize,
}

/// Top-level object that owns all adapters and coordinates commands sent from
/// the HTTP layer, scenes, and automations.  Clone-safe via Arc internals.
pub struct Runtime {
    adapters: Vec<Arc<dyn Adapter>>,
    bus: EventBus,
    registry: DeviceRegistry,
    /// Names of adapters managed as external IPC processes.  Commands to
    /// devices whose adapter name appears here are dispatched via the event
    /// bus rather than an in-process call.
    ipc_adapter_names: HashSet<String>,
}

impl Runtime {
    pub fn new(adapters: Vec<Box<dyn Adapter>>, config: RuntimeConfig) -> Self {
        let bus = EventBus::new(config.event_bus_capacity);
        let registry = DeviceRegistry::new(bus.clone());
        let adapters = adapters.into_iter().map(Arc::from).collect();

        Self {
            adapters,
            bus,
            registry,
            ipc_adapter_names: HashSet::new(),
        }
    }

    /// Register the set of adapter names that are managed as external IPC
    /// processes.  Commands to these adapters are dispatched via the event
    /// bus; commands to any adapter name absent from both this set and the
    /// in-process adapter list will be rejected with an error.
    pub fn with_ipc_adapters(mut self, names: HashSet<String>) -> Self {
        self.ipc_adapter_names = names;
        self
    }

    /// Start all adapters and block until Ctrl-C is received.
    pub async fn run(&self) {
        self.run_until(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    }

    pub fn registry(&self) -> &DeviceRegistry {
        &self.registry
    }

    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Send a command to a device.  Routes to the matching in-process adapter,
    /// or for IPC adapters publishes a `DeviceCommandDispatched` event so the
    /// external process can pick it up.  Returns `false` if the device ID has
    /// no `:` separator; returns an error if no matching adapter is registered.
    pub async fn command_device(
        &self,
        id: &DeviceId,
        command: DeviceCommand,
    ) -> anyhow::Result<bool> {
        let Some((adapter_name, _)) = id.0.split_once(':') else {
            return Ok(false);
        };

        if let Some(adapter) = self
            .adapters
            .iter()
            .find(|adapter| adapter.name() == adapter_name)
        {
            return adapter.command(id, command, self.registry.clone()).await;
        }

        // No in-process adapter owns this device.  If it is a known IPC
        // adapter, dispatch via the event bus so the external process can
        // handle it.  Otherwise the adapter is not registered at all and we
        // must return an error — silently swallowing the command would make
        // scenes report "ok" for devices whose plugin is not installed.
        if self.ipc_adapter_names.contains(adapter_name) {
            self.bus.publish(Event::DeviceCommandDispatched {
                id: id.clone(),
                command,
            });
            return Ok(true);
        }

        anyhow::bail!("no adapter registered for '{adapter_name}'")
    }

    /// Call an adapter-specific function and return its result.
    /// Returns `None` if the target adapter is not registered in-process.
    pub async fn invoke(&self, request: InvokeRequest) -> anyhow::Result<Option<InvokeResponse>> {
        let Some((adapter_name, _)) = request.target.split_once(':') else {
            return Ok(None);
        };

        let Some(adapter) = self
            .adapters
            .iter()
            .find(|adapter| adapter.name() == adapter_name)
        else {
            return Ok(None);
        };

        adapter.invoke(request, self.registry.clone()).await
    }

    /// Run all adapters concurrently until `shutdown` resolves, then abort them.
    pub(crate) async fn run_until<F>(&self, shutdown: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let mut tasks = JoinSet::new();

        for adapter in &self.adapters {
            let adapter = Arc::clone(adapter);
            let registry = self.registry.clone();
            let bus = self.bus.clone();

            tasks.spawn(
                async move { (adapter.name().to_string(), adapter.run(registry, bus).await) },
            );
        }

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    break;
                }
                Some(result) = tasks.join_next() => {
                    match result {
                        Ok((name, Ok(()))) => {
                            tracing::info!(adapter = %name, "adapter exited cleanly");
                        }
                        Ok((name, Err(error))) => {
                            tracing::error!(adapter = %name, error = %error, "adapter exited with error");
                            self.bus.publish(Event::SystemError {
                                message: format!("adapter '{name}' failed: {error}"),
                            });
                        }
                        Err(error) => {
                            tracing::error!(error = %error, "adapter task panicked or was cancelled");
                            self.bus.publish(Event::SystemError {
                                message: format!("adapter task failed: {error}"),
                            });
                        }
                    }

                    if tasks.is_empty() {
                        break;
                    }
                }
                else => break,
            }
        }
    }
}
