//! Issue #15 acceptance uses the production inbox consumer and HTTP boundaries.
use super::*;

static REVIEW_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[path = "event_review_tests.rs"]
mod review_fixes;

#[path = "path_review_tests.rs"]
mod path_review_fixes;

fn opened(pr: bool) -> EventContext {
    let thread = json!({"id":88,"number":3,"created_at":"2026-10-02T01:00:00Z",
        "user":{"id":5,"login":"alice","type":"User"},"draft":true,
        "body":"<!-- xero-merge-queue -->", "head":{"sha":"old-head"},"base":{"sha":"base"}});
    let mut p = json!({"action":"opened","repository":{"id":9,"full_name":"example/project"},"installation":{"id":7}});
    p[if pr { "pull_request" } else { "issue" }] = thread;
    EventContext::capture(
        if pr { "pull_request" } else { "issues" },
        &p,
        Some("opened-1"),
    )
    .unwrap()
    .unwrap()
}

fn label_rules(pr: bool) -> String {
    let event = if pr {
        "pull_request.opened"
    } else {
        "issues.opened"
    };
    format!("[[event_triggers]]\nid='triage-a'\nevent='{event}'\ncommand='label'\nadd=['Triage','bug','triage']\n[[event_triggers]]\nid='triage-b'\nevent='{event}'\ncommand='relabel'\nadd=['bug','triage']")
}

async fn labels(server: &MockServer) {
    response(
        server,
        "GET",
        "/repos/example/project/labels",
        200,
        json!([{"name":"triage"},{"name":"bug"}]),
    )
    .await;
    response(
        server,
        "POST",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
}

async fn pr_meta(server: &MockServer) {
    response(server,"GET","/repos/example/project/pulls/3",200,
        json!({"number":3,"head":{"sha":"new-head"},"base":{"sha":"base","ref":"main"},"changed_files":1,"user":{"login":"alice"},"title":"Draft change"})).await;
}

async fn path_pr_meta(server: &MockServer, changed_files: u64) {
    response(
        server,
        "GET",
        "/repos/example/project/pulls/3",
        200,
        json!({"number":3,"head":{"sha":"new-head"},"base":{"sha":"base","ref":"main"},"changed_files":changed_files,"user":{"login":"alice"},"title":"Path change"}),
    )
    .await;
}

async fn writes(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method != "GET")
        .collect()
}

async fn review_fixture(server: &MockServer, status: u16) -> Config {
    policy(server,"[[event_triggers]]\nid='review'\nevent='pull_request.opened'\ncommand='review'\n[[event_triggers]]\nid='same-review'\nevent='pull_request.opened'\ncommand='review'").await;
    pr_meta(server).await;
    Mock::given(method("GET"))
        .and(path("/repos/example/project/compare/base...new-head"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -0,0 +1 @@\n+fn main() {}\n",
        ))
        .mount(server)
        .await;
    response(server,"POST","/chat/completions",200,json!({"choices":[{"message":{"content":"{\"verdict\":\"approve\",\"summary\":\"Looks good\",\"findings\":[]}"}}]})).await;
    response(
        server,
        "POST",
        "/repos/example/project/pulls/3/reviews",
        status,
        json!({"id":81,"message":"simulated lost response"}),
    )
    .await;
    let mut cfg = cfg();
    cfg.ai_base_url = server.uri();
    cfg.ai_api_key = "test".into();
    cfg.ai_model = "test-model".into();
    cfg.api_format = "chat".into();
    cfg.review_engine = "builtin".into();
    cfg.review_verify = false;
    cfg.merge_queue_enabled = true;
    cfg
}

#[tokio::test]
async fn defaults_and_non_open_events_never_execute_or_backfill() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    for pr in [false, true] {
        let mut ctx = opened(pr);
        ctx.thread_id += i64::from(pr);
        runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
    }
    server.reset().await;
    policy(&server, &label_rules(false)).await;
    labels(&server).await;
    runtime
        .process(
            &gh,
            &cfg(),
            &RepositoryConfigCache::default(),
            &opened(false),
        )
        .await
        .unwrap();
    for event in ["issues", "pull_request"] {
        for action in ["edited", "reopened", "synchronize", "closed"] {
            let mut ctx = opened(event == "pull_request");
            ctx.event = event.into();
            ctx.action = action.into();
            runtime
                .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
                .await
                .unwrap();
        }
    }
    assert!(writes(&server).await.is_empty());
    assert!(runtime.store.list(None).unwrap().is_empty());
}

#[tokio::test]
async fn issue_and_draft_pr_merge_equivalent_rules_across_concurrency_restart_and_config_changes() {
    for pr in [false, true] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        policy(&server, &label_rules(pr)).await;
        labels(&server).await;
        pr_meta(&server).await;
        let cache = RepositoryConfigCache::default();
        let cfg = cfg();
        let ctx = opened(pr);
        let mut duplicate = ctx.clone();
        duplicate.delivery = "opened-duplicate".into();
        let (a, b) = tokio::join!(
            runtime.process(&gh, &cfg, &cache, &ctx),
            runtime.process(&gh, &cfg, &cache, &duplicate)
        );
        assert!(a.is_ok() || b.is_ok());
        runtime
            .process(&gh, &cfg, &cache, &duplicate)
            .await
            .unwrap();
        assert_eq!(writes(&server).await.len(), 1);
        let op = runtime
            .store
            .get(&opened_key(9, 88, "triage-a"))
            .unwrap()
            .unwrap();
        assert_eq!(op.state, State::Succeeded);
        assert_eq!(op.spec.request["rules"], json!(["triage-a", "triage-b"]));
        assert_eq!(op.spec.config_sha.as_deref(), Some("config"));
        assert_eq!(
            op.spec.context.event,
            if pr { "pull_request" } else { "issues" }
        );
        assert_eq!(op.spec.context.user_id, Some(5));
        if pr {
            assert_eq!(op.spec.context.head_sha.as_deref(), Some("new-head"));
        }
        assert!(runtime
            .store
            .session_before(
                &SessionWake {
                    installation_id: 7,
                    repository_id: 9,
                    thread_number: 3,
                    user_id: 5,
                    comment_id: 99,
                    source_at: chrono::Utc::now().timestamp_millis()
                },
                30
            )
            .unwrap()
            .is_none());
        drop(runtime);
        let runtime = Runtime::open(&dir.0).unwrap();
        runtime
            .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
        assert_eq!(writes(&server).await.len(), 1);
        server.reset().await;
        policy(
            &server,
            &label_rules(pr)
                .replace("['Triage','bug','triage']", "['new-label']")
                .replace("triage-b", "new-rule"),
        )
        .await;
        runtime
            .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
        assert!(writes(&server).await.is_empty());
    }
}

#[tokio::test]
async fn auto_review_is_comment_only_and_pins_actual_head_without_session_or_queue() {
    let _serial = REVIEW_TEST.lock().await;
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    let cfg = review_fixture(&server, 201).await;
    let ctx = opened(true);
    runtime.store.enqueue(&ctx, 0).unwrap();
    runtime
        .pump_with(
            |ctx| {
                let runtime = &runtime;
                let gh = &gh;
                let cfg = &cfg;
                async move {
                    runtime
                        .process(gh, cfg, &RepositoryConfigCache::default(), &ctx)
                        .await
                }
            },
            std::time::Duration::from_secs(20),
            std::time::Duration::from_millis(5),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.store.inbox_status().unwrap()[0]["state"],
        "succeeded"
    );
    let requests = writes(&server).await;
    assert_eq!(
        requests.len(),
        2,
        "only AI request and COMMENT review: {requests:?}"
    );
    let published: Value = serde_json::from_slice(
        &requests
            .iter()
            .find(|r| r.url.path().ends_with("/reviews"))
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(published["event"], "COMMENT");
    assert_eq!(published["commit_id"], "new-head");
    let body = published["body"].as_str().unwrap();
    for text in [
        "review",
        "same-review",
        "config",
        "new-head",
        "pull_request.opened",
    ] {
        assert!(body.contains(text), "{body}");
    }
    let op = runtime
        .store
        .get(&opened_key(9, 88, "review"))
        .unwrap()
        .unwrap();
    assert_eq!(op.spec.context.head_sha.as_deref(), Some("new-head"));
    assert_eq!(op.state, State::Succeeded);
    assert!(server.received_requests().await.unwrap().iter().all(|r| !r
        .url
        .path()
        .contains("/permission")
        && !r.url.path().ends_with("/labels")));
}

#[tokio::test]
async fn automatic_codeql_posts_one_traced_report() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(
        &server,
        "[[event_triggers]]\nid='scan'\nevent='pull_request.opened'\ncommand='codeql'",
    )
    .await;
    pr_meta(&server).await;
    response(
        &server,
        "GET",
        "/repos/example/project/code-scanning/alerts",
        200,
        json!([]),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3/files",
        200,
        json!([{"filename":"a.rs"}]),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":82}),
    )
    .await;
    runtime
        .process(
            &gh,
            &cfg(),
            &RepositoryConfigCache::default(),
            &opened(true),
        )
        .await
        .unwrap();
    let requests = writes(&server).await;
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(
        body.contains("CodeQL quality report")
            && body.contains("new-head")
            && body.contains("scan")
    );
}

#[tokio::test]
async fn self_app_is_verified_and_other_bots_or_copied_markers_are_not_suppressed() {
    for (app, user_type, user_id, expected) in [
        (Some(1), "Bot", 42, 0),
        (None, "Bot", 42, 0),
        (Some(99), "Bot", 99, 1),
        (None, "Bot", 99, 1),
        (None, "User", 42, 1),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        policy(&server, &label_rules(true)).await;
        labels(&server).await;
        pr_meta(&server).await;
        response(
            &server,
            "GET",
            "/users/bot%5Bbot%5D",
            200,
            json!({"id":42,"type":"Bot"}),
        )
        .await;
        let mut ctx = opened(true);
        ctx.via_app_id = app;
        ctx.user_id = Some(user_id);
        ctx.user_type = Some(user_type.into());
        ctx.login = Some("bot[bot]".into());
        runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
        assert_eq!(writes(&server).await.len(), expected);
    }
}

#[tokio::test]
async fn reserved_or_nonexistent_labels_disabled_rules_and_config_failure_cannot_write() {
    for text in [
        label_rules(false)
            .replace("['Triage','bug','triage']", "['queued']")
            .replace("['bug','triage']", "['testing']"),
        label_rules(false)
            .replace("['Triage','bug','triage']", "['SCAN']")
            .replace("['bug','triage']", "['scan']"),
        label_rules(false)
            .replace("['Triage','bug','triage']", "['missing']")
            .replace("['bug','triage']", "['missing']"),
        format!(
            "[command_triggers]\nrelabel={{mode='disabled'}}\n{}",
            label_rules(false)
        ),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        policy(&server, &text).await;
        labels(&server).await;
        let mut cfg = cfg();
        cfg.codeql_label = "scan".into();
        cfg.label_merge_queue_queued = "queued".into();
        cfg.label_merge_queue_testing = "testing".into();
        runtime
            .process(&gh, &cfg, &RepositoryConfigCache::default(), &opened(false))
            .await
            .unwrap();
        assert!(writes(&server).await.is_empty());
    }
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    assert!(runtime
        .process(
            &gh,
            &cfg(),
            &RepositoryConfigCache::default(),
            &opened(false)
        )
        .await
        .is_err());
    assert!(runtime.store.list(None).unwrap().is_empty());
    policy(&server, &label_rules(false)).await;
    labels(&server).await;
    runtime
        .process(
            &gh,
            &cfg(),
            &RepositoryConfigCache::default(),
            &opened(false),
        )
        .await
        .unwrap();
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn lost_review_response_reconciles_after_restart_without_second_ai_or_report() {
    let _serial = REVIEW_TEST.lock().await;
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    let cfg = review_fixture(&server, 503).await;
    let ctx = opened(true);
    assert!(runtime
        .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
        .await
        .is_err());
    let key = opened_key(9, 88, "review");
    let child = runtime
        .store
        .children(&key)
        .unwrap()
        .into_iter()
        .find(|c| c.spec.kind == "review")
        .unwrap();
    assert_eq!(child.state, State::Unknown);
    let body = child.spec.request["body"]["body"].clone();
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    let before = writes(&server).await.len();
    response(&server,"GET","/repos/example/project/pulls/3/reviews",200,json!([{"id":81,"body":body,"user":{"login":"alice","type":"User"},"performed_via_github_app":{"id":1}}])).await;
    assert!(runtime
        .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
        .await
        .is_err());
    assert_eq!(writes(&server).await.len(), before);
    server.reset().await;
    review_fixture(&server, 201).await;
    response(&server,"GET","/repos/example/project/pulls/3/reviews",200,json!([{"id":81,"body":body,"user":{"login":"bot[bot]","type":"Bot"},"performed_via_github_app":{"id":1}}])).await;
    runtime
        .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
        .await
        .unwrap();
    assert_eq!(
        runtime.store.get(&key).unwrap().unwrap().state,
        State::Succeeded
    );
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn pending_plans_are_revoked_when_removed_changed_or_disabled() {
    for replacement in [
        "".to_string(),
        label_rules(false)
            .replace("['Triage','bug','triage']", "['different']")
            .replace("['bug','triage']", "['different']"),
        format!(
            "[command_triggers]\nlabel={{mode='disabled'}}\n{}",
            label_rules(false)
        ),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        let ctx = opened(false);
        let key = opened_key(9, 88, "triage-a");
        runtime.store.opened_plan(&json!(["opened-plan",9,88]).to_string(),&json!([{"rules":["triage-a","triage-b"],"action":{"AddLabels":["bug","triage"]},"config_sha":"old-config"}])).unwrap();
        let mut op = spec(&key, "opened");
        op.context = ctx.clone();
        unknown(&runtime.store, &op, false);
        policy(&server, &replacement).await;
        labels(&server).await;
        runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
        assert_eq!(
            runtime.store.get(&key).unwrap().unwrap().state,
            State::Superseded
        );
        assert!(writes(&server).await.is_empty());
    }
}

/// A report whose POST returned an uncertain status can be adopted from labels
/// without repeating the POST. Configuration revocation also wins on this path.
#[tokio::test]
async fn lost_label_response_is_adopted_or_revoked_without_repeating_the_write() {
    for revoke in [false, true] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        let ctx = opened(false);
        policy(&server, &label_rules(false)).await;
        response(
            &server,
            "GET",
            "/repos/example/project/labels",
            200,
            json!([{"name":"triage"},{"name":"bug"}]),
        )
        .await;
        response(
            &server,
            "POST",
            "/repos/example/project/issues/3/labels",
            503,
            json!({"message":"lost"}),
        )
        .await;
        assert!(runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
            .await
            .is_err());
        assert_eq!(writes(&server).await.len(), 1);
        drop(runtime);
        let runtime = Runtime::open(&dir.0).unwrap();
        server.reset().await;
        let rules = if revoke {
            String::new()
        } else {
            label_rules(false)
        };
        policy(&server, &rules).await;
        response(
            &server,
            "GET",
            "/repos/example/project/issues/3/labels",
            200,
            if revoke {
                json!([])
            } else {
                json!([{"name":"triage"},{"name":"bug"}])
            },
        )
        .await;
        runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
        let key = opened_key(9, 88, "triage-a");
        assert_eq!(
            runtime.store.get(&key).unwrap().unwrap().state,
            if revoke {
                State::Superseded
            } else {
                State::Succeeded
            }
        );
        assert!(writes(&server).await.is_empty());
    }
}

fn path_sync() -> EventContext {
    let payload = json!({
        "action":"synchronize",
        "repository":{"id":9,"full_name":"example/project"},
        "installation":{"id":7},
        "pull_request":{"id":88,"number":3,"updated_at":"2026-10-03T01:00:00Z",
            "user":{"id":5,"login":"alice","type":"User"},
            "head":{"sha":"new-head"},"base":{"sha":"base"}}
    });
    EventContext::capture("pull_request", &payload, Some("sync-1"))
        .unwrap()
        .unwrap()
}

fn path_rules() -> &'static str {
    "[path_triggers]\nevents=['pull_request.synchronize']\n[[path_triggers.rules]]\nid='rust'\ninclude=['src/**/*.rs']\nexclude=['src/generated/**']\nlabels=['area/rust']\n[[path_triggers.rules]]\nid='docs'\ninclude=['docs/**']\nlabels=['documentation']"
}

#[tokio::test]
async fn path_rules_match_complete_diff_and_coalesce_existing_labels() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, path_rules()).await;
    path_pr_meta(&server, 2).await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3/files",
        200,
        json!([
            {"filename":"src/lib.rs","status":"modified"},
            {"filename":"docs/guide.md","status":"added"}
        ]),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example/project/labels",
        200,
        json!([{"name":"area/rust"},{"name":"documentation"}]),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
    let ctx = path_sync();
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .unwrap();
    let requests = writes(&server).await;
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["labels"], json!(["area/rust", "documentation"]));
    let mut duplicate = ctx.clone();
    duplicate.delivery = "sync-duplicate".into();
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &duplicate)
        .await
        .unwrap();
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn path_rules_skip_missing_labels_without_blocking_existing_labels() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, path_rules()).await;
    path_pr_meta(&server, 2).await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3/files",
        200,
        json!([
            {"filename":"src/lib.rs","status":"modified"},
            {"filename":"docs/guide.md","status":"added"}
        ]),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example/project/labels",
        200,
        json!([{"name":"area/rust"}]),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &path_sync())
        .await
        .unwrap();
    let requests = writes(&server).await;
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["labels"], json!(["area/rust"]));
}

#[tokio::test]
async fn path_rules_reject_partial_or_malformed_file_lists_without_writes() {
    for files in [
        json!([{"filename":"src/lib.rs","status":"renamed"}]),
        json!([
            {"filename":"src/lib.rs","status":"modified"},
            {"filename":"docs/guide.md","status":"added"}
        ]),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        policy(&server, path_rules()).await;
        path_pr_meta(&server, 1).await;
        response(
            &server,
            "GET",
            "/repos/example/project/pulls/3/files",
            200,
            files,
        )
        .await;
        let result = runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &path_sync())
            .await;
        assert!(result.is_err());
        assert!(writes(&server).await.is_empty());
        assert!(runtime.store.list(None).unwrap().is_empty());
    }
}

#[tokio::test]
async fn path_rules_keep_renames_and_exclusions_semantic() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, path_rules()).await;
    path_pr_meta(&server, 1).await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3/files",
        200,
        json!([
            {"filename":"src/generated/new.rs","previous_filename":"docs/old.md","status":"renamed"}
        ]),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example/project/labels",
        200,
        json!([{"name":"documentation"}]),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &path_sync())
        .await
        .unwrap();
    let writes = writes(&server).await;
    assert_eq!(writes.len(), 1);
    let body: Value = serde_json::from_slice(&writes[0].body).unwrap();
    assert_eq!(body["labels"], json!(["documentation"]));
}

#[tokio::test]
async fn never_started_plan_resumes_once_but_a_revoked_unclaimed_plan_stays_revoked() {
    for revoke in [false, true] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        let ctx = opened(false);
        runtime.store.opened_plan(&json!(["opened-plan",9,88]).to_string(),&json!([{"rules":["triage-a","triage-b"],"action":{"AddLabels":["bug","triage"]},"config_sha":"old-config"}])).unwrap();
        drop(runtime);
        let runtime = Runtime::open(&dir.0).unwrap();
        let rules = if revoke {
            String::new()
        } else {
            label_rules(false)
        };
        policy(&server, &rules).await;
        labels(&server).await;
        runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
        assert_eq!(writes(&server).await.len(), usize::from(!revoke));
        server.reset().await;
        policy(&server, &label_rules(false)).await;
        labels(&server).await;
        runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
        assert!(writes(&server).await.is_empty());
    }
}

#[tokio::test]
async fn interrupted_ai_without_receipt_pauses_instead_of_recomputing() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    let ctx = opened(true);
    let cfg = review_fixture(&server, 201).await;
    let key = opened_key(9, 88, "review");
    runtime
        .store
        .opened_plan(
            &json!(["opened-plan", 9, 88]).to_string(),
            &json!([{"rules":["review","same-review"],"action":"Review","config_sha":"config"}]),
        )
        .unwrap();
    let mut op = spec(&key, "opened");
    op.context = ctx.clone();
    unknown(&runtime.store, &op, true);
    assert!(runtime
        .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
        .await
        .is_err());
    assert!(writes(&server).await.is_empty());
    assert_eq!(
        runtime.store.get(&key).unwrap().unwrap().state,
        State::Unknown
    );
}
