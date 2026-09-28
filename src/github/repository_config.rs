//! Read-only configuration HTTP transport. Keeps status/ETag/Retry-After intact.
use std::time::Duration;

use http_body_util::BodyExt;
use serde_json::Value;

use super::Client;
use crate::config::repository::{Problem, ReasonCode};

#[derive(Debug, Clone)]
pub struct FetchError {
    pub problem: Problem,
    pub retry_after_secs: u64,
}
impl FetchError {
    /// Create a typed read failure with the minimum fifteen-second retry delay.
    pub(crate) fn new(code: ReasonCode, detail: &'static str) -> Self {
        Self {
            problem: Problem::new(code, detail),
            retry_after_secs: 15,
        }
    }
}

pub(crate) struct Response {
    pub status: u16,
    pub etag: Option<String>,
    pub value: Value,
}

/// Use the longest of the minimum backoff, Retry-After seconds/date and rate-limit reset.
fn retry_after(headers: &http::HeaderMap) -> u64 {
    let now = chrono::Utc::now().timestamp();
    let retry = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<u64>().ok().or_else(|| {
                chrono::DateTime::parse_from_rfc2822(v)
                    .ok()
                    .map(|d| d.timestamp().saturating_sub(now).max(0) as u64)
            })
        })
        .unwrap_or(0);
    let reset = if headers
        .get("x-ratelimit-remaining")
        .is_some_and(|v| v == "0")
    {
        headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .map(|v| v.saturating_sub(now).max(0) as u64)
            .unwrap_or(0)
    } else {
        0
    };
    retry.max(reset).max(15)
}

impl Client {
    /// Perform one bounded configuration GET, retaining status and conditional-cache headers.
    /// Only 200 bodies are decoded; 304/404 interpretation belongs to the snapshot loader.
    /// Errors omit remote bodies and include retry guidance without replaying requests.
    pub(crate) async fn config_get(
        &self,
        route: &str,
        etag: Option<&str>,
    ) -> Result<Response, FetchError> {
        let operation = async {
            let mut request = http::Request::builder()
                .method("GET")
                .uri(route)
                .header("Accept", "application/vnd.github+json");
            if let Some(etag) = etag {
                request = request.header("If-None-Match", etag);
            }
            let request = request.body(Vec::<u8>::new()).map_err(|_| {
                FetchError::new(ReasonCode::InvalidResponse, "invalid configuration request")
            })?;
            let response = self.crab.execute(request).await.map_err(|_| {
                FetchError::new(ReasonCode::Transport, "GitHub configuration request failed")
            })?;
            let status = response.status().as_u16();
            let retry = retry_after(response.headers());
            if !matches!(status, 200 | 304 | 404) {
                let rate_limited = status == 429
                    || (status == 403
                        && (response.headers().contains_key("retry-after")
                            || response
                                .headers()
                                .get("x-ratelimit-remaining")
                                .is_some_and(|v| v == "0")));
                let code = if rate_limited {
                    ReasonCode::RateLimited
                } else if status == 403 {
                    ReasonCode::Forbidden
                } else {
                    ReasonCode::ApiFailure
                };
                return Err(FetchError {
                    problem: Problem::new(code, "GitHub refused configuration revalidation"),
                    retry_after_secs: retry,
                });
            }
            let etag = response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let value = if status == 200 {
                // Limit configuration/metadata bodies independently of Content-Length.
                let bytes = http_body_util::Limited::new(response.into_body(), 2 * 1024 * 1024)
                    .collect()
                    .await
                    .map_err(|_| {
                        FetchError::new(
                            ReasonCode::InvalidResponse,
                            "configuration response is incomplete or too large",
                        )
                    })?
                    .to_bytes();
                serde_json::from_slice(&bytes).map_err(|_| {
                    FetchError::new(
                        ReasonCode::InvalidResponse,
                        "invalid GitHub configuration response",
                    )
                })?
            } else {
                Value::Null
            };
            Ok(Response {
                status,
                etag,
                value,
            })
        };
        tokio::time::timeout(Duration::from_secs(15), operation)
            .await
            .map_err(|_| {
                FetchError::new(
                    ReasonCode::Transport,
                    "GitHub configuration request timed out",
                )
            })?
    }
}
