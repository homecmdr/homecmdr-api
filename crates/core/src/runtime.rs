use std::sync::Arc;

use serde::Deserialize;
use tokio::task::JoinSet;

use crate::adapter::Adapter;
use crate::bus::EventBus;
use crate::event::Event;
use crate::registry::DeviceRegistry;

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct RuntimeConfig {
    pub event_bus_capacity: usize,
}

pub struct Runtime {
    adapters: Vec<Arc<dyn Adapter>>,
    bus: EventBus,
    registry: DeviceRegistry,
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
        }
    }

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

    pub(crate) async fn run_until<F>(&self, shutdown: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let mut tasks = JoinSet::new();

        for adapter in &self.adapters {
            let adapter = Arc::clone(adapter);
            let registry = self.registry.clone();
            let bus = self.bus.clone();

            tasks.spawn(async move { (adapter.name().to_string(), adapter.run(registry, bus).await) });
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
