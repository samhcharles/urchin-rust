use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use urchin_core::event::Event;

/// Structured HTTP error returned by the remote end.
/// Callers can downcast an `anyhow::Error` to this type to inspect status + body.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub body: serde_json::Value,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {} — {}", self.status, self.body)
    }
}

impl std::error::Error for HttpError {}

/// HTTP client for the local Urchin daemon or a remote Cloud Hub.
pub struct UrchinClient {
    base_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl UrchinClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: None,
            http: reqwest::Client::new(),
        }
    }

    /// Attach a Bearer token to every request.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Connect to the default local daemon on port 18799.
    pub fn local() -> Self {
        Self::new("http://127.0.0.1:18799")
    }

    /// POST an event to /ingest. Returns the recorded event id on success.
    /// On an HTTP error response, the error downcasts to `HttpError`.
    pub async fn ingest(&self, event: &Event) -> Result<String> {
        let url = format!("{}/ingest", self.base_url);
        let mut req = self.http.post(&url).json(event);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.context("failed to reach Urchin daemon")?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("non-JSON response from daemon")?;

        if !status.is_success() {
            return Err(anyhow::Error::new(HttpError {
                status: status.as_u16(),
                body,
            }));
        }

        Ok(body["id"].as_str().unwrap_or("ok").to_string())
    }

    /// Pull events from the remote cloud hub since `after_cursor`.
    ///
    /// `after_cursor` is an ISO 8601 timestamp string (exclusive lower bound).
    /// Pass `None` to fetch from the beginning.
    /// Returns `PullResponse { events, next_cursor }`.
    pub async fn pull(&self, after_cursor: Option<&str>, limit: usize) -> Result<PullResponse> {
        let mut url = format!("{}/api/urchin-sync/events?limit={}", self.base_url, limit);
        if let Some(cursor) = after_cursor {
            let encoded = utf8_percent_encode(cursor, NON_ALPHANUMERIC);
            url.push_str(&format!("&after={}", encoded));
        }

        let mut req = self.http.get(&url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.context("failed to reach cloud hub")?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .context("non-JSON response from cloud hub")?;

        if !status.is_success() {
            return Err(anyhow::Error::new(HttpError {
                status: status.as_u16(),
                body,
            }));
        }

        let events: Vec<Event> = serde_json::from_value(body["events"].clone())
            .context("failed to deserialize events array")?;

        let next_cursor = body["next_cursor"].as_str().map(|s| s.to_string());

        Ok(PullResponse {
            events,
            next_cursor,
        })
    }

    /// Return a fluent builder pre-wired to this client.
    pub fn builder(&self) -> crate::builder::EventBuilder<'_> {
        crate::builder::EventBuilder::new(self)
    }
}

/// Response from the cloud pull endpoint.
#[derive(Debug)]
pub struct PullResponse {
    pub events: Vec<Event>,
    /// ISO 8601 timestamp of the last returned event; `None` if no more pages.
    pub next_cursor: Option<String>,
}
