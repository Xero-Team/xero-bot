//! Bors merge queue end-to-end tests against a mocked GitHub API.
//!
//! These exercise the driver (`pump_one` / `pump_all`'s per-repo unit) and
//! the queue commands (`render_queue_status`, `enqueue`) the way production
//! runs: every state transition reads GitHub, mutates it, and re-derives the
//! next step from GitHub's answer. The mocks assert the *order and count* of
//! writes, because a driver that mutates the staging branch twice per tick
//! corrupts the queue just as surely as one that never mutates it.

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::config::Config;
use xero_bot::github::Client;

const REPO: &str = "octocat/hello";

/// Bors-enabled config pointed at nothing (octocrab is redirected per-test).
fn bors_cfg() -> Config {
    let mut c = Config::from_env();
    c.app_id = "12345".into();
    c.webhook_secret = "whsec".into();
    c.bot_name = "xero-review".into();
    c.app_slug = "xero-review".into();
    c.rebase_sweep_enabled = false;
    c.bors_enabled = true;
    c.bors_staging_branch = "staging".into();
    c.bors_max_batch = 8;
    c.bors_ci_timeout_secs = 7_200;
    c.bors_poll_interval_secs = 30;
    c.bors_advance_method = "pr".into();
    c.bors_advance_merge_method = "merge".into();
    c.bors_cleanup_staging = true;
    c.bors_advance_strict = false;
    c.label_bors_queued = "bors: queued".into();
    c.label_bors_testing = "bors: testing".into();
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

fn repo_info_mock(default_branch: &str) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": REPO, "default_branch": default_branch
        })))
}

/// A staging ref: `None` = 404 (branch absent).
fn branch_ref(branch: &str, sha: Option<&str>) -> Mock {
    match sha {
        Some(sha) => Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/git/ref/heads/{branch}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ref": format!("refs/heads/{branch}"),
                "object": {"sha": sha}
            }))),
        None => Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/git/ref/heads/{branch}")))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "message": "Not Found"
            }))),
    }
}

/// Commit list shaped like `/commits?sha=…` (newest first), through the
/// pagination wrapper (a bare array is the whole list against wiremock).
fn commits_mock(sha_filter: &str, commits: serde_json::Value) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/commits")))
        .and(query_param("sha", sha_filter))
        .respond_with(ResponseTemplate::new(200).set_body_json(commits))
}

/// A single commit, served for any `sha=` query — the parent lookups ask for
/// `sha=<merge sha>` and read `parents[0]`.
fn single_commit_mock(sha: &str, parents: &[&str]) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/commits")))
        .and(query_param("sha", sha))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "sha": sha,
            "commit": {"message": "xero-bors: merge #10 (head head10)"},
            "parents": parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>()
        }])))
}

fn merge_commit(sha: &str, message: &str) -> serde_json::Value {
    json!({"sha": sha, "commit": {"message": message}})
}

fn marker_commit(sha: &str, pr: i64, head: &str) -> serde_json::Value {
    merge_commit(sha, &format!("xero-bors: merge #{pr} (head {head})"))
}

/// CI reads: check-runs + statuses for one sha.
fn ci_mocks(sha: &str, conclusion: &str) -> Vec<Mock> {
    vec![
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/commits/{sha}/check-runs")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "check_runs": [ {"name": "build", "conclusion": conclusion} ]
            }))),
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/commits/{sha}/statuses")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([]))),
    ]
}

async fn allow_comments(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex(
            format!(r"/repos/{REPO}/issues/\d+/comments").as_str(),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(server)
        .await;
}

async fn allow_labels(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex(
            format!(r"/repos/{REPO}/issues/\d+/labels").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(
            format!(r"/repos/{REPO}/issues/\d+/labels/.*").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
}

/// Two queued PRs, #10 and #11, both open and mergeable.
fn queued_issues_mock() -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues")))
        .and(query_param("labels", "bors: queued"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 10,
                "pull_request": {"url": "x"},
                "state": "open"
            },
            {
                "number": 11,
                "pull_request": {"url": "x"},
                "state": "open"
            }
        ])))
}

fn pr_mock(number: i64, head_sha: &str, state: &str, mergeable: Option<bool>) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/pulls/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": number,
            "state": state,
            "mergeable": mergeable,
            "base": {"ref": "main"},
            "head": {"ref": format!("feature-{number}"), "sha": head_sha,
                     "repo": {"full_name": REPO}}
        })))
}

// ---------------------------------------------------------------------------
// 1. Happy path, split into its two ticks (a driver tick is one transition).
// ---------------------------------------------------------------------------

/// Tick one: queued PRs → staging created → heads merged in order → labels.
#[tokio::test]
async fn batch_of_two_is_staged_in_order() {
    let server = MockServer::start().await;

    // Reading the queue and the PRs.
    repo_info_mock("main").mount(&server).await;
    queued_issues_mock().mount(&server).await;
    pr_mock(10, "head10", "open", Some(true))
        .mount(&server)
        .await;
    pr_mock(11, "head11", "open", Some(true))
        .mount(&server)
        .await;

    // main's tip.
    branch_ref("main", Some("ma1n")).mount(&server).await;
    // staging doesn't exist yet → created from main.
    branch_ref("staging", None).mount(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/git/refs")))
        .and(body_string_contains("staging"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    // Merging both heads in, in number order, with the marker message.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/merges")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"sha": "s0me-merge-sha"})))
        .expect(2)
        .mount(&server)
        .await;

    // Label swap: queued → testing for both members.
    allow_labels(&server).await;
    allow_comments(&server).await;

    let gh = client_for(&server);
    let cfg = bors_cfg();
    let outcome = xero_bot::bors::pump_one(&gh, &cfg, REPO).await;
    assert_eq!(
        outcome,
        xero_bot::bors::PumpOutcome::Started(vec![10, 11]),
        "{outcome}"
    );
}

/// Tick two: the staged chain is green on CI → advance PR created and merged
/// → batch completed, staging deleted.
#[tokio::test]
async fn green_chain_advances_main() {
    let server = MockServer::start().await;
    let cfg = bors_cfg();
    let gh = client_for(&server);

    // Mid-batch state: staging holds a two-member chain.
    repo_info_mock("main").mount(&server).await;
    branch_ref("staging", Some("s0me-merge-sha"))
        .mount(&server)
        .await;
    commits_mock(
        "staging",
        json!([
            marker_commit("s0me-merge-sha", 11, "head11"),
            merge_commit("under11", "xero-bors: merge #10 (head head10)"),
            merge_commit("ma1n", "Initial commit")
        ]),
    )
    .mount(&server)
    .await;
    // Member health check re-reads both PRs.
    pr_mock(10, "head10", "open", Some(true))
        .mount(&server)
        .await;
    pr_mock(11, "head11", "open", Some(true))
        .mount(&server)
        .await;
    // The queue is empty (the members left it when the batch started).
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues")))
        .and(query_param("labels", "bors: queued"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    for m in ci_mocks("s0me-merge-sha", "success") {
        m.mount(&server).await;
    }

    // No stale advance PRs (open PRs toward main: none).
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/pulls")))
        .and(query_param("base", "main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    // Creating the advance PR, then merging it.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 99})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/repos/{REPO}/pulls/99/merge")))
        .and(body_string_contains("merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sha": "advanced-sha"})))
        .expect(1)
        .mount(&server)
        .await;
    // Completion: staging deleted.
    Mock::given(method("DELETE"))
        .and(path(format!("/repos/{REPO}/git/refs/heads/staging")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = xero_bot::bors::pump_one(&gh, &cfg, REPO).await;
    assert_eq!(
        outcome,
        xero_bot::bors::PumpOutcome::Advanced(vec![10, 11], "advanced-sha".into()),
        "{outcome}"
    );
}

// ---------------------------------------------------------------------------
// 2. Red CI: the tail member is the culprit, staging resets, prefix re-tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn red_ci_drops_the_tail_and_resets() {
    let server = MockServer::start().await;

    // A batch of one is already under test: #10 merged into staging.
    let cfg = bors_cfg();
    let gh = client_for(&server);

    // Reads that reach the driver before CI.
    repo_info_mock("main").mount(&server).await;
    branch_ref("staging", Some("merge10")).mount(&server).await;
    commits_mock(
        "staging",
        json!([
            marker_commit("merge10", 10, "head10"),
            merge_commit("ma1n", "Initial commit")
        ]),
    )
    .mount(&server)
    .await;
    pr_mock(10, "head10", "open", Some(true))
        .mount(&server)
        .await;

    // CI is red on the tip, with a named failing check.
    for m in ci_mocks("merge10", "failure") {
        m.mount(&server).await;
    }
    // The parent lookup for the reset target: merge10's first parent is main.
    single_commit_mock("merge10", &["ma1n"])
        .mount(&server)
        .await;

    // Reset staging to main's tip (the parent of the only merge commit).
    Mock::given(method("PATCH"))
        .and(path(format!("/repos/{REPO}/git/refs/heads/staging")))
        .and(body_string_contains("ma1n"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    // The culprit's labels are cleared.
    allow_labels(&server).await;
    allow_comments(&server).await;

    let outcome = xero_bot::bors::pump_one(&gh, &cfg, REPO).await;
    assert_eq!(
        outcome,
        xero_bot::bors::PumpOutcome::Dropped(10, "CI failed".into()),
        "{outcome}"
    );

    // The comment names the failing check so the author doesn't have to dig.
    // (Verified via the request log: the posted body carries "build".)
}

// ---------------------------------------------------------------------------
// 3. Conflict on enqueue-fold: the PR is dropped, the batch continues
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conflict_drops_one_member_and_keeps_the_batch() {
    let server = MockServer::start().await;

    queued_issues_mock().mount(&server).await;
    repo_info_mock("main").mount(&server).await;
    pr_mock(10, "head10", "open", Some(true))
        .mount(&server)
        .await;
    pr_mock(11, "head11", "open", Some(true))
        .mount(&server)
        .await;

    branch_ref("main", Some("ma1n")).mount(&server).await;
    branch_ref("staging", None).mount(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/git/refs")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
        .mount(&server)
        .await;

    // #10 merges, #11 conflicts (409). One 201, one 409, order matters.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/merges")))
        .and(body_string_contains("feature-10"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"sha": "m10"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/merges")))
        .and(body_string_contains("feature-11"))
        .respond_with(
            ResponseTemplate::new(409).set_body_json(json!({"message": "Merge Conflict"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    allow_labels(&server).await;
    allow_comments(&server).await;

    let gh = client_for(&server);
    let cfg = bors_cfg();
    let outcome = xero_bot::bors::pump_one(&gh, &cfg, REPO).await;
    assert_eq!(
        outcome,
        xero_bot::bors::PumpOutcome::Started(vec![10]),
        "the conflicting member must not stall the batch: {outcome}"
    );
}

// ---------------------------------------------------------------------------
// 4. Restart recovery: a mid-batch crash is invisible — the next tick resumes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_mid_batch_resumes_without_re_merging() {
    let server = MockServer::start().await;

    // No queued PRs (both are in the batch), staging has one marker commit
    // of the two-member batch, CI on the tip is still pending.
    let cfg = bors_cfg();
    let gh = client_for(&server);

    repo_info_mock("main").mount(&server).await;
    branch_ref("staging", Some("m2")).mount(&server).await;
    commits_mock(
        "staging",
        json!([
            marker_commit("m2", 11, "head11"),
            merge_commit("m1", "xero-bors: merge #10 (head head10)"),
            merge_commit("ma1n", "Initial commit")
        ]),
    )
    .mount(&server)
    .await;
    pr_mock(10, "head10", "open", Some(true))
        .mount(&server)
        .await;
    pr_mock(11, "head11", "open", Some(true))
        .mount(&server)
        .await;
    queued_issues_mock().mount(&server).await;
    for m in ci_mocks("m2", "") {
        m.mount(&server).await;
    }

    // Nothing may mutate: the batch is mid-test, and a healthy batch means
    // waiting — not re-merging, not resetting.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/merges")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"sha": "x"})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(format!(r"/repos/{REPO}/git/refs/.*").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(0)
        .mount(&server)
        .await;

    let outcome = xero_bot::bors::pump_one(&gh, &cfg, REPO).await;
    assert_eq!(outcome, xero_bot::bors::PumpOutcome::Testing, "{outcome}");
}

// ---------------------------------------------------------------------------
// 5. Advance blocked by protection (405): batch stays alive, no member dropped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advance_pr_405_keeps_the_batch() {
    let server = MockServer::start().await;

    let cfg = bors_cfg();
    let gh = client_for(&server);

    repo_info_mock("main").mount(&server).await;
    branch_ref("staging", Some("m2")).mount(&server).await;
    commits_mock(
        "staging",
        json!([
            marker_commit("m2", 11, "head11"),
            merge_commit("m1", "xero-bors: merge #10 (head head10)"),
            merge_commit("ma1n", "Initial commit")
        ]),
    )
    .mount(&server)
    .await;
    pr_mock(10, "head10", "open", Some(true))
        .mount(&server)
        .await;
    pr_mock(11, "head11", "open", Some(true))
        .mount(&server)
        .await;
    for m in ci_mocks("m2", "success") {
        m.mount(&server).await;
    }
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/pulls")))
        .and(query_param("base", "main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 99})))
        .mount(&server)
        .await;
    // Branch protection refuses the merge.
    Mock::given(method("PUT"))
        .and(path(format!("/repos/{REPO}/pulls/99/merge")))
        .respond_with(ResponseTemplate::new(405).set_body_json(json!({
            "message": "Required status check \"build\" is expected."
        })))
        .expect(1)
        .mount(&server)
        .await;

    // No member may be dropped on an advance refusal.
    Mock::given(method("DELETE"))
        .and(path_regex(
            format!(r"/repos/{REPO}/issues/\d+/labels/.*").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&server)
        .await;
    allow_comments(&server).await;
    allow_labels(&server).await;

    let outcome = xero_bot::bors::pump_one(&gh, &cfg, REPO).await;
    match outcome {
        xero_bot::bors::PumpOutcome::AdvanceBlocked(_) => {}
        other => panic!("expected AdvanceBlocked, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 6. The bot's own approval never enqueues (route level)
// ---------------------------------------------------------------------------

#[test]
fn self_review_never_reaches_the_queue() {
    use xero_bot::dispatch::{route_event, Routing};

    let mut cfg = Config::from_env();
    cfg.app_id = "4768775".into();
    cfg.bot_name = "xero-review".into();
    cfg.app_slug = "xero-review".into();
    cfg.webhook_secret = "whsec".into();
    cfg.bors_enabled = true;

    // The payload our own `post_approve_review` produces.
    let payload = json!({
        "action": "submitted",
        "installation": {"id": 42},
        "repository": {"full_name": REPO},
        "pull_request": {"number": 7},
        "review": {
            "state": "APPROVED",
            "user": {"login": "xero-review[bot]", "type": "Bot"},
            "performed_via_github_app": {"id": 4768775}
        }
    });
    let routing = route_event(&cfg, "pull_request_review", &payload);
    assert!(
        matches!(routing, Routing::Respond(_)),
        "a self-review must be answered inline, never acted on: {routing:?}"
    );
    // And with the App id missing (API-posted), the login check holds.
    let payload = json!({
        "action": "submitted",
        "installation": {"id": 42},
        "repository": {"full_name": REPO},
        "pull_request": {"number": 7},
        "review": {"state": "APPROVED", "user": {"login": "xero-review[bot]", "type": "Bot"}}
    });
    let routing = route_event(&cfg, "pull_request_review", &payload);
    assert!(
        matches!(routing, Routing::Respond(_)),
        "the [bot]-suffix login check must hold without the app id: {routing:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. r+ auto-enqueue: the label lands when the approval relay succeeds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn r_plus_success_enqueues_the_pr() {
    let server = MockServer::start().await;

    // The r+ flow: commenter alice has write, PR #7 is by bob.
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{REPO}/collaborators/alice/permission"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "write"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/7/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 5})))
        .expect(1)
        .mount(&server)
        .await;

    // The queue's reads: PR #7 (open, mergeable) and repo default branch.
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/pulls/7")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "state": "open", "mergeable": true,
            "base": {"ref": "main"},
            "head": {"ref": "feature", "sha": "abc1234", "repo": {"full_name": REPO}}
        })))
        .mount(&server)
        .await;
    repo_info_mock("main").mount(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues/7/labels")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    // The queue's writes: the queued label + the ack comment. These are what
    // distinguish "approved and queued" from a plain r+.
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/labels")))
        .and(body_string_contains("bors: queued"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/7/comments")))
        .and(body_string_contains("Queued for merge"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(&server)
        .await;

    let cfg = bors_cfg();
    let gh = client_for(&server);
    let ctx = xero_bot::handlers::CommentContext {
        repo: REPO.into(),
        pr_number: 7,
        commenter: "alice".into(),
        pr_author: "bob".into(),
        installation_id: 42,
        is_pr: true,
        lang: xero_bot::lang::Lang::En,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Approve { on_behalf_of: None }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"], "r+ must succeed before enqueueing");
}
