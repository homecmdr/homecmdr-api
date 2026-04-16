use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use smart_home_core::adapter::{Adapter, AdapterFactory, RegisteredAdapterFactory};
use smart_home_core::bus::EventBus;
use smart_home_core::capability::{POWER, STATE};
use smart_home_core::command::DeviceCommand;
use smart_home_core::config::AdapterConfig;
use smart_home_core::event::Event;
use smart_home_core::http::{external_http_client, send_with_retry};
use smart_home_core::model::{AttributeValue, Attributes, Device, DeviceId, DeviceKind, Metadata};
use smart_home_core::registry::DeviceRegistry;
use tokio::time::{sleep, Duration};

const ADAPTER_NAME: &str = "roku_tv";
const DEVICE_VENDOR_ID: &str = "tv";

#[derive(Debug, Clone, Deserialize)]
pub struct RokuTvConfig {
    pub enabled: bool,
    pub ip_address: String,
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub test_poll_interval_ms: Option<u64>,
}

pub struct RokuTvFactory;

static ROKU_TV_FACTORY: RokuTvFactory = RokuTvFactory;

inventory::submit! {
    RegisteredAdapterFactory {
        factory: &ROKU_TV_FACTORY,
    }
}

pub struct RokuTvAdapter {
    client: Client,
    poll_interval: Duration,
    base_url: String,
}

impl RokuTvAdapter {
    pub fn new(config: RokuTvConfig) -> Result<Self> {
        let poll_interval = config
            .test_poll_interval_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(config.poll_interval_secs));

        Ok(Self {
            client: external_http_client()?,
            base_url: format!("http://{}:8060", config.ip_address.trim()),
            poll_interval,
        })
    }

    #[cfg(test)]
    fn with_base_url(config: RokuTvConfig, base_url: impl Into<String>) -> Result<Self> {
        let poll_interval = config
            .test_poll_interval_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(config.poll_interval_secs));

        Ok(Self {
            client: external_http_client()?,
            poll_interval,
            base_url: base_url.into(),
        })
    }

    async fn poll_once(&self, registry: &DeviceRegistry) -> Result<()> {
        let info = self.fetch_device_info().await?;
        let previous = registry.get(&roku_device_id());
        let device = build_device(info, previous.as_ref());

        registry
            .upsert(device)
            .await
            .context("failed to upsert Roku TV device")?;

        Ok(())
    }

    async fn fetch_device_info(&self) -> Result<RokuDeviceInfo> {
        let body = send_with_retry(
            self.client
                .get(format!("{}/query/device-info", self.base_url())),
            "Roku device info",
        )
        .await?
            .text()
            .await
            .context("failed to read Roku device-info response")?;

        parse_device_info(&body)
    }

    async fn send_keypress(&self, key: &str) -> Result<()> {
        send_with_retry(
            self.client
                .post(format!("{}/keypress/{key}", self.base_url())),
            &format!("Roku keypress '{key}'"),
        )
        .await?;

        Ok(())
    }

    async fn handle_command(
        &self,
        device_id: &DeviceId,
        command: DeviceCommand,
        registry: DeviceRegistry,
    ) -> Result<bool> {
        if *device_id != roku_device_id() {
            return Ok(false);
        }

        let key = match (command.capability.as_str(), command.action.as_str()) {
            (POWER, "on") => "PowerOn",
            (POWER, "off") => "PowerOff",
            (POWER, "toggle") => "Power",
            _ => return Ok(false),
        };

        self.send_keypress(key).await?;

        let info = self.fetch_device_info().await?;
        let previous = registry.get(device_id);
        registry
            .upsert(build_device(info, previous.as_ref()))
            .await
            .with_context(|| {
                format!(
                    "failed to update registry for '{}': command applied",
                    device_id.0
                )
            })?;

        Ok(true)
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl AdapterFactory for RokuTvFactory {
    fn name(&self) -> &'static str {
        ADAPTER_NAME
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        let config: RokuTvConfig =
            serde_json::from_value(config).context("failed to parse roku_tv adapter config")?;
        validate_config(&config)?;

        if !config.enabled {
            return Ok(None);
        }

        Ok(Some(Box::new(RokuTvAdapter::new(config)?)))
    }
}

#[async_trait]
impl Adapter for RokuTvAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(Event::AdapterStarted {
            adapter: self.name().to_string(),
        });

        loop {
            if let Err(error) = self.poll_once(&registry).await {
                tracing::error!(error = %error, "Roku TV poll failed");
                bus.publish(Event::SystemError {
                    message: format!("roku_tv poll failed: {error}"),
                });
            }

            sleep(self.poll_interval).await;
        }
    }

    async fn command(
        &self,
        device_id: &DeviceId,
        command: DeviceCommand,
        registry: DeviceRegistry,
    ) -> Result<bool> {
        self.handle_command(device_id, command, registry).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RokuDeviceInfo {
    power_mode: String,
    friendly_name: Option<String>,
    model_name: Option<String>,
}

fn validate_config(config: &RokuTvConfig) -> Result<()> {
    if config.ip_address.trim().is_empty() {
        bail!("adapters.roku_tv.ip_address must not be empty");
    }

    if config.poll_interval_secs == 0 && config.test_poll_interval_ms.is_none() {
        bail!("adapters.roku_tv.poll_interval_secs must be >= 1");
    }

    Ok(())
}

fn build_device(info: RokuDeviceInfo, previous: Option<&Device>) -> Device {
    let now = Utc::now();
    let power = if is_powered_on(&info.power_mode) {
        "on"
    } else {
        "off"
    };
    let attributes = Attributes::from([
        (POWER.to_string(), AttributeValue::Text(power.to_string())),
        (
            STATE.to_string(),
            AttributeValue::Text("online".to_string()),
        ),
    ]);

    let mut vendor_specific =
        HashMap::from([("power_mode".to_string(), serde_json::json!(info.power_mode))]);
    if let Some(name) = info.friendly_name {
        vendor_specific.insert("friendly_name".to_string(), serde_json::json!(name));
    }
    if let Some(model) = info.model_name {
        vendor_specific.insert("model_name".to_string(), serde_json::json!(model));
    }

    let metadata = Metadata {
        source: ADAPTER_NAME.to_string(),
        accuracy: None,
        vendor_specific,
    };
    let updated_at = previous
        .filter(|device| {
            device.kind == DeviceKind::Switch
                && device.attributes == attributes
                && device.metadata == metadata
        })
        .map(|device| device.updated_at)
        .unwrap_or(now);

    Device {
        id: roku_device_id(),
        room_id: previous.and_then(|device| device.room_id.clone()),
        kind: DeviceKind::Switch,
        attributes,
        metadata,
        updated_at,
        last_seen: now,
    }
}

fn roku_device_id() -> DeviceId {
    DeviceId(format!("{ADAPTER_NAME}:{DEVICE_VENDOR_ID}"))
}

fn is_powered_on(power_mode: &str) -> bool {
    !matches!(power_mode, "PowerOff" | "DisplayOff" | "Suspend")
}

fn parse_device_info(xml: &str) -> Result<RokuDeviceInfo> {
    Ok(RokuDeviceInfo {
        power_mode: extract_xml_tag(xml, "power-mode").unwrap_or_else(|| "PowerOn".to_string()),
        friendly_name: extract_xml_tag(xml, "friendly-device-name"),
        model_name: extract_xml_tag(xml, "model-name"),
    })
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    let start_index = xml.find(&start)? + start.len();
    let rest = &xml[start_index..];
    let end_index = rest.find(&end)?;
    Some(rest[..end_index].trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;

    struct MockResponse {
        status_line: &'static str,
        content_type: &'static str,
        body: &'static str,
    }

    struct MockServer {
        addr: SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        handle: tokio::task::JoinHandle<()>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock roku server");
            let addr = listener.local_addr().expect("get mock roku server address");
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

            let handle = tokio::spawn({
                let responses = Arc::clone(&responses);
                let requests = Arc::clone(&requests);
                async move {
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            accept_result = listener.accept() => {
                                let (mut socket, _) = accept_result.expect("accept mock connection");
                                let responses = Arc::clone(&responses);
                                let requests = Arc::clone(&requests);

                                tokio::spawn(async move {
                                    let mut buffer = [0_u8; 4096];
                                    let bytes = socket.read(&mut buffer).await.expect("read request bytes");
                                    requests
                                        .lock()
                                        .expect("request log lock")
                                        .push(String::from_utf8_lossy(&buffer[..bytes]).to_string());

                                    let response = responses
                                        .lock()
                                        .expect("mock response queue lock")
                                        .pop_front()
                                        .unwrap_or(MockResponse {
                                            status_line: "HTTP/1.1 500 Internal Server Error",
                                            content_type: "text/plain",
                                            body: "no queued response",
                                        });

                                    let reply = format!(
                                        "{}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                        response.status_line,
                                        response.content_type,
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
                requests,
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("request log lock").clone()
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

    fn adapter_config(ip_address: String) -> RokuTvConfig {
        RokuTvConfig {
            enabled: true,
            ip_address,
            poll_interval_secs: 30,
            test_poll_interval_ms: Some(25),
        }
    }

    fn device_info_xml(power_mode: &str) -> &'static str {
        match power_mode {
            "PowerOff" => "<device-info><power-mode>PowerOff</power-mode><friendly-device-name>Living Room TV</friendly-device-name><model-name>Roku TV</model-name></device-info>",
            _ => "<device-info><power-mode>PowerOn</power-mode><friendly-device-name>Living Room TV</friendly-device-name><model-name>Roku TV</model-name></device-info>",
        }
    }

    #[tokio::test]
    async fn adapter_polls_roku_power_state() {
        let server = MockServer::start(vec![MockResponse {
            status_line: "HTTP/1.1 200 OK",
            content_type: "application/xml",
            body: device_info_xml("PowerOn"),
        }])
        .await;

        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus);
        let adapter = RokuTvAdapter::with_base_url(
            adapter_config(server.addr.ip().to_string()),
            format!("http://{}", server.addr),
        )
        .expect("adapter builds");

        adapter.poll_once(&registry).await.expect("poll succeeds");

        let device = registry
            .get(&roku_device_id())
            .expect("roku tv device present in registry");
        assert_eq!(device.kind, DeviceKind::Switch);
        assert_eq!(
            device.attributes.get(POWER),
            Some(&AttributeValue::Text("on".to_string()))
        );
        assert_eq!(
            device.attributes.get(STATE),
            Some(&AttributeValue::Text("online".to_string()))
        );
    }

    #[tokio::test]
    async fn adapter_command_sends_power_off_keypress() {
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOn"),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "text/plain",
                body: "",
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOff"),
            },
        ])
        .await;

        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus);
        let adapter = RokuTvAdapter::with_base_url(
            adapter_config(server.addr.ip().to_string()),
            format!("http://{}", server.addr),
        )
        .expect("adapter builds");

        adapter
            .poll_once(&registry)
            .await
            .expect("initial poll succeeds");
        assert!(adapter
            .command(
                &roku_device_id(),
                DeviceCommand {
                    capability: POWER.to_string(),
                    action: "off".to_string(),
                    value: None,
                },
                registry.clone(),
            )
            .await
            .expect("command succeeds"));

        let requests = server.requests();
        assert!(requests
            .iter()
            .any(|request| request.starts_with("POST /keypress/PowerOff HTTP/1.1")));

        let device = registry
            .get(&roku_device_id())
            .expect("roku tv device present after command");
        assert_eq!(
            device.attributes.get(POWER),
            Some(&AttributeValue::Text("off".to_string()))
        );
    }

    #[test]
    fn config_requires_ip_address() {
        let error = validate_config(&RokuTvConfig {
            enabled: true,
            ip_address: "   ".to_string(),
            poll_interval_secs: 30,
            test_poll_interval_ms: None,
        })
        .expect_err("empty ip address should fail");

        assert_eq!(
            error.to_string(),
            "adapters.roku_tv.ip_address must not be empty"
        );
    }

    #[test]
    fn parse_device_info_extracts_power_and_metadata() {
        let info = parse_device_info(device_info_xml("PowerOff")).expect("xml parses");

        assert_eq!(info.power_mode, "PowerOff");
        assert_eq!(info.friendly_name.as_deref(), Some("Living Room TV"));
        assert_eq!(info.model_name.as_deref(), Some("Roku TV"));
    }
}
