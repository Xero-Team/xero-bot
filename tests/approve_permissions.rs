//! Who is allowed to make the App approve a PR.
//!
//! An APPROVE review posted by the App is a real approval: a branch-protection
//! rule that requires one counts it and lets the merge button light up. So `r+`
//! is not a comment, it is a privileged write, and every gate below exists
//! because it was missing:
//!
//! 1. **Relayed approvals were free.** `r+ as @teammate` credited the approval
//!    to a colleague with no check at all — not that they had write access, not
//!    even that the login existed. Now the whole feature is behind
//!    `R_PLUS_ALLOW_ON_BEHALF` (default off), and when it is on the credited
//!    login must itself be able to approve.
//! 2. **The author could approve their own PR.** GitHub blocks a real
//!    self-approval, but the App is the review author here, so nothing stopped
//!    it — including via `r+ as @someone-else`, the same act with a detour.
//!
//! The gates are ordered cheapest-first, and the tests assert that ordering by
//! counting requests: a refusal must not cost an API call, so that no refused
//! command can be turned into a permissions oracle or into rate-limit pressure.

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::commands::Command;
use xero_bot::config::Config;
use xero_bot::github::Client;
use xero_bot::handlers::{handle_comment, CommentContext};
use xero_bot::lang::Lang;

const TEST_KEY: &str = include_str!("fixtures/rsa_test_key.txt");
const REPO: &str = "octocat/hello";

/// `on_behalf` is set explicitly rather than left to the environment: the whole
/// point of these tests is which side of the switch we are on.
fn test_cfg(on_behalf: bool) -> Config {
    let mut c = Config::from_env();
    c.app_id = "12345".into();
    c.private_key_pem = Some(TEST_KEY.into());
    c.webhook_secret = "whsec".into();
    c.bot_name = "xero-review".into();
    c.app_slug = "xero-review".into();
    c.r_plus_allow_on_behalf = on_behalf;
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

/// PR #7, authored by `bob`.
fn ctx(commenter: &str) -> CommentContext {
    CommentContext {
        repo: REPO.into(),
        pr_number: 7,
        commenter: commenter.into(),
        pr_author: "bob".into(),
        installation_id: 42,
        is_pr: true,
        lang: Lang::En,
    }
}

async fn allow_comments(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(server)
        .await;
}

/// Nothing may reach `POST /pulls/7/reviews` — the approval itself.
async fn no_approval(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 5})))
        .expect(0)
        .mount(server)
        .await;
}

/// Nothing may ask GitHub about anyone's permissions.
async fn no_permission_queries(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r".*/collaborators/.*/permission$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "admin"})))
        .expect(0)
        .mount(server)
        .await;
}

async fn permission(server: &MockServer, user: &str, level: &str) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{REPO}/collaborators/{user}/permission"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": level})))
        .mount(server)
        .await;
}

async fn approve(
    server: &MockServer,
    cfg: &Config,
    commenter: &str,
    credited: Option<&str>,
) -> String {
    let gh = client_for(server);
    let results = handle_comment(
        &gh,
        cfg,
        &ctx(commenter),
        vec![Command::Approve {
            on_behalf_of: credited.map(str::to_string),
        }],
        vec![],
    )
    .await;
    assert_eq!(results.len(), 1, "one command, one result: {results:?}");
    results.into_iter().next().unwrap()
}

// ---------------------------------------------------------------------------
// Gate 1 — the on-behalf switch
// ---------------------------------------------------------------------------

/// Off by default, and refused before a single request goes out: the gate is
/// pure configuration, so asking GitHub anything first would be wasted work and
/// would leak whether the credited login is a collaborator.
#[tokio::test]
async fn on_behalf_is_refused_when_disabled_without_asking_github_anything() {
    let server = MockServer::start().await;
    no_permission_queries(&server).await;
    no_approval(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("R_PLUS_ALLOW_ON_BEHALF"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let status = approve(&server, &test_cfg(false), "alice", Some("carol")).await;
    assert_eq!(status, "on-behalf-disabled");
}

/// The switch is about *relaying*, so plain `r+` must be untouched by it.
#[tokio::test]
async fn plain_r_plus_still_works_with_the_switch_off() {
    let server = MockServer::start().await;
    allow_comments(&server).await;
    permission(&server, "alice", "write").await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .and(body_string_contains("APPROVE"))
        .and(body_string_contains("@alice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 5})))
        .expect(1)
        .mount(&server)
        .await;

    let status = approve(&server, &test_cfg(false), "alice", None).await;
    assert_eq!(status, "ok");
}

// ---------------------------------------------------------------------------
// Gate 2 — the author
// ---------------------------------------------------------------------------

/// The reported hole: the author routes their own approval through a colleague's
/// name. Refused, and again without spending a request — the check needs only
/// the two logins we already have.
#[tokio::test]
async fn author_cannot_approve_their_own_pr_through_someone_else() {
    let server = MockServer::start().await;
    no_permission_queries(&server).await;
    no_approval(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("self-approval"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    // On-behalf enabled, so the refusal can only be the authorship check.
    let status = approve(&server, &test_cfg(true), "bob", Some("carol")).await;
    assert_eq!(status, "self-approve");
}

#[tokio::test]
async fn author_cannot_approve_their_own_pr_directly() {
    let server = MockServer::start().await;
    allow_comments(&server).await;
    no_permission_queries(&server).await;
    no_approval(&server).await;

    let status = approve(&server, &test_cfg(false), "bob", None).await;
    assert_eq!(status, "self-approve");
}

/// Logins are compared through `normalize_login`, so neither case nor a `[bot]`
/// suffix is a way around the authorship check.
#[tokio::test]
async fn author_check_ignores_case_and_the_bot_suffix() {
    for commenter in ["BOB", "bob[bot]", " Bob "] {
        let server = MockServer::start().await;
        allow_comments(&server).await;
        no_permission_queries(&server).await;
        no_approval(&server).await;

        let status = approve(&server, &test_cfg(false), commenter, None).await;
        assert_eq!(status, "self-approve", "commenter {commenter:?}");
    }
}

/// Crediting the author is the same thing from the other side — a third party
/// hands the author an approval of their own PR.
#[tokio::test]
async fn approval_cannot_be_credited_to_the_author() {
    let server = MockServer::start().await;
    allow_comments(&server).await;
    permission(&server, "alice", "write").await;
    no_approval(&server).await;

    let status = approve(&server, &test_cfg(true), "alice", Some("Bob[bot]")).await;
    assert_eq!(status, "self-approve");
}

// ---------------------------------------------------------------------------
// Gate 3/4 — permissions, on both logins
// ---------------------------------------------------------------------------

/// The commenter's own access is checked before the credited login's, so a
/// read-only user cannot use `r+ as @admin` to probe who is a collaborator.
#[tokio::test]
async fn commenter_without_write_access_is_refused_before_the_credited_login_is_looked_up() {
    let server = MockServer::start().await;
    allow_comments(&server).await;
    permission(&server, "alice", "read").await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{REPO}/collaborators/carol/permission"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "admin"})))
        .expect(0)
        .mount(&server)
        .await;
    no_approval(&server).await;

    let status = approve(&server, &test_cfg(true), "alice", Some("carol")).await;
    assert_eq!(status, "denied");
}

/// The gap this closes: the credited login was never checked, so an approval
/// could be credited to someone who cannot approve at all. GitHub would have
/// rejected their own review request; ours went through as theirs.
#[tokio::test]
async fn credited_login_needs_write_access_too() {
    let server = MockServer::start().await;
    permission(&server, "alice", "write").await;
    permission(&server, "carol", "read").await;
    no_approval(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("they need write access"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;

    let status = approve(&server, &test_cfg(true), "alice", Some("carol")).await;
    assert_eq!(status, "credited-denied");
}

/// Shape is checked before the login becomes a path segment, so a malformed
/// credit costs no request either.
#[tokio::test]
async fn credited_login_must_look_like_a_login() {
    let server = MockServer::start().await;
    allow_comments(&server).await;
    permission(&server, "alice", "admin").await;
    // `carol-` never becomes a path segment: the shape check comes first.
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{REPO}/collaborators/carol-/permission"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "admin"})))
        .expect(0)
        .mount(&server)
        .await;
    no_approval(&server).await;

    let status = approve(&server, &test_cfg(true), "alice", Some("carol-")).await;
    assert_eq!(status, "invalid-credited");
}

/// And the feature still works when it is switched on and everyone qualifies —
/// the approval is posted once, crediting the named user and recording who
/// relayed it.
#[tokio::test]
async fn relayed_approval_is_posted_when_everyone_qualifies() {
    let server = MockServer::start().await;
    allow_comments(&server).await;
    permission(&server, "alice", "write").await;
    permission(&server, "carol", "maintain").await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .and(body_string_contains("APPROVE"))
        .and(body_string_contains("on behalf of @carol"))
        .and(body_string_contains("r+ by alice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 5})))
        .expect(1)
        .mount(&server)
        .await;

    let status = approve(&server, &test_cfg(true), "alice", Some("carol")).await;
    assert_eq!(status, "ok");
}

// ---------------------------------------------------------------------------
// The help table has to describe the deployment it is running in
// ---------------------------------------------------------------------------

/// A row that says a command works when the deployment refuses it is how users
/// end up filing the refusal as a bug.
#[test]
fn help_text_tells_the_truth_about_the_switch() {
    for lang in [Lang::En, Lang::Zh] {
        let off = xero_bot::handlers::help_text("xero-review", lang, false);
        assert!(
            off.contains("R_PLUS_ALLOW_ON_BEHALF"),
            "{lang:?} help must name the setting when the feature is off:\n{off}"
        );
        let on = xero_bot::handlers::help_text("xero-review", lang, true);
        assert!(
            !on.contains("R_PLUS_ALLOW_ON_BEHALF"),
            "{lang:?} help must not tell users to enable what is already on:\n{on}"
        );
        // The row itself exists either way.
        assert!(
            on.contains("r+ as @user") && off.contains("r+ as @user"),
            "{lang:?}"
        );
    }
}
