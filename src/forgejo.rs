//! Forgejo provider primitives.
//!
//! Forgejo is deliberately kept separate from the GitHub App client: it uses a
//! personal/access token, a user-configured base URL, and different webhook
//! headers. This module owns those boundaries so command handling and mirror
//! sync cannot accidentally fall back to GitHub identity or endpoints.

use hmac::{Hmac, Mac};
use reqwest::{Method, Response};
use serde_json::Value;
use sha2::Sha256;

pub mod commands;

pub type HmacSha256 = Hmac<Sha256>;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Verify Forgejo's `X-Forgejo-Signature` / `X-Gitea-Signature` header.
///
/// Forgejo sends a bare lowercase hex HMAC. Some compatible deployments and
/// proxies expose the GitHub-style `sha256=` prefix, so both forms are accepted.
/// An empty secret is a configuration error and always fails closed.
pub fn verify_signature(secret: &str, body: &[u8], signature_header: Option<&str>) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(signature) = signature_header.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let signature = signature.strip_prefix("sha256=").unwrap_or(signature);
    let Ok(expected) = hex::decode(signature) else {
        return false;
    };
    let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgejoEvent {
    Ping,
    /// A newly created issue/PR comment. Forgejo numbers issues and PRs from
    /// one sequence, so `number` is the value both endpoint families use.
    IssueComment {
        repo: String,
        number: i64,
        repository_id: Option<i64>,
        issue_id: Option<i64>,
        comment_id: Option<i64>,
        user_id: Option<i64>,
        source_at: Option<String>,
        body: String,
        commenter: String,
        author: String,
        is_pr: bool,
    },
    Push {
        repo: String,
        ref_name: String,
    },
    Ignored(String),
}

fn jstr<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

fn ji64(value: &Value, path: &[&str]) -> Option<i64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_i64()
}

/// Convert a Forgejo webhook into the small set of events the bot consumes.
pub fn classify(event_header: &str, payload: &Value) -> ForgejoEvent {
    match event_header {
        "ping" => ForgejoEvent::Ping,
        "push" => {
            let Some(repo) = jstr(payload, &["repository", "full_name"]) else {
                return ForgejoEvent::Ignored("push without repository".into());
            };
            let Some(ref_name) = jstr(payload, &["ref"]) else {
                return ForgejoEvent::Ignored("push without ref".into());
            };
            ForgejoEvent::Push {
                repo: repo.to_string(),
                ref_name: ref_name.to_string(),
            }
        }
        "issue_comment" => {
            if jstr(payload, &["action"]) != Some("created") {
                return ForgejoEvent::Ignored("action != created".into());
            }
            let Some(repo) = jstr(payload, &["repository", "full_name"]) else {
                return ForgejoEvent::Ignored("comment without repository".into());
            };
            let Some(number) = ji64(payload, &["issue", "number"]) else {
                return ForgejoEvent::Ignored("comment without issue number".into());
            };
            let Some(body) = jstr(payload, &["comment", "body"]) else {
                return ForgejoEvent::Ignored("comment without body".into());
            };
            let Some(commenter) = jstr(payload, &["comment", "user", "login"]) else {
                return ForgejoEvent::Ignored("comment without author".into());
            };
            let author = jstr(payload, &["issue", "user", "login"])
                .unwrap_or("")
                .to_string();
            let is_pr = payload
                .get("issue")
                .and_then(|issue| issue.get("pull_request"))
                .is_some_and(|pr| !pr.is_null());
            ForgejoEvent::IssueComment {
                repo: repo.to_string(),
                number,
                repository_id: ji64(payload, &["repository", "id"]),
                issue_id: ji64(payload, &["issue", "id"]),
                comment_id: ji64(payload, &["comment", "id"]),
                user_id: ji64(payload, &["comment", "user", "id"]),
                source_at: jstr(payload, &["comment", "created_at"]).map(str::to_owned),
                body: body.to_string(),
                commenter: commenter.to_string(),
                author,
                is_pr,
            }
        }
        other => ForgejoEvent::Ignored(format!("unsupported event: {other}")),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ForgejoError {
    #[error("invalid Forgejo configuration: {0}")]
    Config(String),
    #[error("Forgejo request failed: {0}")]
    Transport(String),
    #[error("Forgejo API returned {status}: {message}")]
    Api { status: u16, message: String },
    #[error("invalid Forgejo response: {0}")]
    InvalidResponse(String),
}

/// Minimal token-authenticated Forgejo API transport.
///
/// The client intentionally does not retry writes. A transport failure after a
/// request was sent has an unknown outcome; replaying it can duplicate comments,
/// reviews, or state changes.
#[derive(Clone)]
pub struct ForgejoClient {
    http: reqwest::Client,
    api_base: reqwest::Url,
    token: String,
}

impl ForgejoClient {
    pub fn new(base_url: &str, token: &str) -> Result<Self, ForgejoError> {
        let base_url = base_url.trim();
        let token = token.trim();
        if base_url.is_empty() {
            return Err(ForgejoError::Config("FORGEJO_BASE_URL is empty".into()));
        }
        if token.is_empty() {
            return Err(ForgejoError::Config("FORGEJO_TOKEN is empty".into()));
        }
        let mut url = reqwest::Url::parse(base_url)
            .map_err(|_| ForgejoError::Config("FORGEJO_BASE_URL is not a URL".into()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(ForgejoError::Config(
                "FORGEJO_BASE_URL must be an absolute http(s) URL".into(),
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ForgejoError::Config(
                "FORGEJO_BASE_URL must not contain a query or fragment".into(),
            ));
        }
        let path = url.path().trim_end_matches('/');
        let path = if path.ends_with("/api/v1") {
            path.to_string()
        } else if path.is_empty() {
            "/api/v1".to_string()
        } else {
            format!("{path}/api/v1")
        };
        url.set_path(&format!("{path}/"));
        url.set_query(None);
        url.set_fragment(None);

        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ForgejoError::Config(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            http,
            api_base: url,
            token: token.to_string(),
        })
    }

    fn endpoint(&self, route: &str) -> Result<reqwest::Url, ForgejoError> {
        if !route.starts_with('/') {
            return Err(ForgejoError::Config("API route must start with /".into()));
        }
        self.api_base
            .join(route.trim_start_matches('/'))
            .map_err(|_| ForgejoError::Config("invalid Forgejo API route".into()))
    }

    pub async fn get(&self, route: &str) -> Result<Value, ForgejoError> {
        self.request(Method::GET, route, None).await
    }

    pub async fn post(&self, route: &str, body: Value) -> Result<Value, ForgejoError> {
        self.request(Method::POST, route, Some(body)).await
    }

    pub async fn patch(&self, route: &str, body: Value) -> Result<Value, ForgejoError> {
        self.request(Method::PATCH, route, Some(body)).await
    }

    pub async fn put(&self, route: &str, body: Value) -> Result<Value, ForgejoError> {
        self.request(Method::PUT, route, Some(body)).await
    }

    pub async fn delete(&self, route: &str) -> Result<Value, ForgejoError> {
        self.request(Method::DELETE, route, None).await
    }

    /// Fetch every page of a Forgejo list endpoint. Forgejo uses `page` and
    /// `limit`, unlike GitHub's `per_page` and Link-header pagination.
    pub async fn get_all(&self, route: &str, limit: u32) -> Result<Vec<Value>, ForgejoError> {
        let limit = limit.clamp(1, 50);
        let mut all = Vec::new();
        for page in 1..=100 {
            let separator = if route.contains('?') { '&' } else { '?' };
            let page = self
                .get(&format!("{route}{separator}page={page}&limit={limit}"))
                .await?;
            let page = page.as_array().ok_or_else(|| {
                ForgejoError::InvalidResponse("expected a JSON array in list response".into())
            })?;
            let count = page.len();
            all.extend(page.iter().cloned());
            if count < limit as usize {
                return Ok(all);
            }
        }
        Err(ForgejoError::InvalidResponse(
            "list endpoint exceeded 100 pages".into(),
        ))
    }

    pub async fn current_user(&self) -> Result<Value, ForgejoError> {
        self.get("/user").await
    }

    pub async fn repository(&self, repo: &str) -> Result<Value, ForgejoError> {
        self.get(&format!("/repos/{repo}")).await
    }

    async fn request(
        &self,
        method: Method,
        route: &str,
        body: Option<Value>,
    ) -> Result<Value, ForgejoError> {
        let url = self.endpoint(route)?;
        let mut request = self
            .http
            .request(method, url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, "xero-bot")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("token {}", self.token),
            );
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ForgejoError::Transport(e.to_string()))?;
        decode_response(response).await
    }
}

async fn decode_response(response: Response) -> Result<Value, ForgejoError> {
    let status = response.status().as_u16();
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ForgejoError::Transport(e.to_string()))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ForgejoError::InvalidResponse(
                "response exceeds 2 MiB".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }

    if !(200..300).contains(&status) {
        let message = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.get("error").and_then(Value::as_str))
                .unwrap_or("request rejected")
                .to_string(),
            Err(_) => String::from_utf8_lossy(&bytes)
                .chars()
                .take(512)
                .collect::<String>(),
        };
        return Err(ForgejoError::Api { status, message });
    }

    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| ForgejoError::InvalidResponse("response is not valid JSON".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn webhook_signature_accepts_bare_and_prefixed_hex() {
        let body = br#"{"ok":true}"#;
        let sig = sign("secret", body);
        assert!(verify_signature("secret", body, Some(&sig)));
        assert!(verify_signature(
            "secret",
            body,
            Some(&format!("sha256={sig}"))
        ));
        assert!(!verify_signature("", body, Some(&sig)));
        assert!(!verify_signature(
            "secret",
            body,
            Some(&sign("other", body))
        ));
        assert!(!verify_signature("secret", body, None));
    }

    #[test]
    fn issue_comment_and_push_are_classified() {
        let comment = json!({
            "action": "created",
            "repository": {"full_name": "team/repo"},
            "issue": {
                "id": 4, "number": 7,
                "user": {"login": "author"},
                "pull_request": {"merged": false}
            },
            "comment": {
                "id": 8, "body": "@bot ping", "created_at": "2026-11-03T00:00:00Z",
                "user": {"id": 9, "login": "alice"}
            }
        });
        match classify("issue_comment", &comment) {
            ForgejoEvent::IssueComment {
                repo,
                number,
                commenter,
                author,
                is_pr,
                ..
            } => {
                assert_eq!(repo, "team/repo");
                assert_eq!(number, 7);
                assert_eq!(commenter, "alice");
                assert_eq!(author, "author");
                assert!(is_pr);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let push = json!({"ref":"refs/heads/topic","repository":{"full_name":"team/repo"}});
        assert_eq!(
            classify("push", &push),
            ForgejoEvent::Push {
                repo: "team/repo".into(),
                ref_name: "refs/heads/topic".into()
            }
        );
    }

    #[tokio::test]
    async fn client_uses_api_v1_token_header_and_never_double_prefixes_v1() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/team/repo"))
            .and(header("authorization", "token test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
            .expect(1)
            .mount(&server)
            .await;

        let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
        assert_eq!(
            client.get("/repos/team/repo").await.unwrap(),
            json!({"id": 1})
        );

        let already_prefixed =
            ForgejoClient::new(&format!("{}/api/v1", server.uri()), "another-token").unwrap();
        let err = already_prefixed
            .get("/missing")
            .await
            .expect_err("no mock should be hit");
        assert!(matches!(err, ForgejoError::Api { status: 404, .. }));
    }

    #[tokio::test]
    async fn api_errors_are_bounded_and_classified() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/issues/7/comments"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"message": "repository is archived"})),
            )
            .mount(&server)
            .await;

        let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
        let err = client
            .post("/repos/team/repo/issues/7/comments", json!({"body":"x"}))
            .await
            .expect_err("403 must fail");
        assert!(matches!(
            err,
            ForgejoError::Api { status: 403, ref message } if message == "repository is archived"
        ));
    }
}
