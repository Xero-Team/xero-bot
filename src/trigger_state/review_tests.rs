//! Regression coverage for the PR review and the subsequent recovery audit.
use super::*;

/// A successful empty DELETE response must not become an ambiguous operation.
#[tokio::test]
async fn bodyless_delete_accepts_204_and_preserves_api_failures() {
    let server = MockServer::start().await;
    let gh = client(&server);
    Mock::given(method("DELETE"))
        .and(path("/repos/example/project/git/refs/heads/old"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    gh.delete("/repos/example/project/git/refs/heads/old")
        .await
        .unwrap();
    for status in [403, 404, 503] {
        server.reset().await;
        response(
            &server,
            "DELETE",
            "/denied",
            status,
            json!({"message":"denied"}),
        )
        .await;
        assert!(
            matches!(gh.delete("/denied").await,Err(crate::github::GhError::Api{status:actual,..}) if actual==status)
        );
    }
}

/// Fast deliveries arriving during a slow review can use idle worker slots.
#[tokio::test]
async fn pump_refills_workers_while_an_earlier_delivery_is_still_running() {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    let dir = Dir::new();
    let runtime = Arc::new(Runtime::open(&dir.0).unwrap());
    let mut slow = context();
    slow.delivery = "a-slow".into();
    runtime.store.enqueue(&slow, 0).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let fast_done = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let task = {
        let runtime = runtime.clone();
        let started = started.clone();
        let release = release.clone();
        let fast_done = fast_done.clone();
        let active = active.clone();
        let peak = peak.clone();
        tokio::spawn(async move {
            runtime
                .pump_with(
                    |ctx| {
                        let started = started.clone();
                        let release = release.clone();
                        let fast_done = fast_done.clone();
                        let active = active.clone();
                        let peak = peak.clone();
                        async move {
                            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(current, Ordering::SeqCst);
                            if ctx.delivery == "a-slow" {
                                started.notify_one();
                                release.notified().await;
                            } else {
                                fast_done.store(true, Ordering::SeqCst);
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                            active.fetch_sub(1, Ordering::SeqCst);
                            Ok(())
                        }
                    },
                    std::time::Duration::from_secs(5),
                    std::time::Duration::from_millis(5),
                )
                .await
        })
    };
    started.notified().await;
    for i in 0..20 {
        let mut next = context();
        next.delivery = format!("fast-{i:02}");
        next.comment_id = Some(100 + i);
        runtime.store.enqueue(&next, 0).unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !fast_done.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(active.load(Ordering::SeqCst) > 0);
    release.notify_one();
    assert_eq!(task.await.unwrap().unwrap(), 21);
    assert!((2..=8).contains(&peak.load(Ordering::SeqCst)));
}

/// One delivery's timeout/panic cannot expire another worker's active claim.
#[tokio::test]
async fn interrupted_delivery_recovers_only_its_own_attempts() {
    for panic in [false, true] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        for name in ["broken", "healthy"] {
            let mut ctx = context();
            ctx.delivery = name.into();
            runtime.store.enqueue(&ctx, 0).unwrap();
        }
        let runtime = &runtime;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        runtime
            .pump_with(
                |ctx| {
                    let barrier = barrier.clone();
                    async move {
                        let mut action = spec(&ctx.delivery, "comment");
                        action.context = ctx.clone();
                        let claim = runtime.store.claim(&action, 0)?;
                        let claim = claim.unwrap();
                        runtime.store.mark_sent(&claim, 0)?;
                        barrier.wait().await;
                        if ctx.delivery == "broken" {
                            if panic {
                                panic!("injected worker panic");
                            }
                            std::future::pending::<()>().await;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        runtime.store.finish(
                            &claim,
                            State::Succeeded,
                            None,
                            "healthy completed",
                            1,
                        )?;
                        Ok(())
                    }
                },
                std::time::Duration::from_millis(70),
                std::time::Duration::from_millis(5),
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.store.get("broken").unwrap().unwrap().state,
            State::Unknown
        );
        assert_eq!(
            runtime.store.get("healthy").unwrap().unwrap().state,
            State::Succeeded
        );
        assert_eq!(
            runtime
                .store
                .inbox_status()
                .unwrap()
                .iter()
                .find(|r| r["delivery"] == "broken")
                .unwrap()["state"],
            "pending"
        );
    }
}

/// A redelivery that reclaims a business key owns the new lease, not its creator.
#[test]
fn recovery_tracks_the_current_delivery_owner_after_reclaim() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let mut action = spec("shared", "comment");
    action.context.delivery = "original".into();
    let old = unknown(&store, &action, false);
    store
        .resolve_unknown(&old, "not_sent", None, "unsent", 102)
        .unwrap();
    action.context.delivery = "redelivery".into();
    let current = store.claim(&action, 103).unwrap().unwrap();
    store.mark_sent(&current, 103).unwrap();
    assert_eq!(current.operation.spec.context.delivery, "original");
    assert_eq!(current.operation.lease_delivery, "redelivery");
    assert_eq!(store.recover_delivery("original", 104).unwrap(), 0);
    assert_eq!(store.get("shared").unwrap().unwrap().state, State::Running);
    assert_eq!(store.recover_delivery("redelivery", 104).unwrap(), 1);
    assert_eq!(store.get("shared").unwrap().unwrap().state, State::Unknown);
}

/// Evidence for an old unsent attempt cannot requeue a newer ambiguous write.
#[tokio::test]
async fn stale_reconciliation_is_fenced_after_a_new_attempt_becomes_unknown() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let action = spec("fenced-recovery", "comment");
    let old = unknown(&store, &action, false);
    store
        .resolve_unknown(&old, "not_sent", None, "unsent", 102)
        .unwrap();
    let second = store.claim(&action, 103).unwrap().unwrap();
    store.mark_sent(&second, 103).unwrap();
    store
        .recover_delivery(&action.context.delivery, 104)
        .unwrap();
    let server = MockServer::start().await;
    assert!(reconcile(&store, &client(&server), 1, &old, 105)
        .await
        .is_err());
    let current = store.get(&action.key).unwrap().unwrap();
    assert_eq!(current.state, State::Unknown);
    assert!(current.sent);
    assert_eq!(current.attempts, 2);
    assert!(store.claim(&action, 200).unwrap().is_none());
}

/// Existing version-one volumes retain evidence when the lease-owner column is added.
#[test]
fn version_one_database_migrates_without_losing_operations() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let claim = store
        .claim(&spec("migrate", "comment"), 100)
        .unwrap()
        .unwrap();
    store.mark_sent(&claim, 100).unwrap();
    drop(store);
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch("DROP INDEX operation_lease; ALTER TABLE operations DROP COLUMN lease_delivery; UPDATE trigger_meta SET version=1;").unwrap();
    drop(db);
    let store = Store::open(&dir.0).unwrap();
    let op = store.get("migrate").unwrap().unwrap();
    assert_eq!(op.state, State::Unknown);
    assert!(op.sent);
    assert_eq!(op.lease_delivery, context().delivery);
}

/// Expired successful envelopes are removed without resetting durable ledgers.
#[test]
fn inbox_retention_preserves_failed_unknown_and_permanent_notification_state() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    for i in 0..5 {
        let mut ctx = context();
        ctx.delivery = format!("retained-{i}");
        store.enqueue(&ctx, 0).unwrap();
        let item = store.claim_inbox(0).unwrap().unwrap();
        if i == 1 {
            store.finish_inbox(&item, Some("retry"), 0).unwrap();
            continue;
        }
        if i >= 2 {
            let mut action = spec(&format!("action-{i}"), "comment");
            action.context = ctx;
            let claim = store
                .claim_notification(&action, &[format!("user-{i}")], 10, 0)
                .unwrap()
                .unwrap();
            store
                .finish(
                    &claim,
                    match i {
                        2 => State::Unknown,
                        3 => State::Failed,
                        _ => State::Succeeded,
                    },
                    None,
                    "result",
                    0,
                )
                .unwrap();
        }
        store.finish_inbox(&item, None, 0).unwrap();
    }
    assert_eq!(store.cleanup_inbox(30 * 86400).unwrap(), 0);
    assert_eq!(store.cleanup_inbox(30 * 86400 + 1).unwrap(), 2);
    let rows = store.inbox_status().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows
        .iter()
        .all(|r| r["delivery"] != "retained-0" && r["delivery"] != "retained-4"));
    assert_eq!(
        store.get("action-4").unwrap().unwrap().recipients,
        vec!["user-4"]
    );
    assert_eq!(
        store.get("action-2").unwrap().unwrap().state,
        State::Unknown
    );
}

/// Cursor pagination bounds diagnostics even for a large persisted backlog.
#[test]
fn inbox_pagination_has_no_gaps_or_duplicates() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    for i in 0..205 {
        let mut ctx = context();
        ctx.delivery = format!("page-{i:03}");
        store.enqueue(&ctx, 0).unwrap();
    }
    let first = store.inbox_status().unwrap();
    assert_eq!(first.len(), 100);
    let next = store
        .inbox_status_page(first.last().unwrap()["delivery"].as_str(), 100)
        .unwrap();
    assert_eq!(next.len(), 100);
    let last = store
        .inbox_status_page(next.last().unwrap()["delivery"].as_str(), 100)
        .unwrap();
    assert_eq!(last.len(), 5);
    assert_eq!(last.last().unwrap()["delivery"], "page-204");
}

/// Absent labels are already removed; their 404 is not a failed suboperation.
#[tokio::test]
async fn deleting_an_absent_label_records_a_completed_idempotent_effect() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    policy(&server, "").await;
    response(
        &server,
        "DELETE",
        "/repos/example/project/issues/3/labels/missing",
        404,
        json!({"message":"Not Found"}),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":1}),
    )
    .await;
    let mut ctx = context();
    ctx.body = Some("@bot label -missing".into());
    runtime
        .process(
            &client(&server),
            &cfg(),
            &RepositoryConfigCache::default(),
            &ctx,
        )
        .await
        .unwrap();
    assert!(runtime.store.list(Some(State::Failed)).unwrap().is_empty());
    assert_eq!(runtime.store.list(Some(State::Succeeded)).unwrap().len(), 3);
}

/// DELETEs with a body still need the returned assignee list, never Null.
#[tokio::test]
async fn delete_with_body_keeps_the_assignment_response() {
    let server = MockServer::start().await;
    response(
        &server,
        "DELETE",
        "/repos/example/project/issues/3/assignees",
        200,
        json!({"assignees":[{"login":"bob"}]}),
    )
    .await;
    assert_eq!(
        client(&server)
            .remove_assignees("example/project", 3, &["alice".into()])
            .await
            .unwrap(),
        vec!["bob"]
    );
}

/// Concurrent deliveries of the same command share exactly one HTTP attempt.
#[tokio::test]
async fn concurrent_pump_deliveries_deduplicate_the_actual_comment_write() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    policy(&server, "").await;
    Mock::given(method("POST"))
        .and(path("/repos/example/project/issues/3/comments"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"id":3}))
                .set_delay(std::time::Duration::from_millis(10)),
        )
        .expect(1)
        .mount(&server)
        .await;
    for i in 0..20 {
        let mut ctx = context();
        ctx.delivery = format!("duplicate-{i:02}");
        runtime.store.enqueue(&ctx, 0).unwrap();
    }
    let gh = client(&server);
    let cfg = cfg();
    let cache = RepositoryConfigCache::default();
    let runtime = &runtime;
    let gh = &gh;
    let cfg = &cfg;
    let cache = &cache;
    assert_eq!(
        runtime
            .pump_with(
                |ctx| async move { runtime.process(gh, cfg, cache, &ctx).await },
                std::time::Duration::from_secs(3),
                std::time::Duration::from_millis(5)
            )
            .await
            .unwrap(),
        20
    );
    assert_eq!(runtime.store.list(Some(State::Succeeded)).unwrap().len(), 2);
    assert!(runtime.store.list(Some(State::Unknown)).unwrap().is_empty());
}

/// Bodyless success inside a durable command is persisted and skipped on replay.
#[tokio::test]
async fn delete_204_is_succeeded_in_a_durable_frame_and_not_repeated() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    policy(&server, "").await;
    Mock::given(method("DELETE"))
        .and(path("/repos/example/project/issues/3/labels/old"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":1}),
    )
    .await;
    let mut ctx = context();
    ctx.body = Some("@bot label -old".into());
    let gh = client(&server);
    let cfg = cfg();
    let cache = RepositoryConfigCache::default();
    runtime.process(&gh, &cfg, &cache, &ctx).await.unwrap();
    runtime.process(&gh, &cfg, &cache, &ctx).await.unwrap();
    assert_eq!(runtime.store.list(Some(State::Succeeded)).unwrap().len(), 3);
    assert!(runtime.store.list(Some(State::Unknown)).unwrap().is_empty());
}

/// Retrying the unsent assignment after a successful reviewer request must
/// preserve the confirmed reviewer response without issuing a second request.
#[tokio::test]
async fn audit_reviewer_receipt_survives_partial_command_recovery() {
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
    Mock::given(method("POST"))
        .and(path("/repos/example/project/pulls/3/requested_reviewers"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(
                json!({"requested_reviewers":[{"login":"bob","private":"discard"}]}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/assignees",
        503,
        json!({"message":"uncertain"}),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":45}),
    )
    .await;
    let mut ctx = context();
    ctx.is_pr = true;
    ctx.body = Some("r? @bob".into());
    let cache = RepositoryConfigCache::default();
    assert!(runtime.process(&gh, &cfg(), &cache, &ctx).await.is_err());
    let interrupted = runtime
        .store
        .list(Some(State::Unknown))
        .unwrap()
        .into_iter()
        .find(|op| op.spec.request["route"] == "/repos/example/project/issues/3/assignees")
        .unwrap();
    runtime
        .store
        .administer(
            &interrupted.spec.key,
            "confirm-not-sent",
            "Fixture proxy proves no upstream write",
            crate::github::chrono_now_secs(),
        )
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/repos/example/project/issues/3/assignees"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"assignees":[{"login":"bob"}]})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    runtime.process(&gh, &cfg(), &cache, &ctx).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let reply = requests
        .iter()
        .find(|r| r.method == "POST" && r.url.path().ends_with("/comments"))
        .unwrap();
    let body = serde_json::from_slice::<Value>(&reply.body).unwrap();
    assert!(
        body["body"]
            .as_str()
            .unwrap()
            .contains("✅ Requested a review from @bob."),
        "{body}"
    );
    let receipt = runtime
        .store
        .list(None)
        .unwrap()
        .into_iter()
        .find(|op| op.spec.request["route"] == "/repos/example/project/pulls/3/requested_reviewers")
        .unwrap();
    assert_eq!(
        receipt.result.unwrap(),
        json!({"requested_reviewers":[{"login":"bob"}]})
    );
}
