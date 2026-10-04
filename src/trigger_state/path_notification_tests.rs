//! Issue #17: production planner, real SQLite transactions and mock GitHub API.
use super::path_review_fixes::run_paths;
use super::*;

#[path = "path_notification_review_tests.rs"]
mod review;

#[path = "notification_scope_tests.rs"]
mod installation_scope;

/// Build trusted path policy with a chosen lifetime cap and explicit recipients.
fn cc_rules(max: u8, users: &[&str]) -> String {
    format!("[path_triggers]\nmax_cc_users_per_pr={max}\nevents=['pull_request.opened','pull_request.synchronize']\n[[path_triggers.rules]]\nid='rust'\ninclude=['src/**']\ncc={}\n",serde_json::to_string(users).unwrap())
}

/// Attest the fixture PR has no historical recipients before enabling CC.
fn verified(runtime: &Runtime) {
    runtime
        .store
        .restore_notification_ledger(
            7,
            9,
            88,
            &[],
            "Fixture: complete empty historical ledger",
            1,
        )
        .unwrap();
}

/// Mount the complete PR snapshot, label inventory and personal-account endpoints.
async fn fixture(server: &MockServer, rules: &str, head: &str, files: Value, status: u16) {
    policy(server, rules).await;
    response(server,"GET","/repos/example/project/pulls/3",200,json!({
        "number":3,"id":88,"created_at":"2026-01-01T00:00:00Z","head":{"sha":head},
        "base":{"sha":"base","ref":"main"},"changed_files":files.as_array().unwrap().len(),"user":{"login":"alice"}})).await;
    response(
        server,
        "GET",
        "/repos/example/project/pulls/3/files",
        200,
        files,
    )
    .await;
    response(
        server,
        "GET",
        "/repos/example/project/labels",
        200,
        json!([{"name":"area/rust"}]),
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
    response(
        server,
        "POST",
        "/repos/example/project/issues/3/comments",
        status,
        if status < 400 {
            json!({"id":51})
        } else {
            json!({"message":"simulated failure"})
        },
    )
    .await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex("^/users/[a-z0-9-]+$"))
        .respond_with(|request: &wiremock::Request| {
            let login = request.url.path().rsplit('/').next().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({"id":42,"type":"User","login":login}))
        })
        .mount(server)
        .await;
}

/// Provide one complete changed-file record for the path rule.
fn files() -> Value {
    json!([{"filename":"src/lib.rs","status":"modified"}])
}

/// Extract posted notification bodies without counting label requests.
async fn comments(server: &MockServer) -> Vec<String> {
    writes(server)
        .await
        .into_iter()
        .filter(|r| r.url.path().ends_with("/comments"))
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["body"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// Inspect only path-CC operations, excluding imported historical ledger rows.
fn notifications(runtime: &Runtime) -> Vec<Operation> {
    runtime
        .store
        .list(None)
        .unwrap()
        .into_iter()
        .filter(|op| op.spec.request["path_notification"] == true)
        .collect()
}

#[tokio::test]
async fn aggregate_sorted_logins_and_suppressed_count_escape_all_untrusted_display() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    verified(&runtime);
    let server = MockServer::start().await;
    let mut rules = cc_rules(2, &["Zoe", "Alice", "bot"]);
    rules.push_str("[[path_triggers.rules]]\nid='@intruder `unsafe` <b>'\ninclude=['src/**']\ncc=['BOB','alice']\n");
    fixture(
        &server,
        &rules,
        "head",
        json!([{"filename":"src/@outsider `file`<img>.rs","status":"modified"}]),
        201,
    )
    .await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    let bodies = comments(&server).await;
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert!(body.contains("cc @alice @bob"));
    assert_eq!(body.matches('@').count(), 2);
    assert!(body.contains("1 additional recipient(s) suppressed"));
    assert!(body.contains("Checked head: head"));
    assert!(body.contains("＠outsider"));
    let op = notifications(&runtime).remove(0);
    assert_eq!(op.recipients, vec!["alice", "bob"]);
    assert_eq!(op.spec.request["suppressed"], json!(["zoe"]));
}

#[tokio::test]
async fn lifetime_dedup_survives_delivery_heads_rule_recreation_restart_close_and_deleted_comment()
{
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    verified(&runtime);
    let server = MockServer::start().await;
    let rules = cc_rules(10, &["alice"]);
    fixture(&server, &rules, "first", files(), 201).await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert_eq!(comments(&server).await.len(), 1);
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    server.reset().await;
    fixture(
        &server,
        &cc_rules(10, &["ALICE", "bob"]).replace("id='rust'", "id='recreated'"),
        "second",
        files(),
        201,
    )
    .await;
    // Deleted comment deliberately absent: success ledger, not history, governs.
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/comments",
        200,
        json!([]),
    )
    .await;
    let mut ctx = path_sync();
    ctx.delivery = "new-delivery".into();
    ctx.head_sha = Some("second".into());
    for action in ["closed", "reopened"] {
        ctx.action = action.into();
        run_paths(&runtime, &server, &ctx).await.unwrap();
    }
    assert!(comments(&server).await.is_empty());
    ctx.action = "synchronize".into();
    run_paths(&runtime, &server, &ctx).await.unwrap();
    let bodies = comments(&server).await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("cc @bob"));
    assert!(!bodies[0].contains("@alice"));
    assert_eq!(
        notifications(&runtime)
            .iter()
            .flat_map(|o| &o.recipients)
            .count(),
        2
    );
}

#[tokio::test]
async fn zero_one_ten_limits_and_lowering_never_exceed_lifetime_budget() {
    for limit in [0, 1, 10] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        verified(&runtime);
        let server = MockServer::start().await;
        let users: Vec<_> = (0..12).map(|n| format!("user{n:02}")).collect();
        let rules = cc_rules(limit, &users.iter().map(String::as_str).collect::<Vec<_>>())
            + "labels=['area/rust']\n";
        fixture(&server, &rules, "first", files(), 201).await;
        run_paths(&runtime, &server, &path_sync()).await.unwrap();
        let bodies = comments(&server).await;
        assert_eq!(bodies.len(), usize::from(limit > 0));
        if limit > 0 {
            assert_eq!(bodies[0].matches('@').count(), limit as usize);
        }
        assert_eq!(
            writes(&server)
                .await
                .iter()
                .filter(|r| r.url.path().ends_with("/labels"))
                .count(),
            1
        );
        server.reset().await;
        fixture(
            &server,
            &rules.replace(
                &format!("max_cc_users_per_pr={limit}"),
                "max_cc_users_per_pr=0",
            ),
            "second",
            files(),
            201,
        )
        .await;
        let mut ctx = path_sync();
        ctx.head_sha = Some("second".into());
        run_paths(&runtime, &server, &ctx).await.unwrap();
        assert!(comments(&server).await.is_empty());
    }
}

#[tokio::test]
async fn concurrent_pushes_share_the_remaining_slot_and_one_aggregate_snapshot() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    verified(&runtime);
    runtime
        .store
        .restore_notification_ledger(
            7,
            9,
            88,
            &["alice".into()],
            "Complete prior comment verified",
            2,
        )
        .unwrap();
    let server = MockServer::start().await;
    fixture(
        &server,
        &cc_rules(2, &["carol", "bob", "alice"]),
        "latest",
        files(),
        201,
    )
    .await;
    let a = path_sync();
    let mut b = a.clone();
    b.delivery = "push-two".into();
    b.head_sha = Some("different".into());
    let (x, y) = tokio::join!(
        run_paths(&runtime, &server, &a),
        run_paths(&runtime, &server, &b)
    );
    x.unwrap();
    y.unwrap();
    let bodies = comments(&server).await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("cc @bob"));
}

#[tokio::test]
async fn unknown_post_reserves_budget_on_new_head_and_rejects_forged_markers() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    verified(&runtime);
    let server = MockServer::start().await;
    let rules = cc_rules(2, &["alice"]) + "labels=['area/rust']\n";
    fixture(&server, &rules, "first", files(), 503).await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    let body = comments(&server).await.remove(0);
    assert_eq!(notifications(&runtime)[0].state, State::Unknown);
    server.reset().await;
    fixture(
        &server,
        &(cc_rules(2, &["bob", "alice"]) + "labels=['area/rust']\n"),
        "second",
        files(),
        201,
    )
    .await;
    response(&server,"GET","/repos/example/project/issues/3/comments",200,json!([
        {"id":51,"body":body,"user":{"type":"User","login":"bot[bot]"},"performed_via_github_app":{"id":1}},
        {"id":52,"body":body,"user":{"type":"Bot","login":"bot[bot]"},"performed_via_github_app":{"id":2}}
    ])).await;
    let mut ctx = path_sync();
    ctx.head_sha = Some("second".into());
    assert!(run_paths(&runtime, &server, &ctx).await.is_err());
    let bodies = comments(&server).await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("cc @bob"));
    assert!(!bodies[0].contains("@alice"));
    assert!(writes(&server)
        .await
        .iter()
        .any(|r| r.url.path().ends_with("/labels")));
    assert!(notifications(&runtime)
        .iter()
        .any(|o| o.state == State::Unknown && o.recipients == vec!["alice"]));
}

#[tokio::test]
async fn lost_volume_blocks_old_pr_cc_but_labels_continue_and_complete_import_is_additive() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let rules = cc_rules(10, &["alice", "bob"]) + "labels=['area/rust']\n";
    fixture(&server, &rules, "head", files(), 201).await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    assert!(comments(&server).await.is_empty());
    assert!(writes(&server)
        .await
        .iter()
        .any(|r| r.url.path().ends_with("/labels")));
    runtime
        .store
        .restore_notification_ledger(
            7,
            9,
            88,
            &["ALICE".into()],
            "Complete historical records, including deleted comments, verified",
            1,
        )
        .unwrap();
    verified(&runtime); // An empty re-import cannot erase Alice's slot.
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert!(comments(&server).await[0].contains("cc @bob"));
    assert!(!comments(&server).await[0].contains("@alice"));
    assert!(!runtime
        .store
        .notification_ledger_ready(7, 9, 89, 0)
        .unwrap());
    assert!(runtime
        .store
        .notification_ledger_ready(7, 9, 89, crate::github::chrono_now_secs() + 1)
        .unwrap());
}

#[tokio::test]
async fn labels_and_notification_write_results_are_independent_and_invalid_rule_has_no_effects() {
    for label_status in [403, 503] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        verified(&runtime);
        let server = MockServer::start().await;
        fixture(
            &server,
            &(cc_rules(10, &["alice"]) + "labels=['area/rust']\n"),
            "head",
            files(),
            201,
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/repos/example/project/issues/3/labels"))
            .respond_with(
                ResponseTemplate::new(label_status)
                    .set_body_json(json!({"message":"simulated label failure"})),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        let _ = run_paths(&runtime, &server, &path_sync()).await;
        assert_eq!(comments(&server).await.len(), 1);
        assert_eq!(notifications(&runtime)[0].state, State::Succeeded);
        let state = if label_status == 403 {
            State::Failed
        } else {
            State::Unknown
        };
        assert!(runtime
            .store
            .list(Some(state))
            .unwrap()
            .iter()
            .any(|o| o.spec.kind == "ensure_labels"));
    }
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    verified(&runtime);
    let server = MockServer::start().await;
    fixture(
        &server,
        &(cc_rules(10, &["alice"]) + "labels=['missing']\n"),
        "head",
        files(),
        201,
    )
    .await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn deleted_renamed_generated_paths_reuse_complete_diff_and_incomplete_inputs_never_notify() {
    for (records, valid) in [
        (json!([{"filename":"src/old.rs","status":"removed"}]), true),
        (
            json!([{"filename":"other/new.rs","previous_filename":"src/old.rs","status":"renamed"}]),
            true,
        ),
        (
            json!([{"filename":"src/generated/code.rs","status":"added"}]),
            true,
        ),
        (
            json!([{"filename":"other/new.rs","status":"renamed"}]),
            false,
        ),
        (json!([{"filename":"src/old.rs","status":"bogus"}]), false),
        (
            json!([{"filename":"src/old.rs","status":"modified"},{"filename":"src/old.rs","status":"modified"}]),
            false,
        ),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        verified(&runtime);
        let server = MockServer::start().await;
        fixture(&server, &cc_rules(10, &["alice"]), "head", records, 201).await;
        assert_eq!(
            run_paths(&runtime, &server, &path_sync()).await.is_ok(),
            valid
        );
        assert_eq!(comments(&server).await.len(), usize::from(valid));
    }
}

#[tokio::test]
async fn reservation_crash_revalidates_current_head_and_releases_only_unsent_slots() {
    let dir = Dir::new();
    let runtime = Arc::new(Runtime::open(&dir.0).unwrap());
    verified(&runtime);
    let server = MockServer::start().await;
    fixture(&server, &cc_rules(1, &["alice"]), "first", files(), 201).await;
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = reached.clone();
    Mock::given(method("GET"))
        .and(path("/users/alice"))
        .respond_with(move |_: &wiremock::Request| {
            observed.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":42,"type":"User","login":"alice"}))
                .set_delay(std::time::Duration::from_secs(5))
        })
        .with_priority(1)
        .mount(&server)
        .await;
    let owned = runtime.clone();
    let gh = client(&server);
    let task = tokio::spawn(async move {
        owned
            .process(&gh, &cfg(), &RepositoryConfigCache::default(), &path_sync())
            .await
    });
    wait_reached(&reached).await;
    assert_eq!(notifications(&runtime)[0].recipients, vec!["alice"]);
    task.abort();
    let _ = task.await;
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    assert_eq!(notifications(&runtime)[0].state, State::Unknown);
    assert!(!notifications(&runtime)[0].sent);
    server.reset().await;
    fixture(&server, &cc_rules(1, &["bob"]), "second", files(), 201).await;
    let mut ctx = path_sync();
    ctx.head_sha = Some("second".into());
    run_paths(&runtime, &server, &ctx).await.unwrap();
    assert_eq!(comments(&server).await.len(), 1);
    assert!(comments(&server).await[0].contains("cc @bob"));
    assert!(notifications(&runtime)
        .iter()
        .any(|o| o.state == State::Superseded && o.recipients.is_empty()));
}

/// Wait until the mock request reaches the intended crash or mutation boundary.
async fn wait_reached(reached: &std::sync::atomic::AtomicBool) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !reached.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn request_timeout_retains_reservations_and_valid_app_receipt_recovers_without_repost() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    verified(&runtime);
    let server = MockServer::start().await;
    fixture(&server, &cc_rules(1, &["alice"]), "first", files(), 201).await;
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = reached.clone();
    Mock::given(method("POST"))
        .and(path("/repos/example/project/issues/3/comments"))
        .respond_with(move |_: &wiremock::Request| {
            observed.store(true, Ordering::SeqCst);
            ResponseTemplate::new(201)
                .set_body_json(json!({"id":51}))
                .set_delay(std::time::Duration::from_secs(5))
        })
        .with_priority(1)
        .mount(&server)
        .await;
    let result = {
        let ctx = path_sync();
        let work = run_paths(&runtime, &server, &ctx);
        tokio::pin!(work);
        tokio::select! {
            _ = wait_reached(&reached) => {}
            result = &mut work => panic!("path work ended before the delayed send: {result:?}"),
        }
        tokio::time::timeout(std::time::Duration::from_millis(100), work).await
    };
    assert!(result.is_err());
    assert!(reached.load(Ordering::SeqCst));
    let body = comments(&server).await.remove(0);
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    let op = notifications(&runtime).remove(0);
    assert_eq!(op.state, State::Unknown);
    assert!(op.sent);
    assert_eq!(op.recipients, vec!["alice"]);
    server.reset().await;
    fixture(&server, &cc_rules(1, &["alice"]), "first", files(), 201).await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/comments",
        200,
        json!([]),
    )
    .await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    assert!(comments(&server).await.is_empty());
    server.reset().await;
    fixture(&server, &cc_rules(1, &["alice"]), "first", files(), 201).await;
    response(&server,"GET","/repos/example/project/issues/3/comments",200,json!([{
        "id":51,"body":body,"user":{"type":"Bot","login":"bot[bot]"},"performed_via_github_app":{"id":1}
    }])).await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert!(comments(&server).await.is_empty());
    assert_eq!(notifications(&runtime)[0].state, State::Succeeded);
}

#[tokio::test]
async fn remote_success_local_receipt_failure_reconciles_exact_body_and_marker() {
    let dir = Dir::new();
    drop(Store::open(&dir.0).unwrap());
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_cc_receipt BEFORE UPDATE ON operations WHEN NEW.state='succeeded' AND json_extract(NEW.spec,'$.request.path_notification')=1 BEGIN SELECT RAISE(ABORT,'receipt crash'); END;").unwrap();
    drop(db);
    let runtime = Runtime::open(&dir.0).unwrap();
    verified(&runtime);
    let server = MockServer::start().await;
    fixture(&server, &cc_rules(1, &["alice"]), "head", files(), 201).await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    let body = comments(&server).await.remove(0);
    drop(runtime);
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch("DROP TRIGGER fail_cc_receipt;").unwrap();
    drop(db);
    let runtime = Runtime::open(&dir.0).unwrap();
    assert_eq!(notifications(&runtime)[0].state, State::Unknown);
    // A copied marker in another App-authored body is insufficient evidence.
    response(&server,"GET","/repos/example/project/issues/3/comments",200,json!([{
        "id":51,"body":format!("different text\n{}",operation_marker(&notifications(&runtime)[0].spec.key)),
        "user":{"type":"Bot","login":"bot[bot]"},"performed_via_github_app":{"id":1}
    }])).await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    assert_eq!(comments(&server).await.len(), 1);
    server.reset().await;
    fixture(&server, &cc_rules(1, &["alice"]), "head", files(), 201).await;
    response(&server,"GET","/repos/example/project/issues/3/comments",200,json!([{
        "id":51,"body":body,"user":{"type":"Bot","login":"bot[bot]"},"performed_via_github_app":{"id":1}
    }])).await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert!(comments(&server).await.is_empty());
    assert_eq!(notifications(&runtime)[0].recipients, vec!["alice"]);
    assert_eq!(notifications(&runtime)[0].state, State::Succeeded);
}

#[tokio::test]
async fn final_head_check_and_current_policy_revoke_unsent_reservations() {
    for change in ["head", "rule", "budget"] {
        let dir = Dir::new();
        let runtime = Arc::new(Runtime::open(&dir.0).unwrap());
        verified(&runtime);
        let server = MockServer::start().await;
        let rules = cc_rules(1, &["alice"]);
        fixture(&server, &rules, "first", files(), 201).await;
        let cache = Arc::new(RepositoryConfigCache::default());
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = reached.clone();
        Mock::given(method("GET"))
            .and(path("/users/alice"))
            .respond_with(move |_: &wiremock::Request| {
                observed.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":42,"type":"User","login":"alice"}))
                    .set_delay(std::time::Duration::from_millis(300))
            })
            .with_priority(1)
            .mount(&server)
            .await;
        let owned = runtime.clone();
        let task_cache = cache.clone();
        let gh = client(&server);
        let task =
            tokio::spawn(
                async move { owned.process(&gh, &cfg(), &task_cache, &path_sync()).await },
            );
        wait_reached(&reached).await;
        if change == "head" {
            Mock::given(method("GET"))
                .and(path("/repos/example/project/pulls/3"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"head":{"sha":"second"},"base":{"sha":"base"}})),
                )
                .with_priority(1)
                .mount(&server)
                .await;
        } else {
            // Use a fresh source/cache after cancelling the reserved worker;
            // recovery must obey the narrowed policy even before a first send.
            task.abort();
            let _ = task.await;
            drop(runtime);
            drop(cache);
            let runtime = Runtime::open(&dir.0).unwrap();
            server.reset().await;
            let replacement = if change == "rule" {
                String::new()
            } else {
                cc_rules(0, &["alice"])
            };
            fixture(&server, &replacement, "first", files(), 201).await;
            run_paths(&runtime, &server, &path_sync()).await.unwrap();
            assert!(comments(&server).await.is_empty());
            assert!(notifications(&runtime)[0].recipients.is_empty());
            continue;
        }
        assert!(task.await.unwrap().is_err());
        assert!(comments(&server).await.is_empty());
        assert_eq!(notifications(&runtime)[0].state, State::Superseded);
        assert!(notifications(&runtime)[0].recipients.is_empty());
    }
}

#[tokio::test]
async fn nonexistent_users_bots_and_rejected_posts_never_consume_sent_slots() {
    for failure in ["missing", "bot", "permission"] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        verified(&runtime);
        let server = MockServer::start().await;
        fixture(
            &server,
            &cc_rules(1, &["alice"]),
            "first",
            files(),
            if failure == "permission" { 403 } else { 201 },
        )
        .await;
        if failure != "permission" {
            Mock::given(method("GET"))
                .and(path("/users/alice"))
                .respond_with(
                    ResponseTemplate::new(if failure == "missing" { 404 } else { 200 })
                        .set_body_json(if failure == "missing" {
                            json!({"message":"Not Found"})
                        } else {
                            json!({"id":42,"login":"alice","type":"Bot"})
                        }),
                )
                .with_priority(1)
                .mount(&server)
                .await;
        }
        assert_eq!(
            run_paths(&runtime, &server, &path_sync()).await.is_err(),
            failure == "permission"
        );
        assert!(notifications(&runtime)[0].recipients.is_empty());
        assert_eq!(
            comments(&server).await.len(),
            usize::from(failure == "permission")
        );
    }
}

#[test]
fn notification_reservations_sort_suppress_and_fence_unrelated_unsent_refusals() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let first = store
        .claim_notification(
            &spec("first", "comment"),
            &["carol".into(), "Bob".into(), "alice".into(), "ALICE".into()],
            2,
            1,
        )
        .unwrap()
        .unwrap();
    assert_eq!(first.operation.recipients, vec!["alice", "bob"]);
    assert_eq!(first.operation.spec.request["suppressed"], json!(["carol"]));
    store.mark_sent(&first, 1).unwrap();
    store
        .finish(&first, State::Unknown, None, "timeout", 2)
        .unwrap();
    // A failed preflight on another code path must not release sent unknown slots.
    store.fail_unsent("first", "policy refused", 3).unwrap();
    assert_eq!(store.get("first").unwrap().unwrap().recipients.len(), 2);
    let second = store
        .claim_notification(
            &spec("second", "comment"),
            &["alice".into(), "carol".into()],
            1,
            4,
        )
        .unwrap()
        .unwrap();
    assert!(second.operation.recipients.is_empty());
    assert_eq!(
        second.operation.spec.request["suppressed"],
        json!(["carol"])
    );
}

#[test]
fn restored_historical_recipients_cannot_be_released_by_old_unsent_plan_recovery() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let claim = store
        .claim_notification(&spec("old-plan", "comment"), &["alice".into()], 1, 1)
        .unwrap()
        .unwrap();
    store
        .finish(&claim, State::Unknown, None, "crash before send", 2)
        .unwrap();
    store
        .restore_notification_ledger(
            7,
            9,
            88,
            &["alice".into()],
            "A separate historical Alice mention is confirmed in the complete backup",
            3,
        )
        .unwrap();
    let op = store.get("old-plan").unwrap().unwrap();
    store
        .resolve_unknown(
            &op,
            "not_sent",
            None,
            "old plan did not send; imported history is independent",
            4,
        )
        .unwrap();
    let retry = store
        .claim_notification(&op.spec, &["alice".into(), "bob".into()], 1, 5)
        .unwrap()
        .unwrap();
    assert!(retry.operation.recipients.is_empty());
    assert_eq!(retry.operation.spec.request["suppressed"], json!(["bob"]));
}
