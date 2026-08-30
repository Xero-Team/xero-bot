//! Integration tests with a mocked GitHub API (wiremock).
//!
//! octocrab's base URL is redirected to the mock server, so full handler
//! flows run end-to-end without touching real GitHub.

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::commands::parse_commands;
use xero_bot::config::Config;
use xero_bot::dispatch::{route_event, Routing, Work};
use xero_bot::github::Client;

/// A Config pointed at nothing (API base overridden per-test via octocrab env).
///
/// Built by overriding `from_env()` rather than as a struct literal, so adding a
/// field to `Config` doesn't break this file.
fn test_cfg() -> Config {
    let mut c = Config::from_env();
    c.app_id = "12345".into();
    c.private_key_pem = Some(TEST_KEY.into());
    c.webhook_secret = "whsec".into();
    c.bot_name = "xero-review".into();
    c.ai_base_url = String::new();
    c.ai_api_key = String::new();
    c.ai_model = String::new();
    c.api_format = "chat".into();
    c.max_diff_chars = 60_000;
    c.review_engine = "builtin".into();
    c.agent_max_turns = 4;
    c.agent_timeout_secs = 10;
    c.pi_path = "definitely-not-a-binary".into();
    c.pi_args = String::new();
    c.codex_path = "definitely-not-a-binary".into();
    c.codex_args = String::new();
    c.data_dir = "/tmp/xero-test".into();
    c.cron_secret = String::new();
    c.rebase_check_delay_secs = 0;
    c.rebase_sweep_enabled = false;
    c.rebase_sweep_interval_secs = 3600;
    c.label_needs_rebase = "needs-rebase".into();
    c.label_waiting_review = "waiting-on-review".into();
    c.label_waiting_author = "waiting-on-author".into();
    c.label_blocked = "blocked".into();
    c.codeql_label = "codeql".into();
    c.port = 8080;
    c
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
/// Shaped like a real `issue_comment` delivery: the `installation` property is
/// GitHub's *simple installation* object, so it carries no `app_slug`. Earlier
/// fixtures fabricated one, which hid the bugs that came from it always being
/// empty in production.
fn make_payload(action: &str, body: &str, commenter: &str, pr_author: &str) -> Value {
    json!({
        "action": action,
        "installation": {"id": 42, "node_id": "MDIzOkludGVncmF0aW9u"},
        "repository": {"full_name": "octocat/hello"},
        "issue": {
            "number": 7,
            "pull_request": {"url": "https://api.github.com/repos/octocat/hello/pulls/7"},
            "user": {"login": pr_author}
        },
        "comment": {"body": body, "user": {"login": commenter, "type": "User"}}
    })
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
        installation_id: 42,
        is_pr: true,
        lang: xero_bot::lang::Lang::Zh,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Ready],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}

#[tokio::test]
async fn test_r_plus_permission_denied() {
    let server = MockServer::start().await;

    // commenter has only "read" permission
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "read"})))
        .expect(1)
        .mount(&server)
        .await;
    // a rejection comment should be posted
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
        installation_id: 42,
        is_pr: true,
        lang: xero_bot::lang::Lang::Zh,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Approve { on_behalf_of: None }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["denied"]);
}

#[tokio::test]
async fn test_r_plus_approves_with_write() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "write"})))
        .expect(1)
        .mount(&server)
        .await;
    // APPROVE review + confirmation comment
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 99})))
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
        installation_id: 42,
        is_pr: true,
        lang: xero_bot::lang::Lang::Zh,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Approve { on_behalf_of: None }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}

#[tokio::test]
async fn test_rebase_check_flags_conflict() {
    let server = MockServer::start().await;

    // PR is conflicted
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "state": "open",
            "mergeable": false,
            "base": {"ref": "main"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/issues/7/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
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
    let status = xero_bot::rebase::check_pr(&gh, &cfg, "octocat/hello", 7).await;
    assert_eq!(status, "flagged");
}

#[tokio::test]
async fn test_codeql_report_posts_findings() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/code-scanning/alerts"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([
                {
                    "rule": {"id": "rs/sql-injection", "severity": "error", "description": "Uncontrolled data used in path expression"},
                    "most_recent_instance": {"location": {"path": "src/main.rs", "start_line": 10}},
                    "html_url": "https://github.com/octocat/hello/security/code-scanning/1"
                }
            ])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7/files"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"filename": "src/main.rs", "status": "modified"}])),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(2) // processing + report
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
    let status = xero_bot::codeql::run_codeql_report(
        &gh,
        &cfg,
        "octocat/hello",
        7,
        xero_bot::lang::Lang::Zh,
    )
    .await;
    assert_eq!(status, "ok");
}

/// Diagnostics reach the PR as exactly one consolidated comment, even with no
/// commands to run. Before this, a mistyped command produced no reply at all
/// and the author had no way to tell it hadn't worked.
#[tokio::test]
async fn test_diagnostics_are_posted_once() {
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
        installation_id: 42,
        is_pr: true,
        lang: xero_bot::lang::Lang::Zh,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![],
        vec![
            "`reviwe` 不是命令,是否想用 `review`?".into(),
            "`label` 需要至少一个 `+标签` 或 `-标签`。".into(),
        ],
    )
    .await;
    assert_eq!(results, vec!["diagnostics:ok"]);
    // wiremock verifies .expect(1) on drop: two complaints, one comment
}

/// Nothing to say means nothing posted — silence is the default for prose.
#[tokio::test]
async fn test_no_diagnostics_means_no_comment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(0)
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
        installation_id: 42,
        is_pr: true,
        lang: xero_bot::lang::Lang::Zh,
    };
    let results = xero_bot::handlers::handle_comment(&gh, &cfg, &ctx, vec![], vec![]).await;
    assert!(results.is_empty(), "{results:?}");
}

/// Drive the whole language path against a mock: fetch the PR's commits, let
/// them pick the language, run a command, and hand back the body of the comment
/// that actually reached GitHub.
///
/// `Claim` is the command under test because its reply is one short sentence
/// that differs in both languages — `ping` answers `pong 🏓` either way, so it
/// would pass no matter which language won.
async fn claim_reply_body(
    commit_subjects: &[&str],
    comment_lang: Option<xero_bot::lang::Lang>,
) -> String {
    let server = MockServer::start().await;

    let commits: Vec<Value> = commit_subjects
        .iter()
        .map(|s| json!({"commit": {"message": s}}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7/commits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(commits))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/assignees"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"assignees": [{"login": "alice"}]})),
        )
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
    let lang = xero_bot::lang::for_pr(&gh, "octocat/hello", 7, comment_lang).await;
    let ctx = xero_bot::handlers::CommentContext {
        repo: "octocat/hello".into(),
        pr_number: 7,
        commenter: "alice".into(),
        pr_author: "bob".into(),
        installation_id: 42,
        is_pr: true,
        lang,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Claim],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);

    let requests = server.received_requests().await.unwrap();
    let posted = requests
        .iter()
        .find(|r| r.url.path().ends_with("/comments"))
        .expect("a reply should have been posted");
    serde_json::from_slice::<Value>(&posted.body).unwrap()["body"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A PR whose commits are Chinese gets a Chinese reply.
#[tokio::test]
async fn test_chinese_commits_get_a_chinese_reply() {
    let body = claim_reply_body(&["修复解析器崩溃", "补充测试", "fix: typo"], None).await;
    assert_eq!(body, "@alice 已认领。", "{body}");
}

/// The same PR with English commits gets an English reply — the same code path,
/// so this is what proves the language is read rather than hardcoded.
#[tokio::test]
async fn test_english_commits_get_an_english_reply() {
    let body = claim_reply_body(&["add the parser", "fix the lexer", "修复崩溃"], None).await;
    assert_eq!(body, "@alice claimed this.", "{body}");
}

/// Commits that say nothing fall back to the triggering comment, and if that
/// says nothing either, to English.
#[tokio::test]
async fn test_no_signal_falls_back_to_the_comment_then_english() {
    let noise = ["1.2.3", "v2 -> v3"];
    let body = claim_reply_body(&noise, Some(xero_bot::lang::Lang::Zh)).await;
    assert_eq!(body, "@alice 已认领。", "{body}");

    let body = claim_reply_body(&noise, None).await;
    assert_eq!(body, "@alice claimed this.", "{body}");
}

/// The commits endpoint failing must not fail the command — the reply still
/// goes out, in the fallback language.
#[tokio::test]
async fn test_commits_error_still_replies_in_english() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7/commits"))
        .respond_with(ResponseTemplate::new(500))
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
    assert_eq!(
        xero_bot::lang::for_pr(&gh, "octocat/hello", 7, None).await,
        xero_bot::lang::Lang::En
    );
    // and the comment's own language is still honoured when the API is down
    assert_eq!(
        xero_bot::lang::for_pr(&gh, "octocat/hello", 7, Some(xero_bot::lang::Lang::Zh)).await,
        xero_bot::lang::Lang::Zh
    );
}

/// `claim` in an issue does exactly what it does in a PR — same endpoints.
#[tokio::test]
async fn test_claim_works_on_an_issue() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/assignees"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"assignees": [{"login": "alice"}]})),
        )
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
        installation_id: 42,
        is_pr: false,
        lang: xero_bot::lang::Lang::En,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::Claim],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);
}

/// A PR-only command in an issue says so, in one comment, and touches nothing
/// else. Before this it was dropped at the dispatch layer with no reply at all.
#[tokio::test]
async fn test_pr_only_commands_on_an_issue_explain_themselves() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(4)
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
        installation_id: 42,
        is_pr: false,
        lang: xero_bot::lang::Lang::En,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![
            xero_bot::commands::Command::Review,
            xero_bot::commands::Command::Codeql,
            xero_bot::commands::Command::Approve { on_behalf_of: None },
            xero_bot::commands::Command::Reject,
        ],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["not-a-pr"; 4]);

    // Each names the command the user actually typed, and no `/pulls/` or
    // permission endpoint was touched on the way — the gate is before the work.
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<String> = requests
        .iter()
        .map(|r| {
            assert!(
                r.url.path().starts_with("/repos/octocat/hello/issues/"),
                "unexpected request to {}",
                r.url.path()
            );
            serde_json::from_slice::<Value>(&r.body).unwrap()["body"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    for verb in ["`review`", "`codeql`", "`r+`", "`r-`"] {
        assert!(
            bodies.iter().any(|b| b.contains(verb)),
            "no reply mentioned {verb}: {bodies:?}"
        );
    }
}

/// `r? @user` degrades honestly: an issue has no reviewers, so the reply says
/// assignment rather than claiming a review was requested.
#[tokio::test]
async fn test_review_request_on_an_issue_is_called_an_assignment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello/issues/7/assignees"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"assignees": [{"login": "carol"}]})),
        )
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
        installation_id: 42,
        is_pr: false,
        lang: xero_bot::lang::Lang::En,
    };
    let results = xero_bot::handlers::handle_comment(
        &gh,
        &cfg,
        &ctx,
        vec![xero_bot::commands::Command::RequestReview {
            user: "carol".into(),
        }],
        vec![],
    )
    .await;
    assert_eq!(results, vec!["ok"]);

    let requests = server.received_requests().await.unwrap();
    let posted = requests
        .iter()
        .find(|r| r.url.path().ends_with("/comments"))
        .expect("a reply should have been posted");
    let body = serde_json::from_slice::<Value>(&posted.body).unwrap()["body"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(body.contains("@carol"), "{body}");
    assert!(
        !body.contains("as reviewer") && body.contains("assignment"),
        "the reply must not claim a reviewer was set: {body}"
    );
}

#[test]
fn test_parse_and_route_end_to_end() {
    let cfg = test_cfg();
    // every supported command parses and routes to Act
    for (body, _expected) in [
        ("@xero-review ping", "ping"),
        ("@xero-review help", "help"),
        ("r? @octocat", "review-request"),
        ("?r", "ready"),
        ("?r cc @alice", "ready+cc"),
        ("?r @alice", "ready+review-request"),
        ("@xero-review label +bug", "label"),
        ("@xero-review label +bug -wip; claim; ready", "chained"),
        ("@xero-review claim", "claim"),
        ("@xero-review r+", "approve"),
        ("@xero-review r-", "reject"),
        ("@xero-review codeql", "codeql"),
    ] {
        let payload = make_payload("created", body, "alice", "bob");
        match route_event(&cfg, "issue_comment", &payload) {
            Routing::Act(Work::Comment { commands, .. }) => {
                let parsed = parse_commands("xero-review", body);
                assert_eq!(commands.len(), parsed.commands.len(), "for body: {body}");
                assert!(!commands.is_empty(), "for body: {body}");
                assert!(
                    parsed.diagnostics.is_empty(),
                    "valid command must not warn: {body} -> {:?}",
                    parsed.diagnostics
                );
            }
            other => panic!("expected Act(Comment) for {body}, got {other:?}"),
        }
    }
}
