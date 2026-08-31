//! Regression tests for secrets reaching a public thread.
//!
//! A 401 from the AI provider was reported into the PR as
//! `AI request failed (401 Unauthorized) to https://api.example.ai/v1/responses: …`,
//! publishing the endpoint the operator had configured. The report is assembled
//! from provider output in many places — and `reqwest` errors carry the URL on
//! their own — so the guarantee under test is the boundary one: nothing posted
//! to GitHub carries a registered secret, whatever built the string.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::config::Config;
use xero_bot::github::Client;

const TEST_KEY: &str = include_str!("fixtures/rsa_test_key.txt");
const REPO: &str = "octocat/hello";
const ENDPOINT: &str = "https://ai-relay.internal.example/v1";
const HOST: &str = "ai-relay.internal.example";
const KEY: &str = "sk-RELsupersecretkeyvalue0123456789";

/// Registers the secrets, the way `Config::from_env` does in production.
fn cfg_with_secrets() -> Config {
    let mut c = Config::from_env();
    c.app_id = "12345".into();
    c.private_key_pem = Some(TEST_KEY.into());
    c.ai_base_url = ENDPOINT.into();
    c.ai_api_key = KEY.into();
    c.ai_model = "gpt-5.6-terra".into();
    c.api_format = "responses".into();
    // The fields were set after `from_env` registered, so register again — the
    // call is idempotent and this is what any config mutation owes.
    xero_bot::redact::register(&c);
    c
}

fn client_for(server: &MockServer) -> Client {
    let crab = xero_bot::github::client_builder()
        .personal_token("ghp_test")
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    Client {
        crab,
        app_slug: "xero-review".into(),
    }
}

/// Every body the mock GitHub received, concatenated.
async fn bodies(server: &MockServer) -> String {
    server
        .received_requests()
        .await
        .expect("recording enabled")
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The exact comment that leaked, posted through the real path.
#[tokio::test]
async fn a_failure_comment_does_not_publish_the_ai_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/4/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(&server)
        .await;

    let _cfg = cfg_with_secrets();
    let gh = client_for(&server);
    let leaked = format!(
        "## 🤖 AI Code Review\n\n❌ 审查出错: `AI request failed (401 Unauthorized) to \
         {ENDPOINT}/responses: {{\"error\":{{\"message\":\"Invalid token\"}}}}`"
    );
    gh.post_issue_comment(REPO, 4, &leaked)
        .await
        .expect("comment posted");

    let sent = bodies(&server).await;
    assert!(!sent.contains(HOST), "endpoint published: {sent}");
    assert!(sent.contains("<AI_ENDPOINT>"), "{sent}");
    // The diagnosis a reader actually needs must survive the scrubbing.
    assert!(sent.contains("401 Unauthorized"), "{sent}");
    assert!(sent.contains("Invalid token"), "{sent}");
}

/// The API key is worse than the endpoint, and travels the same route.
#[tokio::test]
async fn a_comment_does_not_publish_the_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/4/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(&server)
        .await;

    let _cfg = cfg_with_secrets();
    let gh = client_for(&server);
    gh.post_issue_comment(REPO, 4, &format!("❌ sent `Authorization: Bearer {KEY}`"))
        .await
        .expect("comment posted");

    let sent = bodies(&server).await;
    assert!(!sent.contains(KEY), "key published: {sent}");
    assert!(sent.contains("<AI_API_KEY>"), "{sent}");
}

/// `post_review` is the path that usually succeeds, so it can't lean on the
/// plain-comment fallback's scrubbing — and inline bodies are model text.
#[tokio::test]
async fn a_review_scrubs_both_the_summary_and_the_inline_comments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/4/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&server)
        .await;

    let _cfg = cfg_with_secrets();
    let gh = client_for(&server);
    let inline = vec![json!({
        "path": "src/main.rs",
        "line": 12,
        "side": "RIGHT",
        "body": format!("this calls {ENDPOINT}/responses with {KEY}"),
    })];
    gh.post_review(REPO, 4, &format!("summary mentioning {ENDPOINT}"), inline)
        .await
        .expect("review posted");

    let sent = bodies(&server).await;
    assert!(!sent.contains(HOST), "endpoint published: {sent}");
    assert!(!sent.contains(KEY), "key published: {sent}");
    // The inline comment survived as a comment; only its secrets are gone.
    assert!(
        sent.contains("this calls <AI_ENDPOINT>/responses"),
        "{sent}"
    );
}

/// The shape-based pass at the same boundary: a git credential nobody
/// registered, quoted back by a subprocess engine.
#[tokio::test]
async fn a_github_token_is_scrubbed_without_being_registered() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/4/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(&server)
        .await;

    let _cfg = cfg_with_secrets();
    let gh = client_for(&server);
    gh.post_issue_comment(
        REPO,
        4,
        "fatal: unable to access \
         'https://x-access-token:ghs_abcdefghijklmnopqrstuvwxyz012345@github.com/o/r.git/': 403",
    )
    .await
    .expect("comment posted");

    let sent = bodies(&server).await;
    assert!(!sent.contains("ghs_"), "token published: {sent}");
    // The whole `user:pass@` is replaced, not just the token — that is
    // `redact_any`'s URL rule, and the repository it failed on still reads.
    assert!(sent.contains("https://***@github.com/o/r.git"), "{sent}");
}
