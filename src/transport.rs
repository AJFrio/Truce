//! Shared OpenRouter HTTP stack: rustls, bearer auth, and the retry loop.
//!
//! The API key is stored only here. Error strings pass through [`redact`]
//! before they leave this module.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::StatusCode;
use serde::Serialize;

use crate::config::Settings;
use crate::error::{Result, TruceError};

struct Inner {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    timeout: Duration,
    retries: u32,
}

/// Cloneable HTTP client. `Debug` masks the API key.
#[derive(Clone)]
pub(crate) struct Transport {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport")
            .field("base_url", &self.inner.base_url)
            .field("api_key", &"***")
            .finish()
    }
}

impl Transport {
    pub(crate) fn new(settings: &Settings) -> Result<Self> {
        let api_key = settings.api_key.clone().ok_or(TruceError::MissingApiKey)?;
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .map_err(|err| TruceError::OpenRouter(redact(&err.to_string(), &api_key)))?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                api_key,
                base_url: settings.base_url.trim_end_matches('/').to_string(),
                timeout: settings.timeout,
                retries: settings.retries,
            }),
        })
    }

    pub(crate) fn redact_text(&self, text: &str) -> String {
        redact(text, &self.inner.api_key)
    }

    /// `POST {base_url}/{endpoint}` with a JSON body.
    ///
    /// 401 and 403 are [`TruceError::OpenRouterAuth`] and are not retried.
    /// 429, 5xx, timeouts, and connect errors use jittered exponential
    /// backoff and honor `Retry-After` (capped at 60s). Other 4xx responses
    /// fail immediately.
    pub(crate) async fn post_json(&self, endpoint: &str, body: &impl Serialize) -> Result<String> {
        let url = join_endpoint(&self.inner.base_url, endpoint);
        let mut last = String::from("OpenRouter request failed");
        let attempts = self.inner.retries.saturating_add(1);

        for attempt in 0..attempts {
            let send = self
                .inner
                .http
                .post(&url)
                .bearer_auth(&self.inner.api_key)
                .header("HTTP-Referer", "https://github.com/AJFrio/Truce")
                .header("X-Title", "Truce")
                .json(body)
                .send();

            let response = match tokio::time::timeout(self.inner.timeout, send).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(err)) => {
                    last = self.redact_text(&err.to_string());
                    if err.is_timeout() || err.is_connect() || err.is_request() {
                        sleep_backoff(attempt, None).await;
                        continue;
                    }
                    return Err(TruceError::OpenRouter(last));
                }
                Err(_) => {
                    last = format!("request timed out after {:?}", self.inner.timeout);
                    sleep_backoff(attempt, None).await;
                    continue;
                }
            };

            let status = response.status();
            let headers = response.headers().clone();
            let text = response
                .text()
                .await
                .unwrap_or_else(|err| self.redact_text(&err.to_string()));
            let text = self.redact_text(&text);

            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(TruceError::OpenRouterAuth);
            }

            if status.as_u16() == 429 || status.is_server_error() {
                last = format!("HTTP {status}: {text}");
                sleep_backoff(attempt, retry_after(&headers)).await;
                continue;
            }

            if !status.is_success() {
                return Err(TruceError::OpenRouter(format!("HTTP {status}: {text}")));
            }

            return Ok(text);
        }

        Err(TruceError::OpenRouter(format!("retries exhausted: {last}")))
    }
}

pub(crate) fn join_endpoint(base: &str, endpoint: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        endpoint.trim_start_matches('/')
    )
}

/// Replace every occurrence of `key` (and therefore `Bearer {key}`) in `text`.
pub fn redact(text: &str, key: &str) -> String {
    if key.is_empty() {
        return text.to_string();
    }
    text.replace(key, "***")
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    raw.parse::<u64>()
        .ok()
        .map(|secs| Duration::from_secs(secs.min(60)))
}

async fn sleep_backoff(attempt: u32, retry_after: Option<Duration>) {
    if let Some(delay) = retry_after {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        return;
    }
    let base_ms = 200u64.saturating_mul(1u64 << attempt.min(5));
    let jitter = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        nanos % 200
    };
    let delay = Duration::from_millis((base_ms + jitter).min(8_000));
    tokio::time::sleep(delay).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_base_and_endpoint() {
        assert_eq!(
            join_endpoint("http://127.0.0.1:9/api/v1", "systemone"),
            "http://127.0.0.1:9/api/v1/systemone"
        );
        assert_eq!(
            join_endpoint("http://127.0.0.1:9/api/v1/", "/chat/completions"),
            "http://127.0.0.1:9/api/v1/chat/completions"
        );
    }

    #[test]
    fn redact_strips_key() {
        let key = "sk-or-v1-SECRETVALUE";
        let out = redact(&format!("Authorization Bearer {key} failed"), key);
        assert!(!out.contains("SECRETVALUE"), "{out}");
        assert!(out.contains("***"), "{out}");
    }
}
