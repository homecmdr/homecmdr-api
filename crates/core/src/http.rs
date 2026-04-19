use anyhow::{bail, Context, Result};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use std::time::Duration;
use tokio::time::sleep;

pub const EXTERNAL_HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub const EXTERNAL_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const EXTERNAL_HTTP_MAX_ATTEMPTS: usize = 3;
pub const EXTERNAL_HTTP_RETRY_DELAY: Duration = Duration::from_millis(250);

pub fn external_http_client() -> Result<Client> {
    Client::builder()
        .timeout(EXTERNAL_HTTP_REQUEST_TIMEOUT)
        .connect_timeout(EXTERNAL_HTTP_CONNECT_TIMEOUT)
        .build()
        .context("failed to build external HTTP client")
}

pub async fn send_with_retry(request: RequestBuilder, operation: &str) -> Result<Response> {
    for attempt in 1..=EXTERNAL_HTTP_MAX_ATTEMPTS {
        let Some(request) = request.try_clone() else {
            bail!("failed to clone {operation} request for retry handling");
        };

        match request.send().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let status = response.status();

                if attempt < EXTERNAL_HTTP_MAX_ATTEMPTS && is_retryable_status(status) {
                    tracing::warn!(
                        operation,
                        attempt,
                        max_attempts = EXTERNAL_HTTP_MAX_ATTEMPTS,
                        status = %status,
                        "retrying external HTTP request after retryable status"
                    );
                    sleep(EXTERNAL_HTTP_RETRY_DELAY).await;
                    continue;
                }

                return response
                    .error_for_status()
                    .with_context(|| format!("{operation} returned an error status"));
            }
            Err(error) => {
                if attempt < EXTERNAL_HTTP_MAX_ATTEMPTS && is_retryable_error(&error) {
                    tracing::warn!(
                        operation,
                        attempt,
                        max_attempts = EXTERNAL_HTTP_MAX_ATTEMPTS,
                        error = %error,
                        "retrying external HTTP request after transient failure"
                    );
                    sleep(EXTERNAL_HTTP_RETRY_DELAY).await;
                    continue;
                }

                return Err(error).with_context(|| format!("failed to request {operation}"));
            }
        }
    }

    unreachable!("external HTTP retry loop always returns before exhausting attempts")
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn is_retryable_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect()
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
        body: &'static str,
        delay: Duration,
    }

    struct MockServer {
        addr: SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        handle: tokio::task::JoinHandle<()>,
        request_count: Arc<Mutex<usize>>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock server");
            let addr = listener.local_addr().expect("get mock server address");
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let request_count = Arc::new(Mutex::new(0_usize));
            let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

            let handle = tokio::spawn({
                let responses = Arc::clone(&responses);
                let request_count = Arc::clone(&request_count);
                async move {
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            accept_result = listener.accept() => {
                                let (mut socket, _) = accept_result.expect("accept mock connection");
                                let responses = Arc::clone(&responses);
                                let request_count = Arc::clone(&request_count);

                                tokio::spawn(async move {
                                    let mut buffer = [0_u8; 2048];
                                    let _ = socket.read(&mut buffer).await;
                                    *request_count.lock().expect("request count lock") += 1;

                                    let response = responses
                                        .lock()
                                        .expect("mock response queue lock")
                                        .pop_front()
                                        .unwrap_or(MockResponse {
                                            status_line: "HTTP/1.1 500 Internal Server Error",
                                            body: "fallback",
                                            delay: Duration::ZERO,
                                        });

                                    sleep(response.delay).await;

                                    let reply = format!(
                                        "{}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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
                request_count,
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn request_count(&self) -> usize {
            *self.request_count.lock().expect("request count lock")
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

    #[tokio::test]
    async fn retries_retryable_statuses_until_success() {
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 503 Service Unavailable",
                body: "temporary",
                delay: Duration::ZERO,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "healthy",
                delay: Duration::ZERO,
            },
        ])
        .await;
        let client = external_http_client().expect("client builds");

        let response = send_with_retry(client.get(server.url()), "test request")
            .await
            .expect("request succeeds after retry");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn times_out_requests_using_shared_client_policy() {
        let server = MockServer::start(vec![MockResponse {
            status_line: "HTTP/1.1 200 OK",
            body: "slow",
            delay: EXTERNAL_HTTP_REQUEST_TIMEOUT + Duration::from_millis(50),
        }])
        .await;
        let client = external_http_client().expect("client builds");

        let error = send_with_retry(client.get(server.url()), "slow request")
            .await
            .expect_err("slow request should time out");

        let error_text = error.to_string();
        assert!(
            error_text.contains("slow request") || error_text.contains("operation timed out"),
            "unexpected timeout error: {error_text}"
        );
        assert_eq!(server.request_count(), EXTERNAL_HTTP_MAX_ATTEMPTS);
    }
}
