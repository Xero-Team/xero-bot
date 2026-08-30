//! Regression tests for recognizing the App's own output.
//!
//! Two compounding bugs made every "is this mine?" check fail:
//!
//! 1. Callers passed `installation.app_slug` from the webhook payload, but that
//!    property is GitHub's *simple installation* object — id and node_id only —
//!    so the slug was always the empty string.
//! 2. Even with a real slug, the comparison was `login == slug`, while a review
//!    or comment authored by an App has login `slug[bot]`.
//!
//! Together they silently disabled the incremental review (no previous review
//! was ever found, on any engine) and made `@bot r-` always answer "没有可撤回的
//! bot 审批" — even immediately after a successful `r+`.
//!
//! The old test fixtures fabricated `installation.app_slug`, which is why none
//! of this was caught.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::config::Config;
use xero_bot::github::{normalize_login, Client};

const TEST_KEY: &str = include_str!("fixtures/rsa_test_key.txt");

fn test_cfg() -> Config {
    let mut c = Config::from_env();
    c.app_id = "12345".into();
    c.private_key_pem = Some(TEST_KEY.into());
    c.webhook_secret = "whsec".into();
    c.bot_name = "xero-review".into();
    c.app_slug = String::new();
    c
}

fn client_for(server: &MockServer, app_slug: &str) -> Client {
    let crab = octocrab::OctocrabBuilder::new()
        .personal_token("ghp_test")
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    Client {
        crab,
        app_slug: app_slug.into(),
    }
}

#[test]
fn normalize_login_strips_bot_suffix() {
    for input in ["x[bot]", "X[BOT]", " x[Bot] ", "x", "X"] {
        assert_eq!(normalize_login(input), "x", "input {input:?}");
    }
    // only a *trailing* suffix is stripped
    assert_eq!(normalize_login("x[bot]y"), "x[bot]y");
    assert_eq!(normalize_login("[bot]"), "");
    assert_eq!(normalize_login(""), "");
}

/// `Client::installation` normalizes whatever slug it's handed, so a caller that
/// passes the `[bot]` form still ends up comparing correctly.
#[tokio::test]
async fn client_normalizes_the_slug_it_is_given() {
    let cfg = test_cfg();
    let c = Client::installation(&cfg, 42, "xero-review[bot]").unwrap();
    assert_eq!(c.app_slug, "xero-review");
}

/// The production failure: a review authored by `xero-review[bot]` must be
/// recognized as ours. Before the fix this returned an empty list, so the
/// incremental review had no memory of the previous round.
#[tokio::test]
async fn own_previous_reviews_matches_bot_suffixed_author() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "state": "COMMENTED", "body": "human note",
             "user": {"login": "alice"}},
            {"id": 2, "state": "COMMENTED", "body": "## 🤖 AI Code Review\nfindings",
             "user": {"login": "xero-review[bot]"}},
        ])))
        .mount(&server)
        .await;

    let gh = client_for(&server, "xero-review");
    let mine = gh
        .own_previous_reviews("octocat/hello", 7)
        .await
        .expect("listing reviews");

    assert_eq!(
        mine.len(),
        1,
        "must find the bot's own review, got {mine:?}"
    );
    assert_eq!(mine[0].get("id").and_then(|i| i.as_i64()), Some(2));
}

/// An empty slug must not match every review — it should match none, loudly.
#[tokio::test]
async fn own_previous_reviews_with_empty_slug_matches_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "state": "COMMENTED", "body": "x", "user": {"login": ""}},
        ])))
        .mount(&server)
        .await;

    let gh = client_for(&server, "");
    let mine = gh.own_previous_reviews("octocat/hello", 7).await.unwrap();
    assert!(
        mine.is_empty(),
        "empty slug must match nothing, got {mine:?}"
    );
}

/// `r-` must find and dismiss the approval the bot posted for `r+`. Before the
/// fix this always replied "没有可撤回的 bot 审批" and dismissed nothing.
#[tokio::test]
async fn reject_dismisses_bot_approval() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 99, "state": "APPROVED", "body": "✅ Approved on behalf of @alice",
             "user": {"login": "xero-review[bot]"}},
        ])))
        .mount(&server)
        .await;
    // The assertion that matters: the dismissal is actually issued.
    Mock::given(method("PUT"))
        .and(path("/repos/octocat/hello/pulls/7/reviews/99/dismissals"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 99})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(&server)
        .await;

    let gh = client_for(&server, "xero-review");
    let cfg = test_cfg();
    let ctx = xero_bot::handlers::CommentContext {
        repo: "octocat/hello".into(),
        pr_number: 7,
        commenter: "alice".into(),
        pr_author: "bob".into(),
        installation_id: 42,
        is_pr: true,
        lang: xero_bot::lang::Lang::Zh,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Reject],
        vec![],
    )
    .await;
    assert_eq!(
        results,
        vec!["ok"],
        "r- must dismiss, not report nothing found"
    );
    // wiremock verifies .expect(1) on drop
}

/// `APP_SLUG` short-circuits the `GET /app` round trip. Serverless builds a
/// fresh process per invocation, so the override is the zero-latency path.
#[tokio::test]
async fn configured_app_slug_is_used_without_calling_the_api() {
    let server = MockServer::start().await;
    // Any /app call would 500; reaching it fails the test's intent.
    Mock::given(method("GET"))
        .and(path("/app"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let mut cfg = test_cfg();
    cfg.app_slug = "configured-name[bot]".into();
    let slug = xero_bot::github::resolve_app_slug(&cfg).await;
    assert_eq!(slug, "configured-name");
}
