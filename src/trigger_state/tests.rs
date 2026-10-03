use super::*;
use crate::config::cache::RepositoryConfigCache;
use crate::config::Config;
use crate::github::Client;
use base64::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Dir(PathBuf);
impl Dir {
    /// Allocate isolated temporary state for persistence and restart tests.
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "xero-triggers-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    /// Remove only the fixture-owned state directory.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a complete source event with extra token fields that must be discarded.
fn payload() -> Value {
    json!({"action":"created","repository":{"id":9,"full_name":"example/project","token":"never-store-me"},"installation":{"id":7,"token":"never-store-me"},
        "issue":{"id":88,"number":3,"user":{"login":"alice"}},
        "comment":{"id":20,"created_at":"2026-10-02T01:00:00Z","body":"@bot ping","user":{"id":5,"login":"alice","type":"User"}},"token":"never-store-me"})
}
/// Capture the standard comment fixture using the production allowlist.
fn context() -> EventContext {
    EventContext::capture("issue_comment", &payload(), Some("delivery-1"))
        .unwrap()
        .unwrap()
}
/// Construct a stable operation intent for storage and recovery tests.
fn spec(key: &str, kind: &str) -> OperationSpec {
    OperationSpec {
        key: key.into(),
        parent: None,
        kind: kind.into(),
        context: context(),
        config_sha: Some("config-a".into()),
        request: json!({"method":"POST","route":"/repos/example/project/issues/3/comments","body":{"body":"hello"}}),
    }
}
/// Leave a claimed operation interrupted, with optional persisted send intent.
fn unknown(store: &Store, spec: &OperationSpec, sent: bool) -> Operation {
    let claim = store.claim(spec, 100).unwrap().unwrap();
    if sent {
        store.mark_sent(&claim, 100).unwrap();
    }
    store.recover_running(100, 101).unwrap();
    store.get(&spec.key).unwrap().unwrap()
}

/// Verify that context is allowlisted and invalid identity never becomes an accepted event.
#[test]
fn context_is_allowlisted_and_invalid_identity_never_becomes_an_accepted_event() {
    let ctx = context();
    let saved = serde_json::to_string(&ctx).unwrap();
    assert!(!saved.contains("never-store-me"));
    assert_eq!(ctx.comment_id, Some(20));
    assert_eq!(ctx.user_id, Some(5));
    assert!(EventContext::capture("issue_comment", &payload(), None).is_err());
    let mut bad = payload();
    bad["repository"]["id"] = json!(0);
    assert!(EventContext::capture("issue_comment", &bad, Some("d")).is_err());
    assert!(
        EventContext::capture("issue_comment", &json!({"action":"edited"}), None)
            .unwrap()
            .is_none()
    );
    assert!(
        EventContext::capture("pull_request_review", &json!({"action":"submitted"}), None)
            .unwrap()
            .is_none()
    );
    let mut p = payload();
    p["action"] = json!("synchronize");
    p["pull_request"] = json!({"id":88,"number":3,"updated_at":"2026-10-02T01:01:00Z","head":{"sha":"head"},"base":{"sha":"base"},"user":{"id":5,"login":"alice"}});
    let pr = EventContext::capture("pull_request", &p, Some("p"))
        .unwrap()
        .unwrap();
    assert_eq!(pr.head_sha.as_deref(), Some("head"));
    assert_eq!(pr.base_sha.as_deref(), Some("base"));
}

/// Verify that keys use semantic parameters and never delivery config or head.
#[test]
fn keys_use_semantic_parameters_and_never_delivery_config_or_head() {
    use crate::commands::{parse_commands, Command};
    let key = |text| manual_key(9, 20, &parse_commands("bot", text).commands[0].command);
    assert_eq!(key("claim"), key("take"));
    assert_eq!(key("r= @Alice"), key("r+ as @alice"));
    assert_eq!(key("cc @bob @ALICE"), key("cc @alice @bob @bob"));
    assert_ne!(
        manual_key(9, 20, &Command::Ping),
        manual_key(9, 21, &Command::Ping)
    );
    assert_ne!(opened_key(9, 3, "a"), opened_key(9, 3, "b"));
    assert_eq!(
        recipient_key(9, 88, "Alice").unwrap(),
        recipient_key(9, 88, "alice").unwrap()
    );
    assert!(recipient_key(9, 88, "org/team").is_err());
}

/// Verify that concurrent deliveries claim one business action and one inbox owner.
#[test]
fn concurrent_deliveries_claim_one_business_action_and_one_inbox_owner() {
    let dir = Dir::new();
    let store = Arc::new(Store::open(&dir.0).unwrap());
    let barrier = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|i| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut ctx = context();
                ctx.delivery = format!("delivery-{}", i % 4);
                store.enqueue(&ctx, 100).unwrap();
                let mut action = spec("same-business-action", "comment");
                action.context = ctx;
                barrier.wait();
                store.claim(&action, 100).unwrap().is_some()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter(|h| h.thread().id() != std::thread::current().id())
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let mut claimed = 0;
    while store.claim_inbox(100).unwrap().is_some() {
        claimed += 1;
    }
    assert_eq!(claimed, 4);
    assert_eq!(store.inbox_status().unwrap().len(), 4);
    let mut conflict = context();
    conflict.body = Some("changed".into());
    conflict.delivery = "delivery-0".into();
    assert!(store.enqueue(&conflict, 101).is_err());
}

/// Verify that restart preserves unknown success and rejects stale completions.
#[test]
fn restart_preserves_unknown_success_and_rejects_stale_completions() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    store.enqueue(&context(), 100).unwrap();
    let inbox = store.claim_inbox(100).unwrap().unwrap();
    let a = store
        .claim(&spec("success", "comment"), 100)
        .unwrap()
        .unwrap();
    store
        .finish(&a, State::Succeeded, Some(&json!({"id":1})), "ok", 100)
        .unwrap();
    let running = store
        .claim(&spec("interrupted", "comment"), 100)
        .unwrap()
        .unwrap();
    store.mark_sent(&running, 100).unwrap();
    assert!(Store::open(&dir.0).is_err());
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    assert_eq!(
        store.get("interrupted").unwrap().unwrap().state,
        State::Unknown
    );
    assert_eq!(
        store.get("success").unwrap().unwrap().state,
        State::Succeeded
    );
    assert!(store
        .claim(&spec("interrupted", "comment"), 200)
        .unwrap()
        .is_none());
    assert!(store
        .finish(&running, State::Succeeded, None, "late result", 200)
        .is_err());
    assert!(store.finish_inbox(&inbox, None, 200).is_err());
    store.resume_inbox(200).unwrap();
    assert!(store.claim_inbox(200).unwrap().is_some());
}

/// Verify that notifications share transactional lifetime budget and release only proven unsent.
#[test]
fn notifications_share_transactional_lifetime_budget_and_release_only_proven_unsent() {
    let dir = Dir::new();
    let store = Arc::new(Store::open(&dir.0).unwrap());
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut s = spec(&format!("rule-{i}"), "comment");
                s.context.head_sha = Some(format!("head-{i}"));
                s.config_sha = Some(format!("config-{i}"));
                barrier.wait();
                store
                    .claim_notification(
                        &s,
                        &["Shared".into(), format!("person-{i}"), format!("extra-{i}")],
                        10,
                        100,
                    )
                    .unwrap()
                    .unwrap()
            })
        })
        .collect();
    let claims: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        claims
            .iter()
            .map(|c| c.operation.recipients.len())
            .sum::<usize>(),
        10
    );
    let reserved = claims
        .iter()
        .find(|c| !c.operation.recipients.is_empty())
        .unwrap();
    store.mark_sent(reserved, 100).unwrap();
    store
        .finish(reserved, State::Unknown, None, "timeout", 100)
        .unwrap();
    let later = store
        .claim_notification(
            &spec("push-after-reopen", "comment"),
            &["new-person".into()],
            10,
            200,
        )
        .unwrap()
        .unwrap();
    assert!(later.operation.recipients.is_empty());
    store
        .resolve_unknown(
            &store.get(&reserved.operation.spec.key).unwrap().unwrap(),
            "not_sent",
            None,
            "operator proved no send",
            200,
        )
        .unwrap();
    let released = store
        .claim_notification(
            &spec("after-release", "comment"),
            &["new-person".into()],
            10,
            201,
        )
        .unwrap()
        .unwrap();
    assert_eq!(released.operation.recipients, vec!["new-person"]);
    store
        .finish(&released, State::Succeeded, None, "remote confirmed", 201)
        .unwrap();
    let same = store
        .claim_notification(
            &spec("new-rule-same-recipient", "comment"),
            &["NEW-PERSON".into()],
            10,
            202,
        )
        .unwrap()
        .unwrap();
    assert!(same.operation.recipients.is_empty());
    let zero = store
        .claim_notification(&spec("budget-zero", "comment"), &["nobody".into()], 0, 202)
        .unwrap()
        .unwrap();
    assert!(zero.operation.recipients.is_empty());
    assert!(store
        .claim_notification(&spec("bad-budget", "comment"), &["person".into()], 11, 202)
        .is_err());
}

/// Verify that sessions are scoped persistent and ordered by source not arrival.
#[test]
fn sessions_are_scoped_persistent_and_ordered_by_source_not_arrival() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let wake = SessionWake {
        installation_id: 7,
        repository_id: 9,
        thread_number: 88,
        user_id: 5,
        comment_id: 20,
        source_at: 1_000_000,
    };
    store.record_wake(&wake).unwrap();
    let mut call = SessionWake {
        comment_id: 21,
        source_at: 1_000_001,
        ..wake.clone()
    };
    assert!(store.session_before(&call, 30).unwrap().is_some());
    call.comment_id = 19;
    call.source_at = wake.source_at;
    assert!(store.session_before(&call, 30).unwrap().is_none());
    call.comment_id = 21;
    assert!(store.session_before(&call, 30).unwrap().is_some());
    call.user_id = 6;
    assert!(store.session_before(&call, 30).unwrap().is_none());
    call.user_id = 5;
    call.thread_number = 89;
    assert!(store.session_before(&call, 30).unwrap().is_none());
    call.thread_number = 88;
    call.repository_id = 10;
    assert!(store.session_before(&call, 30).unwrap().is_none());
    call.repository_id = 9;
    call.source_at = wake.source_at + 30 * 86_400_000;
    assert!(store.session_before(&call, 30).unwrap().is_none());
    call.source_at = wake.source_at + 1;
    call.installation_id = 8;
    assert!(store.session_before(&call, 30).unwrap().is_none());
    call.installation_id = 7;
    call.source_at = wake.source_at + 30 * 86_400_000;
    let edited = SessionWake {
        source_at: call.source_at,
        ..wake.clone()
    };
    store.record_wake(&edited).unwrap();
    assert!(store.session_before(&call, 30).unwrap().is_none());
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    call.source_at = wake.source_at + 1;
    assert!(store.session_before(&call, 30).unwrap().is_some());
}

/// A wake can be before the current source comment yet already be expired at
/// execution time; delayed delivery must fail closed in that case.
#[test]
fn session_expiry_is_checked_against_execution_time() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let wake = SessionWake {
        installation_id: 7,
        repository_id: 9,
        thread_number: 88,
        user_id: 5,
        comment_id: 20,
        source_at: 1_000_000,
    };
    store.record_wake(&wake).unwrap();
    let call = SessionWake {
        comment_id: 21,
        source_at: wake.source_at + 1,
        ..wake.clone()
    };
    assert!(store
        .session_before_at(&call, 30, wake.source_at + 10 * 86_400_000)
        .unwrap()
        .is_some());
    assert!(store
        .session_before_at(&call, 30, wake.source_at + 30 * 86_400_000)
        .unwrap()
        .is_none());
}

/// Build an installation-like client against the local mock server.
fn client(server: &MockServer) -> Client {
    Client {
        crab: crate::github::client_builder()
            .personal_token("test-token")
            .base_uri(server.uri())
            .unwrap()
            .build()
            .unwrap(),
        app_slug: "bot".into(),
    }
}
/// Mount a method/path-specific JSON response on the local GitHub mock.
async fn response(server: &MockServer, verb: &str, route: &str, status: u16, value: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(value))
        .mount(server)
        .await;
}

/// Verify that forged marker wrong app and missing result stay unknown without post.
#[tokio::test]
async fn forged_marker_wrong_app_and_missing_result_stay_unknown_without_post() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    for (i, author) in [
        json!({"user":{"login":"alice","type":"User"},"performed_via_github_app":{"id":1}}),
        json!({"user":{"login":"bot[bot]","type":"Bot"},"performed_via_github_app":{"id":999}}),
        json!({"user":{"login":"bot[bot]","type":"Bot"}}),
    ]
    .iter()
    .enumerate()
    {
        server.reset().await;
        let s = spec(&format!("forged-{i}"), "comment");
        let op = unknown(&store, &s, true);
        let mut fake = author.clone();
        fake["body"] = json!(operation_marker(&s.key));
        fake["id"] = json!(1);
        response(
            &server,
            "GET",
            "/repos/example/project/issues/3/comments",
            200,
            json!([fake]),
        )
        .await;
        assert_eq!(
            reconcile(&store, &gh, 1, &op, 102).await.unwrap(),
            Recovery::Paused
        );
        assert_eq!(store.get(&s.key).unwrap().unwrap().state, State::Unknown);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

/// Verify that remote success local crash reconciles comments and reviews by verified marker.
#[tokio::test]
async fn remote_success_local_crash_reconciles_comments_and_reviews_by_verified_marker() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    for kind in ["comment", "review"] {
        server.reset().await;
        let mut s = spec(kind, kind);
        if kind == "review" {
            s.request["route"] = json!("/repos/example/project/pulls/3/reviews");
        }
        let op = unknown(&store, &s, true);
        response(&server,"GET",s.request["route"].as_str().unwrap(),200,json!([{"id":100,"user":{"type":"Bot","login":"bot[bot]"},"performed_via_github_app":{"id":1},"body":format!("done\n\n{}",operation_marker(&s.key))}])).await;
        assert_eq!(
            reconcile(&store, &gh, 1, &op, 103).await.unwrap(),
            Recovery::Confirmed
        );
        assert_eq!(store.get(kind).unwrap().unwrap().state, State::Succeeded);
        assert!(store.claim(&s, 200).unwrap().is_none());
    }
}

/// Verify that labels reconcile then backoff retry but unsent comment retries without network.
#[tokio::test]
async fn labels_reconcile_then_backoff_retry_but_unsent_comment_retries_without_network() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    let s = spec("unsent", "comment");
    let op = unknown(&store, &s, false);
    assert_eq!(
        reconcile(&store, &gh, 1, &op, 102).await.unwrap(),
        Recovery::RetrySafe
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(store.claim(&s, 103).unwrap().is_some());
    let mut s = spec("labels", "ensure_labels");
    s.request = json!({"method":"POST","route":"/repos/example/project/issues/3/labels","body":{"labels":["area/rust"]}});
    let op = unknown(&store, &s, true);
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
    assert_eq!(
        reconcile(&store, &gh, 1, &op, 105).await.unwrap(),
        Recovery::RetrySafe
    );
    assert!(store.claim(&s, 105).unwrap().is_none());
    let claim = store.claim(&s, 115).unwrap().unwrap();
    store.mark_sent(&claim, 115).unwrap();
    store
        .finish(&claim, State::Unknown, None, "timeout", 115)
        .unwrap();
    server.reset().await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/labels",
        200,
        json!([{"name":"area/rust"}]),
    )
    .await;
    assert_eq!(
        reconcile(&store, &gh, 1, &store.get(&s.key).unwrap().unwrap(), 116)
            .await
            .unwrap(),
        Recovery::Confirmed
    );
    assert_eq!(retry_delay(u32::MAX), 3600);
}

/// Verify that admin requires evidence and never blindly retries unknown.
#[test]
fn admin_requires_evidence_and_never_blindly_retries_unknown() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let s = spec("admin", "comment");
    unknown(&store, &s, true);
    assert!(store
        .administer("admin", "retry", "try again", 200)
        .is_err());
    assert!(store
        .administer("admin", "confirm-not-sent", "", 200)
        .is_err());
    store
        .administer(
            "admin",
            "confirm-not-sent",
            "request rejected before upstream by verified proxy trace",
            200,
        )
        .unwrap();
    assert_eq!(store.get("admin").unwrap().unwrap().state, State::Pending);
}

/// Serve a verified default-branch repository configuration to runtime tests.
async fn policy(server: &MockServer, text: &str) {
    response(
        server,
        "GET",
        "/repos/example/project",
        200,
        json!({"id":9,"default_branch":"main"}),
    )
    .await;
    response(
        server,
        "GET",
        "/repos/example/project/git/ref/heads/main",
        200,
        json!({"object":{"sha":"config"}}),
    )
    .await;
    response(server,"GET","/repos/example/project/contents/.github/xero-bot.toml",200,json!({"sha":"blob","type":"file","encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode(text)})).await;
}
/// Configure deterministic bot identity with idle scheduling disabled.
fn cfg() -> Config {
    let mut cfg = Config::from_env();
    cfg.app_id = "1".into();
    cfg.bot_name = "bot".into();
    cfg.app_slug = "bot".into();
    cfg.idle_workflows_enabled = false;
    cfg
}

/// Verify that production commands survive duplicate delivery and restart without idle.
#[tokio::test]
async fn production_commands_survive_duplicate_delivery_and_restart_without_idle() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":44}),
    )
    .await;
    let ctx = context();
    runtime.store.enqueue(&ctx, 100).unwrap();
    let cache = RepositoryConfigCache::default();
    runtime.process(&gh, &cfg(), &cache, &ctx).await.unwrap();
    let mut duplicate = ctx.clone();
    duplicate.delivery = "different-delivery".into();
    runtime
        .process(&gh, &cfg(), &cache, &duplicate)
        .await
        .unwrap();
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    runtime.process(&gh, &cfg(), &cache, &ctx).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let posts: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert_eq!(posts.len(), 1);
    assert!(String::from_utf8_lossy(&posts[0].body).contains("xero-trigger:"));
    assert_eq!(runtime.store.list(Some(State::Succeeded)).unwrap().len(), 2);
}

/// A real explicit mention opens a durable session that a later bare command
/// can use after restart; the consumer never reads the GitHub comment history.
#[tokio::test]
async fn explicit_wake_authorizes_later_bare_command_without_history_scan() {
    let dir = Dir::new();
    let server = MockServer::start().await;
    policy(&server, "").await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id": 44}),
    )
    .await;
    let first = context();
    let runtime = Runtime::open(&dir.0).unwrap();
    runtime
        .process(
            &client(&server),
            &cfg(),
            &RepositoryConfigCache::default(),
            &first,
        )
        .await
        .unwrap();
    drop(runtime);

    let mut later = first.clone();
    later.delivery = "delivery-2".into();
    later.comment_id = Some(21);
    later.source_time = "2026-10-02T02:00:00Z".into();
    later.body = Some("ping".into());
    let runtime = Runtime::open(&dir.0).unwrap();
    runtime
        .process(
            &client(&server),
            &cfg(),
            &RepositoryConfigCache::default(),
            &later,
        )
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        2
    );
    assert!(requests
        .iter()
        .filter(|request| request.method == "GET")
        .all(|request| { !request.url.path().ends_with("/issues/3/comments") }));
}

/// Verify that partial comment failure never repeats successful command or unknown write.
#[tokio::test]
async fn partial_comment_failure_never_repeats_successful_command_or_unknown_write() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    Mock::given(method("POST"))
        .and(path("/repos/example/project/issues/3/comments"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            ResponseTemplate::new(if body["body"].as_str().unwrap().starts_with("pong") {
                201
            } else {
                503
            })
            .set_body_json(json!({"id":40,"message":"unavailable"}))
        })
        .mount(&server)
        .await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/comments",
        200,
        json!([]),
    )
    .await;
    let mut ctx = context();
    ctx.body = Some("@bot ping; cc @bob".into());
    let cache = RepositoryConfigCache::default();
    assert!(runtime.process(&gh, &cfg(), &cache, &ctx).await.is_err());
    assert!(runtime.process(&gh, &cfg(), &cache, &ctx).await.is_err());
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        2
    );
    assert_eq!(runtime.store.list(Some(State::Succeeded)).unwrap().len(), 2);
    assert_eq!(runtime.store.list(Some(State::Unknown)).unwrap().len(), 2);
}

/// Verify that unavailable configuration is pending and recovery obeys new disabled policy.
#[tokio::test]
async fn unavailable_configuration_is_pending_and_recovery_obeys_new_disabled_policy() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    response(
        &server,
        "GET",
        "/repos/example/project",
        503,
        json!({"message":"unavailable"}),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":77}),
    )
    .await;
    let ctx = context();
    runtime.store.enqueue(&ctx, 100).unwrap();
    let item = runtime.store.claim_inbox(100).unwrap().unwrap();
    let result = runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await;
    assert!(result.is_err());
    runtime
        .store
        .finish_inbox(&item, Some("config unavailable"), 100)
        .unwrap();
    assert_eq!(runtime.store.inbox_status().unwrap()[0]["state"], "pending");
    server.reset().await;
    policy(&server, "[command_triggers]\nping={mode='disabled'}").await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":78}),
    )
    .await;
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .unwrap();
    for request in server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == "POST")
    {
        assert!(!String::from_utf8_lossy(&request.body).contains("pong"));
    }
    assert!(runtime
        .store
        .list(None)
        .unwrap()
        .iter()
        .all(|op| op.spec.kind != "command"));
}

/// Verify that old head is superseded before any recovered write.
#[tokio::test]
async fn old_head_is_superseded_before_any_recovered_write() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3",
        200,
        json!({"head":{"sha":"new-head"},"base":{"sha":"base"}}),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3/commits",
        200,
        json!([]),
    )
    .await;
    let mut ctx = context();
    ctx.is_pr = true;
    let key = manual_key(9, 20, &crate::commands::Command::Ping);
    let mut action = spec(&key, "command");
    action.context = ctx.clone();
    action.context.head_sha = Some("old-head".into());
    action.context.base_sha = Some("base".into());
    unknown(&runtime.store, &action, false);
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .unwrap();
    assert_eq!(
        runtime.store.get(&key).unwrap().unwrap().state,
        State::Superseded
    );
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

/// Verify that resumed approval rechecks current permission before reaching approval api.
#[tokio::test]
async fn resumed_approval_rechecks_current_permission_before_reaching_approval_api() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3",
        200,
        json!({"head":{"sha":"head"},"base":{"sha":"base"}}),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3/commits",
        200,
        json!([]),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example/project/collaborators/alice/permission",
        200,
        json!({"permission":"read"}),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":8}),
    )
    .await;
    let mut ctx = context();
    ctx.is_pr = true;
    ctx.author = Some("bob".into());
    ctx.body = Some("@bot r+".into());
    let key = manual_key(
        9,
        20,
        &crate::commands::Command::Approve { on_behalf_of: None },
    );
    let mut action = spec(&key, "command");
    action.context = ctx.clone();
    unknown(&runtime.store, &action, false);
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .any(|r| r.url.path().ends_with("/permission")));
    assert!(requests
        .iter()
        .all(|r| !(r.method == "POST" && r.url.path().ends_with("/reviews"))));
}

/// Verify that retry of failed suboperation does not repeat successful label write.
#[tokio::test]
async fn retry_of_failed_suboperation_does_not_repeat_successful_label_write() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
    response(
        &server,
        "DELETE",
        "/repos/example/project/issues/3/labels/old",
        503,
        json!({"message":"lost response"}),
    )
    .await;
    let mut ctx = context();
    ctx.body = Some("@bot label +new -old".into());
    let cache = RepositoryConfigCache::default();
    assert!(runtime.process(&gh, &cfg(), &cache, &ctx).await.is_err());
    let pending = runtime
        .store
        .list(Some(State::Unknown))
        .unwrap()
        .into_iter()
        .find(|op| op.spec.kind == "other")
        .unwrap();
    runtime
        .store
        .administer(
            &pending.spec.key,
            "confirm-not-sent",
            "verified upstream request was not forwarded",
            crate::github::chrono_now_secs(),
        )
        .unwrap();
    Mock::given(method("DELETE"))
        .and(path("/repos/example/project/issues/3/labels/old"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .with_priority(1)
        .mount(&server)
        .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":45}),
    )
    .await;
    runtime.process(&gh, &cfg(), &cache, &ctx).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "POST" && r.url.path().ends_with("/labels"))
            .count(),
        1
    );
    assert_eq!(requests.iter().filter(|r| r.method == "DELETE").count(), 2);
    assert!(runtime.store.list(Some(State::Unknown)).unwrap().is_empty());
}

/// Verify that actual remote success local commit failure recovers without second post.
#[tokio::test]
async fn actual_remote_success_local_commit_failure_recovers_without_second_post() {
    let dir = Dir::new();
    drop(Store::open(&dir.0).unwrap());
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_receipt BEFORE UPDATE ON operations WHEN NEW.state='succeeded' BEGIN SELECT RAISE(ABORT,'simulated crash before receipt'); END;").unwrap();
    drop(db);
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":45}),
    )
    .await;
    let ctx = context();
    let cache = RepositoryConfigCache::default();
    assert!(runtime.process(&gh, &cfg(), &cache, &ctx).await.is_err());
    let posted = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.method == "POST")
        .unwrap();
    let body: Value = serde_json::from_slice(&posted.body).unwrap();
    drop(runtime);
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch("DROP TRIGGER fail_receipt;").unwrap();
    drop(db);
    response(&server,"GET","/repos/example/project/issues/3/comments",200,json!([{"id":45,"user":{"type":"Bot","login":"bot[bot]"},"performed_via_github_app":{"id":1},"body":body["body"]}])).await;
    let runtime = Runtime::open(&dir.0).unwrap();
    runtime.process(&gh, &cfg(), &cache, &ctx).await.unwrap();
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        1
    );
    assert!(runtime.store.list(Some(State::Unknown)).unwrap().is_empty());
}

/// Verify that storage failure at send boundary makes zero external writes.
#[tokio::test]
async fn storage_failure_at_send_boundary_makes_zero_external_writes() {
    let dir = Dir::new();
    drop(Store::open(&dir.0).unwrap());
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_intent BEFORE UPDATE ON operations WHEN NEW.sent=1 BEGIN SELECT RAISE(ABORT,'intent persistence failed'); END;").unwrap();
    drop(db);
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    assert!(runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &context())
        .await
        .is_err());
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

/// Verify that abandoned notification reservations survive restart and sent work cannot be superseded.
#[test]
fn abandoned_notification_reservations_survive_restart_and_sent_work_cannot_be_superseded() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let claim = store
        .claim_notification(&spec("reserved", "comment"), &["alice".into()], 1, 100)
        .unwrap()
        .unwrap();
    store.mark_sent(&claim, 100).unwrap();
    assert!(store
        .finish(&claim, State::Superseded, None, "new head", 101)
        .is_err());
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    let unknown = store.get("reserved").unwrap().unwrap();
    assert_eq!(unknown.state, State::Unknown);
    assert_eq!(unknown.recipients, vec!["alice"]);
    let next = store
        .claim_notification(&spec("next-head", "comment"), &["bob".into()], 1, 200)
        .unwrap()
        .unwrap();
    assert!(next.operation.recipients.is_empty());
}

/// Verify that cancellation after http send stays unknown and never reposts absent marker.
#[tokio::test]
async fn cancellation_after_http_send_stays_unknown_and_never_reposts_absent_marker() {
    use std::sync::atomic::AtomicBool;
    let dir = Dir::new();
    let runtime = Arc::new(Runtime::open(&dir.0).unwrap());
    let server = MockServer::start().await;
    policy(&server, "").await;
    let reached = Arc::new(AtomicBool::new(false));
    let observed = reached.clone();
    Mock::given(method("POST"))
        .and(path("/repos/example/project/issues/3/comments"))
        .respond_with(move |_: &wiremock::Request| {
            observed.store(true, Ordering::SeqCst);
            ResponseTemplate::new(201)
                .set_body_json(json!({"id":67}))
                .set_delay(std::time::Duration::from_secs(5))
        })
        .mount(&server)
        .await;
    let task_runtime = runtime.clone();
    let gh = client(&server);
    let task = tokio::spawn(async move {
        task_runtime
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &context())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !reached.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/comments",
        200,
        json!([]),
    )
    .await;
    assert!(runtime
        .process(
            &client(&server),
            &cfg(),
            &RepositoryConfigCache::default(),
            &context()
        )
        .await
        .is_err());
    assert_eq!(runtime.store.list(Some(State::Unknown)).unwrap().len(), 2);
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        1
    );
}

/// Verify that failed budget transaction rolls back both claim and reservations.
#[test]
fn failed_budget_transaction_rolls_back_both_claim_and_reservations() {
    let dir = Dir::new();
    drop(Store::open(&dir.0).unwrap());
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_budget BEFORE INSERT ON recipients WHEN NEW.login='bob' BEGIN SELECT RAISE(ABORT,'budget write failed'); END;").unwrap();
    drop(db);
    let store = Store::open(&dir.0).unwrap();
    assert!(store
        .claim_notification(
            &spec("atomic", "comment"),
            &["alice".into(), "bob".into()],
            10,
            100
        )
        .is_err());
    assert!(store.get("atomic").unwrap().is_none());
    let next = store
        .claim_notification(
            &spec("after-rollback", "comment"),
            &["alice".into()],
            1,
            100,
        )
        .unwrap()
        .unwrap();
    assert_eq!(next.operation.recipients, vec!["alice"]);
}

/// Verify that a late worker cannot complete a new attempt.
#[test]
fn a_late_worker_cannot_complete_a_new_attempt() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let s = spec("fenced", "comment");
    let first = store.claim(&s, 100).unwrap().unwrap();
    store.recover_running(100, 101).unwrap();
    store
        .resolve_unknown(
            &store.get(&s.key).unwrap().unwrap(),
            "not_sent",
            None,
            "no send intent",
            102,
        )
        .unwrap();
    let second = store.claim(&s, 103).unwrap().unwrap();
    assert_eq!(second.attempt, first.attempt + 1);
    assert!(store
        .finish(&first, State::Succeeded, None, "late", 104)
        .is_err());
    store
        .finish(&second, State::Succeeded, None, "current", 104)
        .unwrap();
}

/// Verify that administrative success requires assignment receipt and resumes failed parent.
#[test]
fn administrative_success_requires_assignment_receipt_and_resumes_failed_parent() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let parent = store
        .claim(&spec("parent", "command"), 100)
        .unwrap()
        .unwrap();
    store
        .finish(&parent, State::Failed, None, "partial", 101)
        .unwrap();
    let mut action = spec("assignment", "other");
    action.parent = Some("parent".into());
    action.request["route"] = json!("/repos/example/project/issues/3/assignees");
    unknown(&store, &action, true);
    assert!(store
        .administer("assignment", "confirm-success", "verified on GitHub", 102)
        .is_err());
    store
        .administer_with_receipt(
            "assignment",
            "confirm-success",
            "verified on GitHub",
            Some(&json!({"assignees":[{"login":"alice"}],"token":"never-store-me"})),
            102,
        )
        .unwrap();
    assert_eq!(store.get("parent").unwrap().unwrap().state, State::Pending);
    let assignment = store.get("assignment").unwrap().unwrap();
    assert_eq!(assignment.state, State::Succeeded);
    assert_eq!(
        assignment.result.unwrap(),
        json!({"assignees":[{"login":"alice"}]})
    );
}

/// Verify that claim insert failure keeps inbox retryable even without an operation row.
#[tokio::test]
async fn claim_insert_failure_keeps_inbox_retryable_even_without_an_operation_row() {
    let dir = Dir::new();
    drop(Store::open(&dir.0).unwrap());
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_claim BEFORE INSERT ON operations BEGIN SELECT RAISE(ABORT,'claim persistence failed'); END;").unwrap();
    drop(db);
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(&server, "").await;
    let ctx = context();
    runtime.store.enqueue(&ctx, 100).unwrap();
    let item = runtime.store.claim_inbox(100).unwrap().unwrap();
    assert!(runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .is_err());
    runtime
        .store
        .finish_inbox(&item, Some("claim failed"), 100)
        .unwrap();
    assert_eq!(runtime.store.inbox_status().unwrap()[0]["state"], "pending");
    assert!(runtime.store.list(None).unwrap().is_empty());
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[path = "review_tests.rs"]
mod review;

#[path = "drain_tests.rs"]
mod drain;
