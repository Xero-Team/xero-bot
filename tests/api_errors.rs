//! Regression tests for GitHub API error classification.
//!
//! `classify_octo_error` used to hardcode `status: 0`, which silently disabled
//! every `GhError::Api { status: 403/404/422 }` branch in the codebase — the
//! CodeQL "not enabled" notice, the assign-failure explanation, and the
//! "404 means the label was already absent" checks all stopped matching.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::github::{Client, GhError};

async fn client_for(server: &MockServer) -> Client {
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

/// The exact production failure: an App without `pull_requests` permission gets
/// 403 when commenting on a PR. The status must survive into `GhError`.
#[tokio::test]
async fn comment_403_reports_real_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/comments"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "message": "Resource not accessible by integration"
        })))
        .mount(&server)
        .await;

    let gh = client_for(&server).await;
    let err = gh
        .post_issue_comment("octocat/hello", 7, "pong")
        .await
        .expect_err("403 must surface as an error");

    match err {
        GhError::Api { status, message } => {
            assert_eq!(status, 403, "status must be the real code, not 0");
            assert!(
                message.contains("not accessible"),
                "message should carry GitHub's text, got: {message}"
            );
        }
        other => panic!("expected GhError::Api, got {other:?}"),
    }
}

/// `handlers` treats a 404 from label removal as "already absent". That check
/// is `matches!(e, GhError::Api { status: 404, .. })`, so it only works if the
/// status is extracted.
#[tokio::test]
async fn label_removal_404_reports_real_status() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/repos/octocat/hello/issues/7/labels/gone"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "message": "Label does not exist"
        })))
        .mount(&server)
        .await;

    let gh = client_for(&server).await;
    let err = gh
        .remove_label("octocat/hello", 7, "gone")
        .await
        .expect_err("404 must surface as an error");

    assert!(
        matches!(err, GhError::Api { status: 404, .. }),
        "the 404-means-absent branch must match, got {err:?}"
    );
}

/// A failed command must still be reported as "error" to the caller — the fix
/// adds logging, it must not change the returned labels.
#[tokio::test]
async fn failing_ping_still_labeled_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/comments"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "message": "Resource not accessible by integration"
        })))
        .mount(&server)
        .await;

    let gh = client_for(&server).await;
    let cfg = xero_bot::config::Config::from_env();
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
        vec![xero_bot::commands::Command::Ping],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["error"]);
}
