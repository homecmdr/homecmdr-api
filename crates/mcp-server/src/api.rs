/// Thin async HTTP client that adds a Bearer token to every request.
pub struct ApiClient {
    inner: reqwest::Client,
    base_url: String,
}

impl ApiClient {
    pub fn new(base_url: impl Into<String>, token: impl AsRef<str>) -> anyhow::Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.as_ref()))
                .map_err(|e| anyhow::anyhow!("invalid token: {e}"))?,
        );
        let inner = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;
        Ok(Self {
            inner,
            base_url: base_url.into(),
        })
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.inner.get(&url).send().await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("GET {path} returned {status}: {body}");
        }
        Ok(body)
    }

    pub async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.inner.post(&url).json(body).send().await?;
        let status = resp.status();
        let resp_body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("POST {path} returned {status}: {resp_body}");
        }
        Ok(resp_body)
    }

    pub async fn get_text(&self, path: &str) -> anyhow::Result<String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.inner.get(&url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("not found: {path}");
        }
        if !status.is_success() {
            anyhow::bail!("GET {path} returned {status}");
        }
        Ok(resp.text().await?)
    }

    pub async fn put_json(&self, path: &str, body: &serde_json::Value) -> anyhow::Result<()> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.inner.put(&url).json(body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            anyhow::bail!("PUT {path} returned {status}: {msg}");
        }
        Ok(())
    }
}
