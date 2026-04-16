use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use smart_home_core::adapter::{Adapter, AdapterFactory, RegisteredAdapterFactory};
use smart_home_core::bus::EventBus;
use smart_home_core::capability::{
    measurement_value, TEMPERATURE_OUTDOOR, WIND_DIRECTION, WIND_SPEED,
};
use smart_home_core::config::AdapterConfig;
use smart_home_core::event::Event;
use smart_home_core::http::{external_http_client, send_with_retry};
use smart_home_core::model::{AttributeValue, Attributes, Device, DeviceId, DeviceKind, Metadata};
use smart_home_core::registry::DeviceRegistry;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

const ADAPTER_NAME: &str = "open_meteo";
const DEFAULT_BASE_URL: &str = "https://api.open-meteo.com";

#[derive(Debug, Clone, Deserialize)]
pub struct OpenMeteoConfig {
    pub enabled: bool,
    pub latitude: f64,
    pub longitude: f64,
    pub poll_interval_secs: u64,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub test_poll_interval_ms: Option<u64>,
}

pub struct OpenMeteoFactory;

static OPEN_METEO_FACTORY: OpenMeteoFactory = OpenMeteoFactory;

inventory::submit! {
    RegisteredAdapterFactory {
        factory: &OPEN_METEO_FACTORY,
    }
}

pub struct OpenMeteoAdapter {
    client: Client,
    config: OpenMeteoConfig,
    base_url: String,
    poll_interval: Duration,
}

impl OpenMeteoAdapter {
    pub fn new(config: OpenMeteoConfig) -> Result<Self> {
        let poll_interval = config.test_poll_interval_ms.map(Duration::from_millis);
        let base_url = config.base_url.clone();

        Self::with_options(config, base_url, poll_interval)
    }

    pub fn with_base_url(config: OpenMeteoConfig, base_url: impl Into<String>) -> Result<Self> {
        Self::with_options(config, base_url, None)
    }

    pub fn with_options(
        config: OpenMeteoConfig,
        base_url: impl Into<String>,
        poll_interval: Option<Duration>,
    ) -> Result<Self> {
        Ok(Self {
            client: external_http_client()?,
            poll_interval: poll_interval
                .unwrap_or_else(|| Duration::from_secs(config.poll_interval_secs)),
            config,
            base_url: base_url.into(),
        })
    }

    #[cfg(test)]
    fn with_base_url_and_poll_interval(
        config: OpenMeteoConfig,
        base_url: impl Into<String>,
        poll_interval: Duration,
    ) -> Result<Self> {
        Self::with_options(config, base_url, Some(poll_interval))
    }

    async fn poll_once(&self, registry: &DeviceRegistry) -> Result<()> {
        let weather = self.fetch_weather().await?;

        for (vendor_id, attributes) in [
            (
                TEMPERATURE_OUTDOOR,
                single_attribute(
                    TEMPERATURE_OUTDOOR,
                    measurement_value(weather.temperature, "celsius"),
                ),
            ),
            (
                WIND_SPEED,
                single_attribute(WIND_SPEED, measurement_value(weather.wind_speed, "km/h")),
            ),
            (
                WIND_DIRECTION,
                single_attribute(
                    WIND_DIRECTION,
                    AttributeValue::Integer(weather.wind_direction as i64),
                ),
            ),
        ] {
            let previous = registry.get(&DeviceId(format!("{ADAPTER_NAME}:{vendor_id}")));
            registry
                .upsert(build_device(vendor_id, attributes, previous.as_ref()))
                .await
                .with_context(|| format!("failed to upsert Open-Meteo device '{vendor_id}'"))?;
        }

        Ok(())
    }

    async fn fetch_weather(&self) -> Result<CurrentWeather> {
        let response = send_with_retry(
            self.client
                .get(format!(
                    "{}/v1/forecast",
                    self.base_url.trim_end_matches('/')
                ))
                .query(&[
                    ("latitude", self.config.latitude.to_string()),
                    ("longitude", self.config.longitude.to_string()),
                    ("current_weather", "true".to_string()),
                ]),
            "Open-Meteo forecast",
        )
        .await?;

        let body: ForecastResponse = response
            .json()
            .await
            .context("failed to parse Open-Meteo response")?;

        Ok(body.current_weather)
    }

    async fn handle_poll_error(&self, bus: &EventBus, error: anyhow::Error) {
        tracing::error!(error = %error, "Open-Meteo poll failed");
        bus.publish(Event::SystemError {
            message: format!("open_meteo poll failed: {error}"),
        });
        sleep(self.poll_interval()).await;
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }
}

impl AdapterFactory for OpenMeteoFactory {
    fn name(&self) -> &'static str {
        ADAPTER_NAME
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        let config: OpenMeteoConfig =
            serde_json::from_value(config).context("failed to parse open_meteo adapter config")?;
        validate_config(&config)?;

        if !config.enabled {
            return Ok(None);
        }

        Ok(Some(Box::new(OpenMeteoAdapter::new(config)?)))
    }
}

#[async_trait]
impl Adapter for OpenMeteoAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(Event::AdapterStarted {
            adapter: self.name().to_string(),
        });

        loop {
            if let Err(error) = self.poll_once(&registry).await {
                self.handle_poll_error(&bus, error).await;
                continue;
            }

            sleep(self.poll_interval()).await;
        }
    }
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    current_weather: CurrentWeather,
}

#[derive(Debug, Deserialize)]
struct CurrentWeather {
    #[serde(rename = "temperature")]
    temperature: f64,
    #[serde(rename = "windspeed")]
    wind_speed: f64,
    #[serde(rename = "winddirection")]
    wind_direction: f64,
}

fn single_attribute(name: &str, value: AttributeValue) -> Attributes {
    let mut attributes = HashMap::new();
    attributes.insert(name.to_string(), value);
    attributes
}

fn build_device(vendor_id: &str, attributes: Attributes, previous: Option<&Device>) -> Device {
    let now = Utc::now();
    let metadata = Metadata {
        source: ADAPTER_NAME.to_string(),
        accuracy: None,
        vendor_specific: HashMap::new(),
    };
    let updated_at = previous
        .filter(|device| {
            device.kind == DeviceKind::Sensor
                && device.attributes == attributes
                && device.metadata == metadata
        })
        .map(|device| device.updated_at)
        .unwrap_or(now);

    Device {
        id: DeviceId(format!("{ADAPTER_NAME}:{vendor_id}")),
        room_id: previous.and_then(|device| device.room_id.clone()),
        kind: DeviceKind::Sensor,
        attributes,
        metadata,
        updated_at,
        last_seen: now,
    }
}

fn validate_config(config: &OpenMeteoConfig) -> Result<()> {
    if config.poll_interval_secs < 60 {
        anyhow::bail!("adapters.open_meteo.poll_interval_secs must be >= 60");
    }

    Ok(())
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;

    struct MockResponse {
        status_line: &'static str,
        body: &'static str,
    }

    struct MockServer {
        addr: SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock server");
            let addr = listener.local_addr().expect("get mock server address");
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

            let handle = tokio::spawn({
                let responses = Arc::clone(&responses);
                async move {
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            accept_result = listener.accept() => {
                                let (mut socket, _) = accept_result.expect("accept mock connection");
                                let responses = Arc::clone(&responses);

                                tokio::spawn(async move {
                                    let mut buffer = [0_u8; 2048];
                                    let _ = socket.read(&mut buffer).await;

                                    let response = responses
                                        .lock()
                                        .expect("mock response queue lock")
                                        .pop_front()
                                        .unwrap_or(MockResponse {
                                            status_line: "HTTP/1.1 500 Internal Server Error",
                                            body: "{\"error\":\"no queued response\"}",
                                        });

                                    let reply = format!(
                                        "{}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                        response.status_line,
                                        response.body.len(),
                                        response.body,
                                    );

                                    let _ = socket.write_all(reply.as_bytes()).await;
                                });
                            }
                        }
                    }
                }
            });

            Self {
                addr,
                shutdown: Some(shutdown_tx),
                handle,
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            self.handle.abort();
        }
    }

    fn adapter_config() -> OpenMeteoConfig {
        OpenMeteoConfig {
            enabled: true,
            latitude: 51.5,
            longitude: -0.1,
            poll_interval_secs: 60,
            base_url: DEFAULT_BASE_URL.to_string(),
            test_poll_interval_ms: None,
        }
    }

    #[tokio::test]
    async fn adapter_produces_expected_devices_from_successful_response() -> Result<()> {
        let server = MockServer::start(vec![MockResponse {
            status_line: "HTTP/1.1 200 OK",
            body: "{\"current_weather\":{\"temperature\":18.25,\"windspeed\":11.5,\"winddirection\":225.0}}",
        }])
        .await;

        let adapter = OpenMeteoAdapter::with_base_url(adapter_config(), server.base_url())?;
        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus);

        adapter.poll_once(&registry).await?;

        assert_eq!(
            registry
                .get(&DeviceId("open_meteo:temperature_outdoor".to_string()))
                .expect("temperature device exists")
                .attributes,
            single_attribute(TEMPERATURE_OUTDOOR, measurement_value(18.25, "celsius"))
        );
        assert_eq!(
            registry
                .get(&DeviceId("open_meteo:wind_speed".to_string()))
                .expect("wind speed device exists")
                .attributes,
            single_attribute(WIND_SPEED, measurement_value(11.5, "km/h"))
        );
        assert_eq!(
            registry
                .get(&DeviceId("open_meteo:wind_direction".to_string()))
                .expect("wind direction device exists")
                .attributes,
            single_attribute(WIND_DIRECTION, AttributeValue::Integer(225))
        );

        Ok(())
    }

    #[tokio::test]
    async fn adapter_retries_after_http_error_and_recovers() -> Result<()> {
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 500 Internal Server Error",
                body: "{\"error\":\"temporary\"}",
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "{\"current_weather\":{\"temperature\":18.25,\"windspeed\":11.5,\"winddirection\":225.0}}",
            },
        ])
        .await;

        let adapter = Arc::new(OpenMeteoAdapter::with_base_url_and_poll_interval(
            adapter_config(),
            server.base_url(),
            Duration::from_millis(25),
        )?);
        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus.clone());
        let mut subscriber = bus.subscribe();

        let adapter_task = {
            let adapter = Arc::clone(&adapter);
            let registry = registry.clone();
            let bus = bus.clone();
            tokio::spawn(async move { adapter.run(registry, bus).await })
        };

        let started = subscriber.recv().await.expect("adapter started event");
        assert_eq!(
            started,
            Event::AdapterStarted {
                adapter: "open_meteo".to_string(),
            }
        );

        timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    subscriber.recv().await.expect("system error event"),
                    Event::SystemError { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("system error event arrives in time");

        timeout(Duration::from_secs(2), async {
            loop {
                if registry
                    .get(&DeviceId("open_meteo:temperature_outdoor".to_string()))
                    .is_some()
                {
                    break;
                }

                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("adapter recovers on next poll interval");

        adapter_task.abort();
        let _ = adapter_task.await;

        Ok(())
    }

    #[tokio::test]
    async fn adapter_refreshes_last_seen_without_bumping_updated_at_for_identical_poll(
    ) -> Result<()> {
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "{\"current_weather\":{\"temperature\":18.25,\"windspeed\":11.5,\"winddirection\":225.0}}",
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "{\"current_weather\":{\"temperature\":18.25,\"windspeed\":11.5,\"winddirection\":225.0}}",
            },
        ])
        .await;

        let adapter = OpenMeteoAdapter::with_base_url(adapter_config(), server.base_url())?;
        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus);

        adapter.poll_once(&registry).await?;
        let original = registry
            .get(&DeviceId("open_meteo:temperature_outdoor".to_string()))
            .expect("temperature device exists after first poll");

        tokio::time::sleep(Duration::from_millis(5)).await;
        adapter.poll_once(&registry).await?;

        let seen_again = registry
            .get(&DeviceId("open_meteo:temperature_outdoor".to_string()))
            .expect("temperature device exists after second poll");

        assert_eq!(seen_again.updated_at, original.updated_at);
        assert!(seen_again.last_seen > original.last_seen);

        Ok(())
    }

    #[test]
    fn factory_returns_none_when_disabled() {
        let adapter = OPEN_METEO_FACTORY
            .build(serde_json::json!({
                "enabled": false,
                "latitude": 51.5,
                "longitude": -0.1,
                "poll_interval_secs": 90
            }))
            .expect("factory should parse disabled config");

        assert!(adapter.is_none());
    }

    #[test]
    fn factory_rejects_invalid_poll_interval() {
        let error = OPEN_METEO_FACTORY
            .build(serde_json::json!({
                "enabled": true,
                "latitude": 51.5,
                "longitude": -0.1,
                "poll_interval_secs": 59
            }))
            .err()
            .expect("factory should reject invalid poll interval");

        assert_eq!(
            error.to_string(),
            "adapters.open_meteo.poll_interval_secs must be >= 60"
        );
    }
}
