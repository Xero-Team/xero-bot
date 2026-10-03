//! PR #25 review regressions exercise policy, revocation and report recovery.
use super::*;

/// A definite invalid policy consumes the creation event; repairing it cannot
/// turn old threads into new subscriptions. Transport failures retain retries.
#[tokio::test]
async fn invalid_event_domain_is_terminal_and_cannot_backfill_after_repair() {
    let duplicate = format!("{}\n{}", label_rules(false), label_rules(false));
    let oversized = (0..33).map(|i| format!("[[event_triggers]]\nid='r-{i}'\nevent='issues.opened'\ncommand='label'\nadd=['bug']\n")).collect::<String>();
    for invalid in [duplicate, oversized, "[invalid TOML".into()] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        let cfg = cfg();
        let ctx = opened(false);
        policy(&server, &invalid).await;
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
                std::time::Duration::from_secs(10),
                std::time::Duration::from_millis(5),
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.store.inbox_status().unwrap()[0]["state"],
            "succeeded",
            "invalid domain must not retry"
        );
        assert!(writes(&server).await.is_empty());
        runtime
            .store
            .cleanup_inbox(crate::github::chrono_now_secs() + 31 * 86400)
            .unwrap();
        assert!(runtime.store.inbox_status().unwrap().is_empty());
        drop(runtime);
        let runtime = Runtime::open(&dir.0).unwrap();
        server.reset().await;
        policy(&server, &label_rules(false)).await;
        labels(&server).await;
        let mut duplicate = ctx;
        duplicate.delivery = "redelivery-after-policy-repair".into();
        runtime
            .process(&gh, &cfg, &RepositoryConfigCache::default(), &duplicate)
            .await
            .unwrap();
        assert!(
            writes(&server).await.is_empty(),
            "fixed config backfilled an old thread"
        );
    }
}

/// Both initial planning and recovery must refuse a semantic domain error.
#[tokio::test]
async fn invalid_domain_revokes_a_frozen_plan_even_before_its_first_claim() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let ctx = opened(false);
    runtime.store.opened_plan(&json!(["opened-plan",9,88]).to_string(),&json!([{"rules":["triage-a","triage-b"],"action":{"AddLabels":["bug","triage"]},"config_sha":"old-config"}])).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    policy(
        &server,
        &format!("{}\n{}", label_rules(false), label_rules(false)),
    )
    .await;
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .store
            .get(&opened_key(9, 88, "triage-a"))
            .unwrap()
            .unwrap()
            .state,
        State::Superseded
    );
    server.reset().await;
    policy(&server, &label_rules(false)).await;
    labels(&server).await;
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .unwrap();
    assert!(writes(&server).await.is_empty());
}

/// Label normalization and the reserved-label gate must use the same Unicode
/// equivalence, otherwise the inventory restores the control label's spelling.
#[tokio::test]
async fn unicode_control_labels_cannot_bypass_the_deployment_veto() {
    for field in [0, 1, 2] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        let mut cfg = cfg();
        let label = "队列/ÜBER";
        match field {
            0 => cfg.label_merge_queue_queued = label.into(),
            1 => cfg.label_merge_queue_testing = label.into(),
            _ => cfg.codeql_label = label.into(),
        };
        policy(&server,"[[event_triggers]]\nid='unicode'\nevent='issues.opened'\ncommand='label'\nadd=['队列/über']").await;
        response(
            &server,
            "GET",
            "/repos/example/project/labels",
            200,
            json!([{"name":label}]),
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
            .process(&gh, &cfg, &RepositoryConfigCache::default(), &opened(false))
            .await
            .unwrap();
        assert!(
            writes(&server).await.is_empty(),
            "reserved label reached GitHub"
        );
    }
}

/// Policy revocation must persist before an unreliable reconciliation GET;
/// restoring the policy later cannot revive the old operation or its children.
#[tokio::test]
async fn revocation_survives_reconciliation_failure_and_later_policy_restore() {
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
    server.reset().await;
    policy(&server, "").await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/labels",
        403,
        json!({"message":"temporarily refused"}),
    )
    .await;
    assert!(runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .is_err());
    let key = opened_key(9, 88, "triage-a");
    assert_eq!(
        runtime.store.get(&key).unwrap().unwrap().state,
        State::Superseded
    );
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    server.reset().await;
    policy(&server, &label_rules(false)).await;
    labels(&server).await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
    runtime
        .process(&gh, &cfg(), &RepositoryConfigCache::default(), &ctx)
        .await
        .unwrap();
    assert!(writes(&server).await.is_empty());
    assert!(runtime
        .store
        .children(&key)
        .unwrap()
        .iter()
        .all(|c| c.state == State::Superseded));
}

/// Cancellation after a fallback announcement is not a completed AI report.
#[tokio::test]
async fn agent_fallback_notice_is_not_published_or_adopted_as_the_final_report() {
    let _serial = REVIEW_TEST.lock().await;
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let gh = client(&server);
    let mut cfg = review_fixture(&server, 201).await;
    cfg.review_engine = "agent".into();
    cfg.agent_max_turns = 0;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":83}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(10))
                .set_body_json(json!({})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let ctx = opened(true);
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(500),
        runtime.process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
    )
    .await
    .is_err());
    let requests = writes(&server).await;
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/chat/completions")
            .count(),
        1
    );
    assert!(
        requests
            .iter()
            .all(|r| !r.url.path().ends_with("/comments")),
        "a non-final fallback notice escaped"
    );
    runtime
        .store
        .recover_delivery(&ctx.delivery, crate::github::chrono_now_secs())
        .unwrap();
    assert!(runtime
        .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
        .await
        .is_err());
    assert_eq!(
        runtime
            .store
            .get(&opened_key(9, 88, "review"))
            .unwrap()
            .unwrap()
            .state,
        State::Unknown
    );
    assert_eq!(writes(&server).await.len(), requests.len());
}

/// Older persisted announcement receipts cannot settle an unfinished planner.
#[tokio::test]
async fn recovery_never_treats_a_legacy_notice_as_a_completed_report() {
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
            &json!([{"rules":["review"],"action":"Review","config_sha":"config"}]),
        )
        .unwrap();
    let mut parent = spec(&key, "opened");
    parent.context = ctx.clone();
    unknown(&runtime.store, &parent, true);
    let mut notice = spec("legacy-notice", "comment");
    notice.parent = Some(key.clone());
    notice.context = ctx.clone();
    let claim = runtime.store.claim(&notice, 100).unwrap().unwrap();
    runtime.store.mark_sent(&claim, 100).unwrap();
    runtime
        .store
        .finish(
            &claim,
            State::Succeeded,
            Some(&json!({"id":83})),
            "fallback notice posted",
            101,
        )
        .unwrap();
    assert!(runtime
        .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
        .await
        .is_err());
    assert_eq!(
        runtime.store.get(&key).unwrap().unwrap().state,
        State::Unknown
    );
    assert!(writes(&server).await.is_empty());
}

/// A failed configuration read is not evidence of a disabled subscription.
#[tokio::test]
async fn transient_config_outage_keeps_the_creation_inbox_retryable() {
    for status in [403, 429, 503] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        let cfg = cfg();
        let ctx = opened(false);
        response(
            &server,
            "GET",
            "/repos/example/project",
            status,
            json!({"message":"temporarily unavailable"}),
        )
        .await;
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
                std::time::Duration::from_secs(10),
                std::time::Duration::from_millis(5),
            )
            .await
            .unwrap();
        assert_eq!(runtime.store.inbox_status().unwrap()[0]["state"], "pending");
        assert!(runtime.store.list(None).unwrap().is_empty());
        server.reset().await;
        policy(&server, &label_rules(false)).await;
        labels(&server).await;
        runtime
            .process(&gh, &cfg, &RepositoryConfigCache::default(), &ctx)
            .await
            .unwrap();
        assert_eq!(writes(&server).await.len(), 1);
    }
}

/// Equal array lengths alone do not prove the complete diff: missing or
/// duplicate names must not turn a malformed file response into a clean report.
#[tokio::test]
async fn malformed_automatic_file_lists_cannot_publish_a_clean_codeql_report() {
    for files in [
        json!([{}]),
        json!([{"filename":""}]),
        json!([{"filename":"a.rs"},{"filename":"a.rs"}]),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let gh = client(&server);
        policy(
            &server,
            "[[event_triggers]]\nid='scan'\nevent='pull_request.opened'\ncommand='codeql'",
        )
        .await;
        response(&server,"GET","/repos/example/project/pulls/3",200,json!({"head":{"sha":"new-head"},"base":{"sha":"base"},"changed_files":files.as_array().unwrap().len()})).await;
        response(
            &server,
            "GET",
            "/repos/example/project/pulls/3/files",
            200,
            files,
        )
        .await;
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
            "POST",
            "/repos/example/project/issues/3/comments",
            201,
            json!({"id":89}),
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
        let result = runtime
            .store
            .get(&opened_key(9, 88, "scan"))
            .unwrap()
            .unwrap();
        assert_eq!(result.state, State::Failed);
        let requests = writes(&server).await;
        assert_eq!(requests.len(), 1);
        assert!(String::from_utf8_lossy(&requests[0].body).contains("Failed"));
    }
}
