//! A local persistence error must stop admission without cancelling sibling writes.
use super::*;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Install a deterministic SQL fault before the runtime takes exclusive ownership.
fn fault_runtime(dir: &Dir, sql: &str) -> Runtime {
    drop(Store::open(&dir.0).unwrap());
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch(sql).unwrap();
    drop(db);
    Runtime::open(&dir.0).unwrap()
}

/// Give each test delivery an independent canonical source identity.
fn event(delivery: &str, comment: i64) -> EventContext {
    EventContext {
        delivery: delivery.into(),
        comment_id: Some(comment),
        ..context()
    }
}

/// Both bookkeeping calls and stale inbox leases must drain an in-flight POST.
#[tokio::test]
async fn bookkeeping_errors_drain_sibling_http_writes_and_stop_new_claims() {
    for (sql, expected) in [
        (
            "CREATE TRIGGER fail_finish BEFORE UPDATE ON inbox
          WHEN OLD.delivery='broken' AND NEW.state='succeeded'
          BEGIN SELECT RAISE(ABORT,'finish failed'); END;",
            "finish failed",
        ),
        (
            "CREATE TRIGGER fail_recovery BEFORE UPDATE ON operations
          WHEN OLD.key='broken' AND NEW.state='unknown'
          BEGIN SELECT RAISE(ABORT,'recovery failed'); END;",
            "recovery failed",
        ),
        (
            "CREATE TRIGGER stale_lease AFTER UPDATE ON inbox
          WHEN NEW.delivery='broken' AND OLD.state='pending' AND NEW.state='running'
          BEGIN UPDATE inbox SET attempts=attempts+1 WHERE delivery=NEW.delivery; END;",
            "stale inbox lease",
        ),
    ] {
        let dir = Dir::new();
        let runtime = fault_runtime(&dir, sql);
        runtime.store.enqueue(&event("broken", 1), 0).unwrap();
        runtime.store.enqueue(&event("healthy", 2), 0).unwrap();
        let server = MockServer::start().await;
        let received = Arc::new(tokio::sync::Notify::new());
        let signal = received.clone();
        Mock::given(method("POST"))
            .and(path("/write"))
            .respond_with(move |_: &wiremock::Request| {
                signal.notify_one();
                ResponseTemplate::new(201)
                    .set_body_json(json!({"id": 123}))
                    .set_delay(Duration::from_millis(80))
            })
            .expect(1)
            .mount(&server)
            .await;
        let gh = client(&server);
        let completed = AtomicBool::new(false);
        let runtime = &runtime;
        let gh = &gh;
        let completed = &completed;
        let result = runtime
            .pump_with(
                |ctx| {
                    let received = received.clone();
                    async move {
                        let mut action = spec(&ctx.delivery, "other");
                        action.context = ctx.clone();
                        let claim = runtime.store.claim(&action, 0)?.unwrap();
                        runtime.store.mark_sent(&claim, 0)?;
                        if ctx.delivery == "broken" {
                            received.notified().await;
                            runtime.store.enqueue(&event("later", 3), 0)?;
                            if expected != "recovery failed" {
                                runtime.store.finish(
                                    &claim,
                                    State::Succeeded,
                                    None,
                                    "confirmed",
                                    1,
                                )?;
                            }
                            // Only the recovery-fault case leaves an unfinished operation.
                            return Ok(());
                        }
                        assert_eq!(ctx.delivery, "healthy", "new work admitted after failure");
                        let value = gh.post("/write", Some(json!({"body":"hello"}))).await?;
                        runtime.store.finish(
                            &claim,
                            State::Succeeded,
                            Some(&value),
                            "confirmed",
                            1,
                        )?;
                        completed.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                },
                Duration::from_secs(2),
                Duration::from_millis(5),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains(expected));
        assert!(
            completed.load(Ordering::SeqCst),
            "healthy POST was cancelled by {expected}"
        );
        assert_eq!(
            runtime.store.get("healthy").unwrap().unwrap().state,
            State::Succeeded
        );
        let rows = runtime.store.inbox_status().unwrap();
        assert_eq!(
            rows.iter().find(|r| r["delivery"] == "healthy").unwrap()["state"],
            "succeeded"
        );
        let later = rows.iter().find(|r| r["delivery"] == "later").unwrap();
        assert_eq!(later["state"], "pending");
        assert_eq!(later["attempts"], 0);
    }
}

/// Admission failure after a POST starts must wait for its confirmed receipt.
#[tokio::test]
async fn claim_failure_drains_an_already_sent_request() {
    let dir = Dir::new();
    let runtime = fault_runtime(
        &dir,
        "CREATE TRIGGER fail_claim BEFORE UPDATE ON inbox
        WHEN NEW.delivery='poison' AND NEW.state='running'
        BEGIN SELECT RAISE(ABORT,'claim failed'); END;",
    );
    runtime.store.enqueue(&event("healthy", 1), 0).unwrap();
    let server = MockServer::start().await;
    let received = Arc::new(tokio::sync::Notify::new());
    let signal = received.clone();
    Mock::given(method("POST"))
        .and(path("/write"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            ResponseTemplate::new(201)
                .set_body_json(json!({"id":456}))
                .set_delay(Duration::from_millis(80))
        })
        .expect(1)
        .mount(&server)
        .await;
    let gh = client(&server);
    let runtime = &runtime;
    let gh = &gh;
    let pump = runtime.pump_with(
        |ctx| async move {
            assert_eq!(ctx.delivery, "healthy");
            let mut action = spec("healthy", "other");
            action.context = ctx;
            let claim = runtime.store.claim(&action, 0)?.unwrap();
            runtime.store.mark_sent(&claim, 0)?;
            let result = gh.post("/write", Some(json!({"body":"hello"}))).await?;
            runtime
                .store
                .finish(&claim, State::Succeeded, Some(&result), "confirmed", 1)?;
            Ok(())
        },
        Duration::from_secs(2),
        Duration::from_millis(5),
    );
    let admit = async {
        received.notified().await;
        runtime.store.enqueue(&event("poison", 2), 0).unwrap();
        runtime.store.enqueue(&event("unclaimed", 3), 0).unwrap();
    };
    let (result, ()) = tokio::join!(pump, admit);
    assert!(result.unwrap_err().to_string().contains("claim failed"));
    assert_eq!(
        runtime.store.get("healthy").unwrap().unwrap().state,
        State::Succeeded
    );
    assert!(runtime
        .store
        .inbox_status()
        .unwrap()
        .iter()
        .filter(|r| r["delivery"] != "healthy")
        .all(|r| r["attempts"] == 0));
}

/// A periodic cleanup error must follow the same drain path as worker failures.
#[tokio::test(start_paused = true)]
async fn maintenance_failure_drains_active_workers_without_admitting_more() {
    let dir = Dir::new();
    let runtime = fault_runtime(
        &dir,
        "CREATE TRIGGER fail_cleanup BEFORE DELETE ON inbox
        BEGIN SELECT RAISE(ABORT,'cleanup failed'); END;",
    );
    runtime.store.enqueue(&event("controller", 1), 0).unwrap();
    runtime.store.enqueue(&event("healthy", 2), 0).unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let runtime = &runtime;
    let result = runtime
        .pump_with(
            |ctx| {
                let barrier = barrier.clone();
                async move {
                    let mut action = spec(&ctx.delivery, "other");
                    action.context = ctx.clone();
                    let claim = runtime.store.claim(&action, 0)?.unwrap();
                    runtime.store.mark_sent(&claim, 0)?;
                    barrier.wait().await;
                    if ctx.delivery == "controller" {
                        // Initial cleanup has already completed. Introduce an expired
                        // envelope and make the production 60-second timer eligible.
                        runtime.store.enqueue(&event("expired", 3), 0)?;
                        let expired = runtime.store.claim_inbox(0)?.unwrap();
                        runtime.store.finish_inbox(&expired, None, 0)?;
                        runtime.store.enqueue(&event("later", 4), 0)?;
                        tokio::time::advance(Duration::from_secs(61)).await;
                    } else {
                        assert_eq!(ctx.delivery, "healthy");
                        tokio::time::sleep(Duration::from_secs(100)).await;
                    }
                    runtime
                        .store
                        .finish(&claim, State::Succeeded, None, "confirmed", 1)?;
                    Ok(())
                }
            },
            Duration::from_secs(120),
            Duration::from_secs(1),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("cleanup failed"));
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
            .find(|r| r["delivery"] == "later")
            .unwrap()["attempts"],
        0
    );
}

/// Further drain errors must not replace the original failure or cancel healthy work.
#[tokio::test(start_paused = true)]
async fn draining_retains_first_error_when_another_worker_also_fails() {
    let dir = Dir::new();
    let runtime = fault_runtime(
        &dir,
        "
        CREATE TRIGGER fail_first BEFORE UPDATE ON inbox
        WHEN NEW.delivery='first' AND NEW.state='succeeded'
        BEGIN SELECT RAISE(ABORT,'first failure'); END;
        CREATE TRIGGER fail_second BEFORE UPDATE ON inbox
        WHEN NEW.delivery='second' AND NEW.state='succeeded'
        BEGIN SELECT RAISE(ABORT,'second failure'); END;",
    );
    for (id, name) in ["first", "second", "healthy"].iter().enumerate() {
        runtime
            .store
            .enqueue(&event(name, id as i64 + 1), 0)
            .unwrap();
    }
    let runtime = &runtime;
    let result = runtime
        .pump_with(
            |ctx| async move {
                let delay = match ctx.delivery.as_str() {
                    "first" => 1,
                    "second" => 2,
                    "healthy" => 3,
                    _ => panic!("new work admitted while draining"),
                };
                let mut action = spec(&ctx.delivery, "other");
                action.context = ctx.clone();
                let claim = runtime.store.claim(&action, 0)?.unwrap();
                runtime.store.mark_sent(&claim, 0)?;
                tokio::time::sleep(Duration::from_secs(delay)).await;
                runtime
                    .store
                    .finish(&claim, State::Succeeded, None, "confirmed", 1)?;
                if ctx.delivery == "first" {
                    runtime.store.enqueue(&event("later", 4), 0)?;
                }
                Ok(())
            },
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("first failure"));
    assert_eq!(runtime.store.list(Some(State::Succeeded)).unwrap().len(), 3);
    let rows = runtime.store.inbox_status().unwrap();
    assert_eq!(
        rows.iter().find(|r| r["delivery"] == "healthy").unwrap()["state"],
        "succeeded"
    );
    assert_eq!(
        rows.iter().find(|r| r["delivery"] == "later").unwrap()["attempts"],
        0
    );
}

/// Draining must still cancel a genuinely hung item at its own deadline.
#[tokio::test(start_paused = true)]
async fn draining_preserves_each_items_timeout_and_uncertainty_handling() {
    let dir = Dir::new();
    let runtime = fault_runtime(
        &dir,
        "CREATE TRIGGER fail_finish BEFORE UPDATE ON inbox
        WHEN NEW.delivery='broken' AND NEW.state='succeeded'
        BEGIN SELECT RAISE(ABORT,'bookkeeping failure'); END;",
    );
    runtime.store.enqueue(&event("broken", 1), 0).unwrap();
    runtime.store.enqueue(&event("hung", 2), 0).unwrap();
    let started = tokio::time::Instant::now();
    let runtime = &runtime;
    let result = runtime
        .pump_with(
            |ctx| async move {
                let mut action = spec(&ctx.delivery, "other");
                action.context = ctx.clone();
                let claim = runtime.store.claim(&action, 0)?.unwrap();
                runtime.store.mark_sent(&claim, 0)?;
                if ctx.delivery == "hung" {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                runtime
                    .store
                    .finish(&claim, State::Succeeded, None, "confirmed", 1)?;
                Ok(())
            },
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("bookkeeping failure"));
    assert_eq!(started.elapsed(), Duration::from_secs(5));
    let hung = runtime.store.get("hung").unwrap().unwrap();
    assert_eq!(hung.state, State::Unknown);
    assert!(hung.sent);
    let rows = runtime.store.inbox_status().unwrap();
    let deferred = rows.iter().find(|r| r["delivery"] == "hung").unwrap();
    assert_eq!(deferred["state"], "pending");
    assert!(deferred["next_at"].as_i64().unwrap() > crate::github::chrono_now_secs());
}
