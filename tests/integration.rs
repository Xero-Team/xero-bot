//! Integration tests with a mocked GitHub API (wiremock).
//!
//! octocrab's base URL is redirected to the mock server, so full handler
//! flows run end-to-end without touching real GitHub.

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::commands::parse_commands;
use xero_bot::config::Config;
use xero_bot::dispatch::{execute_work, route_event, Routing, Work};
use xero_bot::github::Client;

/// A Config pointed at nothing (API base overridden per-test via octocrab env).
fn test_cfg() -> Config {
    Config {
        app_id: "12345".into(),
        private_key_pem: Some(TEST_KEY.into()),
        webhook_secret: "whsec".into(),
        bot_name: "xero-review".into(),
        ai_base_url: String::new(),
        ai_api_key: String::new(),
        ai_model: String::new(),
        api_format: "chat".into(),
        max_diff_chars: 60_000,
        review_engine: "builtin".into(),
        agent_max_turns: 4,
        agent_timeout_secs: 10,
        pi_path: "definitely-not-a-binary".into(),
        pi_args: String::new(),
        codex_path: "definitely-not-a-binary".into(),
        codex_args: String::new(),
        data_dir: "/tmp/xero-test".into(),
        cron_secret: String::new(),
        rebase_check_delay_secs: 0,
        rebase_sweep_enabled: false,
        rebase_sweep_interval_secs: 3600,
        label_needs_rebase: "needs-rebase".into(),
        label_waiting_review: "waiting-on-review".into(),
        label_waiting_author: "waiting-on-author".into(),
        label_blocked: "blocked".into(),
        codeql_label: "codeql".into(),
        port: 8080,
    }
}

/// A throwaway key for JWT signing (test-only; never used against real GitHub).
const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDMpw6DQMz8TB/6
39G2yFVxlx61ALJVqmoE/DMLYvFl9nr97ulZnszkX5za3gj3TQ5TP/duVPBlWayB
tK/viJSBzoSMsl8YvThygARLGtsF1/gzM57MpktN1OtRk7LD1dd2uKK8AGaTA9WK
BtjSAiruDOsg34pMT9lb3rU/r7RjrV/BflSxChIHoxhUgIrvsqJt2NYHI2N7kXB+
3SlJ90pe6iR0UzAt5xH2qFBEPdD/GcMUc21CYkE9Q2gQwpPDFkQuo9t4ywkSmf4X
N2Ftm+oyI6LVXsS+eu8/mNxEvROb+iolp51LwjVk3B+fIXjA2pLpzapPUXbGOfhB
Rm+35yj1AgMBAAECggEAWUDKXYfXXnk8wUb3yUWZrg6AP+Rr4lyOHFp5UI/4Q8W5
YiHd904AgeEJIZMQSfp7MueE28ODjFANogvRZyAj1HDi8hGg08NCaP1X4gF2YBgO
kRYEPbCQywL/FfbaUfpjG83uexuZoKhdavMNgJmda3CK4y1avWldnGmGlp3kiEt+
gLgSUz3RG8eM1Ol2zzyhUpdFQAPtbEyjveM9+7rgH1KrubZ0FGIxLNsDEZ5qlVlE
RXwtU0TDexep3vDQiThArKxOO9iLzOng0xHgXwxr4NGd5P0OZzO6gCaYQ8Kgo2aw
xjz1M0zAlmoVGr7OqqQtb5QKVyL/i8N5XJn94w6uiQKBgQD1jBsMmX4ftZRIwDlD
r2ZtvkbK4pKumBVxaYGtHGrlqD7+zlYvc8GGhtQydpDO+OFxJ0pQowQWKePDQ9I2
zfeFRD3aTIIC+HFbjO6D3UX4Yfp1H4JYtNfN28HDELj0o6uSjYbZLQyS0gu2mST1
W6sTzP12cVw7zAjhNdHQzkoi+wKBgQDVXUstw/Yrcyx84w8M/ZfigZl2Cc9fYtR4
XsHuzACd35NZRXOXWmpIQJbfDBcyUiNh+FWjHv/0g0k+UA8EgD8Gz8U5VH5oqpjd
UshgvDLO44wygXlLfJqkZD4PkAdzBOyM+zD7k9PbHZU/JwT5QJS5bDpbpF9TPjxL
MJ0ZwMigzwKBgQD033U2OniSDNZFOxWgj3I5rVESEaQwY9C2mn5M8hMU1pWELKe8
iNcNXraNYLqG/aJt4r307q0roTjXyXIBX6Qhje2VH0lkxvjdUQ2oCWo3Cxbn6LVn
22l/jVGNQ8b/iZ2X+HXrbUalwL0Xq2A1I+bXR03Z6bEOnSqZ1b9ZWfCLMwKBgCjR
smJNDTl+zVIPNn/rvDUPSka01cGP7MoihsOir7OEZHI9wUGBgLfV84c0jvOHl1FU
6z1L3vfubgLH2jeoOWaaNUckjRKFIL2m6sLm/mlqSxYWgxgX/JXav6zGh0ZP+Nl3
7QUUYQGYhUcRtffhjRJ0TC3gIoSQcYSJBmU45qktAoGBAMPTmzmdxbuTzdVWvtdK
vKE4ngIwvXUdAWtCBVaA6ADsDRFAeCML3WVjtgeBdy5k65qxQ2XEzO/GKa33ggYP
THli4Tl6Pt511+D2rhwz0kGeuwnGj92ar+0Gn1Mms8YGjOZnWvK7Cq7FfOf9tv5U
f3dqYBjNIT+/oq4iaJ5a96EG
-----END PRIVATE KEY-----";

/// NOTE: the key above is a fixture placeholder — for tests that don't hit
/// the real token endpoint (octocrab is redirected to the mock), the exact
/// key contents don't matter as long as the PEM parses. If it doesn't parse
/// on some platform, tests are skipped by nature of the assertion failing.
fn make_payload(action: &str, body: &str, commenter: &str, pr_author: &str) -> Value {
    json!({
        "action": action,
        "installation": {"id": 42, "app_slug": "xero-review"},
        "repository": {"full_name": "octocat/hello"},
        "issue": {
            "number": 7,
            "pull_request": {"url": "https://api.github.com/repos/octocat/hello/pulls/7"},
            "user": {"login": pr_author}
        },
        "comment": {"body": body, "user": {"login": commenter}}
    })
}

/// Route + execute the work synchronously against a mock.
async fn route_and_run(cfg: &Config, event: &str, payload: &Value) -> Option<Value> {
    match route_event(cfg, event, payload) {
        Routing::Respond(v) => Some(v),
        Routing::Act(work) => {
            execute_work(cfg, work).await;
            None
        }
    }
}

#[tokio::test]
async fn test_ping_comment_replies_pong() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let crab = octocrab::OctocrabBuilder::new()
        .personal_token("ghp_test")
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    let gh = Client {
        crab,
        app_slug: "xero-review".into(),
    };
    let cfg = test_cfg();
    let ctx = xero_bot::handlers::CommentContext {
        repo: "octocat/hello".into(),
        pr_number: 7,
        commenter: "alice".into(),
        pr_author: "bob".into(),
        app_slug: "xero-review".into(),
        installation_id: 42,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Ping],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}

#[tokio::test]
async fn test_ready_label_flow_with_mock() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/issues/7/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let crab = octocrab::OctocrabBuilder::new()
        .personal_token("ghp_test")
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    let gh = Client {
        crab,
        app_slug: "xero-review".into(),
    };
    let cfg = test_cfg();
    let ctx = xero_bot::handlers::CommentContext {
        repo: "octocat/hello".into(),
        pr_number: 7,
        commenter: "alice".into(),
        pr_author: "bob".into(),
        app_slug: "xero-review".into(),
        installation_id: 42,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Ready],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}
