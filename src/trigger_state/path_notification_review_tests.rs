//! PR #27 review regressions: permanent recipient errors cannot keep inbox work alive.
use super::*;
use std::time::Duration;

/// Drive the production inbox admission, backoff and completion boundary.
async fn pump_paths(runtime: &Runtime, server: &MockServer) -> usize {
    let gh = client(server);
    let config = cfg();
    let cache = RepositoryConfigCache::default();
    runtime
        .pump_with(
            |ctx| {
                let (gh, config, cache) = (&gh, &config, &cache);
                async move { runtime.process(gh, config, cache, &ctx).await }
            },
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await
        .unwrap()
}

/// Definite identity errors terminate the snapshot, survive restart, and let
/// its inbox envelope complete without repeatedly consuming and releasing slots.
#[tokio::test]
async fn invalid_recipients_finish_the_inbox_and_never_retry_the_snapshot() {
    for (status, identity) in [
        (404, json!({"message":"Not Found"})),
        (200, json!({"id":42,"login":"alice","type":"Bot"})),
        (200, json!({"id":42,"login":"alice","type":"Organization"})),
        (200, json!({"id":42,"login":"renamed-alice","type":"User"})),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        verified(&runtime);
        let server = MockServer::start().await;
        fixture(
            &server,
            &(cc_rules(2, &["alice", "bob"]) + "labels=['area/rust']\n"),
            "first",
            files(),
            201,
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/users/alice"))
            .respond_with(ResponseTemplate::new(status).set_body_json(identity))
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;
        let ctx = path_sync();
        runtime.store.enqueue(&ctx, 0).unwrap();
        assert_eq!(pump_paths(&runtime, &server).await, 1);
        assert_eq!(pump_paths(&runtime, &server).await, 0);
        let op = notifications(&runtime).remove(0);
        assert_eq!(op.state, State::Failed);
        assert_eq!(op.attempts, 1);
        assert!(!op.sent);
        assert!(op.recipients.is_empty());
        let result = op.result.unwrap();
        assert_eq!(result["invalid_recipient"], "alice");
        assert_eq!(result["rules"], json!(["rust"]));
        assert!(op.detail.contains("alice"));
        assert_eq!(
            runtime.store.inbox_status().unwrap()[0]["state"],
            "succeeded"
        );
        assert!(writes(&server)
            .await
            .iter()
            .any(|r| r.url.path().ends_with("/labels")));
        drop(runtime);

        let runtime = Runtime::open(&dir.0).unwrap();
        let mut redelivery = ctx.clone();
        redelivery.delivery = "after-restart".into();
        runtime.store.enqueue(&redelivery, 0).unwrap();
        assert_eq!(pump_paths(&runtime, &server).await, 1);
        run_paths(&runtime, &server, &ctx).await.unwrap();
        assert_eq!(notifications(&runtime)[0].attempts, 1);
        assert!(runtime
            .store
            .inbox_status()
            .unwrap()
            .iter()
            .all(|i| i["state"] == "succeeded"));
        assert!(comments(&server).await.is_empty());
        server.verify().await;

        // A corrected rule on a later snapshot can still notify a valid user.
        server.reset().await;
        fixture(&server, &cc_rules(2, &["bob"]), "second", files(), 201).await;
        let mut later = ctx;
        later.head_sha = Some("second".into());
        run_paths(&runtime, &server, &later).await.unwrap();
        let bodies = comments(&server).await;
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].contains("cc @bob"));
    }
}

/// Rate limits, server faults and malformed identity reads remain unsent and
/// retryable; once the read recovers they publish once and finish the inbox.
#[tokio::test]
async fn transient_recipient_lookup_errors_preserve_retry_and_recover_once() {
    for (status, body) in [
        (403, json!({"message":"API rate limit exceeded"})),
        (429, json!({"message":"Too Many Requests"})),
        (503, json!({"message":"Service Unavailable"})),
        (200, json!({"login":"alice","type":"User"})),
        (200, json!({"id":42,"login":"alice"})),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        verified(&runtime);
        let server = MockServer::start().await;
        fixture(
            &server,
            &cc_rules(2, &["alice", "bob"]),
            "head",
            files(),
            201,
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/users/alice"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;
        let ctx = path_sync();
        runtime.store.enqueue(&ctx, 0).unwrap();
        assert_eq!(pump_paths(&runtime, &server).await, 1);
        let op = notifications(&runtime).remove(0);
        assert_eq!(op.state, State::Pending);
        assert!(!op.sent);
        assert!(op.recipients.is_empty());
        assert!(op.detail.contains("preflight ended before send:"));
        let inbox = runtime.store.inbox_status().unwrap();
        assert_eq!(inbox[0]["state"], "pending");
        assert!(inbox[0]["next_at"].as_i64().unwrap() > crate::github::chrono_now_secs());
        assert!(comments(&server).await.is_empty());
        server.verify().await;
        server.reset().await;
        fixture(
            &server,
            &cc_rules(2, &["alice", "bob"]),
            "head",
            files(),
            201,
        )
        .await;

        // Advance the durable retry clock without sleeping through backoff.
        let at = crate::github::chrono_now_secs() + 3600;
        let item = runtime.store.claim_inbox(at).unwrap().unwrap();
        let recovered = run_paths(&runtime, &server, &item.context).await;
        assert!(
            recovered.is_ok(),
            "status {status}: {recovered:?}; operations {:?}",
            runtime.store.list(None).unwrap()
        );
        runtime.store.finish_inbox(&item, None, at).unwrap();
        let op = notifications(&runtime).remove(0);
        assert_eq!(op.state, State::Succeeded);
        assert_eq!(op.attempts, 2);
        assert_eq!(op.recipients, vec!["alice", "bob"]);
        assert_eq!(
            runtime.store.inbox_status().unwrap()[0]["state"],
            "succeeded"
        );
        assert_eq!(comments(&server).await.len(), 1);
    }
}

/// Repository database IDs isolate ledgers, while replacing an installation
/// cannot create a second lifetime budget for the same repository and PR.
#[test]
fn installation_changes_preserve_lifetime_dedup_and_repository_isolation() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let first = store
        .claim_notification(
            &spec("before-reinstall", "comment"),
            &["alice".into()],
            1,
            1,
        )
        .unwrap()
        .unwrap();
    store.mark_sent(&first, 1).unwrap();
    store
        .finish(&first, State::Succeeded, None, "accepted", 2)
        .unwrap();
    let mut reinstalled = spec("after-reinstall", "comment");
    reinstalled.context.installation_id += 1;
    let next = store
        .claim_notification(&reinstalled, &["alice".into(), "bob".into()], 1, 3)
        .unwrap()
        .unwrap();
    assert!(next.operation.recipients.is_empty());
    assert_eq!(next.operation.spec.request["suppressed"], json!(["bob"]));
    assert_eq!(
        next.operation.spec.context.installation_id,
        reinstalled.context.installation_id
    );

    let mut other = reinstalled.clone();
    other.key = "different-repository".into();
    other.context.repository_id += 1;
    assert_eq!(
        store
            .claim_notification(&other, &["alice".into()], 1, 4)
            .unwrap()
            .unwrap()
            .operation
            .recipients,
        vec!["alice"]
    );
    other.key = "different-pr".into();
    other.context.repository_id = reinstalled.context.repository_id;
    other.context.thread_id += 1;
    assert_eq!(
        store
            .claim_notification(&other, &["alice".into()], 1, 4)
            .unwrap()
            .unwrap()
            .operation
            .recipients,
        vec!["alice"]
    );
}
