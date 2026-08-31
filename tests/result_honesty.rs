//! Regression tests for reporting what GitHub actually did.
//!
//! A cluster of handlers reported success from the fact that a request returned
//! without an error, which is a different claim from the one they made to the
//! user. Three shapes of that mistake are covered here:
//!
//! 1. **Silently permissive endpoints.** `POST .../assignees` answers 201 and
//!    simply omits a login it won't assign, so `assign` and `claim` confirmed
//!    assignments that never happened. `DELETE .../assignees` answers the same
//!    200 whether it removed anyone or not, so `unclaim` told users who were
//!    never assigned that their assignment was released.
//! 2. **The wrong endpoint.** `r?` called only `assignees` while replying that a
//!    review had been requested — a different relation, and the one a
//!    required-review rule actually counts.
//! 3. **Retrying a request that may have succeeded.** `post_review` retried
//!    indiscriminately; on a timeout or 5xx that produces two reviews, and
//!    nobody can tell which is current.

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::commands::Command;
use xero_bot::config::Config;
use xero_bot::github::{Client, ReviewPostMode};
use xero_bot::handlers::{handle_comment, CommentContext};
use xero_bot::lang::Lang;

const TEST_KEY: &str = include_str!("fixtures/rsa_test_key.txt");
const REPO: &str = "octocat/hello";

fn test_cfg() -> Config {
    let mut c = Config::from_env();
    c.app_id = "12345".into();
    c.private_key_pem = Some(TEST_KEY.into());
    c.webhook_secret = "whsec".into();
    c.bot_name = "xero-review".into();
    c.app_slug = "xero-review".into();
    c
}

/// Built through the production builder, so the retry policy under test is the
/// shipped one. Octocrab's default replays POSTs on a 5xx, which is what
/// `review_500_is_not_retried_or_downgraded` is really guarding against.
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

/// `commenter` is who typed the command; `is_pr` decides whether the reviewers
/// endpoint exists at all.
fn ctx(commenter: &str, is_pr: bool) -> CommentContext {
    CommentContext {
        repo: REPO.into(),
        pr_number: 7,
        commenter: commenter.into(),
        pr_author: "bob".into(),
        installation_id: 42,
        is_pr,
        // English so the assertions can read the reply.
        lang: Lang::En,
    }
}

/// Any reply is fine; individual tests that care mount their own.
async fn allow_comments(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// assign / claim
// ---------------------------------------------------------------------------

/// The production shape of the bug: 201, empty `assignees`, "✅ Assigned".
#[tokio::test]
async fn assign_reports_the_assignment_github_ignored() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/assignees")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"assignees": []})))
        .expect(1)
        .mount(&server)
        .await;
    // The reply has to say so rather than claiming the assignment.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("ignored the assignment"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", true),
        vec![Command::Assign {
            user: "carol".into(),
        }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ignored"]);
}

/// And it still says "ok" when the assignment did take — including when GitHub
/// echoes the login back with different capitalization.
#[tokio::test]
async fn assign_confirms_a_real_assignment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/assignees")))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"assignees": [{"login": "Carol"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    allow_comments(&server).await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", true),
        vec![Command::Assign {
            user: "carol".into(),
        }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}

// ---------------------------------------------------------------------------
// unclaim
// ---------------------------------------------------------------------------

/// Nothing to release: say so, and don't issue a DELETE whose result would be
/// indistinguishable from a real removal anyway.
#[tokio::test]
async fn unclaim_of_an_unassigned_user_does_not_delete() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues/7")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"assignees": [{"login": "bob"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/repos/{REPO}/issues/7/assignees")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"assignees": []})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("nothing to release"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", true),
        vec![Command::Unclaim],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["not-assigned"]);
}

#[tokio::test]
async fn unclaim_releases_a_real_assignment() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues/7")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"assignees": [{"login": "alice"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/repos/{REPO}/issues/7/assignees")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"assignees": []})))
        .expect(1)
        .mount(&server)
        .await;
    allow_comments(&server).await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", true),
        vec![Command::Unclaim],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}

/// A removal that left the user assigned is a failure, not a success.
#[tokio::test]
async fn unclaim_notices_a_removal_that_did_not_take() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues/7")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"assignees": [{"login": "alice"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/repos/{REPO}/issues/7/assignees")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"assignees": [{"login": "alice"}]})),
        )
        .mount(&server)
        .await;
    allow_comments(&server).await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", true),
        vec![Command::Unclaim],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["not-removed"]);
}

// ---------------------------------------------------------------------------
// r? — request review
// ---------------------------------------------------------------------------

/// The requested-reviewers endpoint is the one that was never called.
#[tokio::test]
async fn request_review_calls_both_endpoints() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/requested_reviewers")))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"requested_reviewers": [{"login": "carol"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/assignees")))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"assignees": [{"login": "carol"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    allow_comments(&server).await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", true),
        vec![Command::RequestReview {
            user: "carol".into(),
        }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}

/// The two halves fail independently, and one reply reports both.
#[tokio::test]
async fn request_review_reports_each_half_separately() {
    let server = MockServer::start().await;
    // 422: GitHub's way of saying this user can't review the PR.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/requested_reviewers")))
        .respond_with(
            ResponseTemplate::new(422).set_body_json(
                json!({"message": "Reviews may only be requested from collaborators."}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/assignees")))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"assignees": [{"login": "carol"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    // One comment, carrying both outcomes — the explanation for the refusal and
    // the confirmation of the assignment.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("reviewer"))
        .and(body_string_contains("Assigned @carol"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", true),
        vec![Command::RequestReview {
            user: "carol".into(),
        }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["partial"]);
}

/// An issue has no reviewers, so the request is skipped rather than sent to an
/// endpoint that is under `/pulls/` and would always 404.
#[tokio::test]
async fn request_review_on_an_issue_only_assigns() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/requested_reviewers")))
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/assignees")))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"assignees": [{"login": "carol"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("an issue has no reviewers"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", false),
        vec![Command::RequestReview {
            user: "carol".into(),
        }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}

// ---------------------------------------------------------------------------
// post_review
// ---------------------------------------------------------------------------

fn inline() -> Vec<serde_json::Value> {
    vec![json!({"path": "src/main.rs", "line": 3, "body": "note"})]
}

/// Every published review carries the marker that makes it findable later.
#[tokio::test]
async fn review_body_carries_the_marker() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .and(body_string_contains("xero-bot-review"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let mode = gh
        .post_review(REPO, 7, "summary", inline())
        .await
        .expect("posting");
    assert_eq!(mode, ReviewPostMode::Full);
    assert_eq!(mode.to_string(), "ok");
}

/// 422 means the request was definitively rejected, so no review exists and a
/// second POST cannot duplicate one. The inline comments are the usual cause
/// (a line outside the diff), so they are what gets dropped.
#[tokio::test]
async fn review_422_retries_without_the_inline_comments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .and(body_string_contains("src/main.rs"))
        .respond_with(
            ResponseTemplate::new(422)
                .set_body_json(json!({"message": "line must be part of the diff"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .and(body_string_contains("\"comments\":[]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let mode = gh
        .post_review(REPO, 7, "summary", inline())
        .await
        .expect("retry without inline");
    assert_eq!(mode, ReviewPostMode::InlineDropped);
    // The status has to differ from a full success: the inline comments are
    // gone, and "ok" hid that.
    assert_eq!(mode.to_string(), "ok-no-inline");
}

/// 403/404: the reviews endpoint isn't available to us. The findings still are,
/// so they go out as a plain comment rather than being discarded.
#[tokio::test]
async fn review_403_falls_back_to_a_plain_comment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"message": "Resource not accessible by integration"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Marked, so the next incremental run can still recognize it as ours even
    // though it is not in the reviews list.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("xero-bot-review"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let mode = gh
        .post_review(REPO, 7, "summary", inline())
        .await
        .expect("comment fallback");
    assert_eq!(mode, ReviewPostMode::PlainComment);
    assert_eq!(mode.to_string(), "ok-as-comment");
}

/// The one that matters most: a 500 may mean the review *was* created. Retrying
/// or falling back would publish a second one, and then nobody can tell which
/// of the two is current — worse than reporting the failure.
#[tokio::test]
async fn review_500_is_not_retried_or_downgraded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(0)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let err = gh
        .post_review(REPO, 7, "summary", inline())
        .await
        .expect_err("a 500 must surface");
    assert!(format!("{err}").contains("500"), "{err}");
    // wiremock verifies both expectations on drop
}

// ---------------------------------------------------------------------------
// r- — dismissal
// ---------------------------------------------------------------------------

/// Every dismissal failing was reported as "withdrew 0 approval(s)" with status
/// `ok`, while the approval was still standing.
#[tokio::test]
async fn reject_reports_error_when_every_dismissal_fails() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 99, "state": "APPROVED", "body": "approved",
             "user": {"login": "xero-review[bot]"}},
        ])))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews/99/dismissals")))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("still standing"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let results = handle_comment(
        &gh,
        &test_cfg(),
        &ctx("alice", true),
        vec![Command::Reject],
        vec![],
    )
    .await;
    assert_eq!(results.len(), 1);
    assert!(
        results[0].starts_with("error"),
        "a failed withdrawal must not report ok, got {:?}",
        results[0]
    );
}
