//! PR #26 regressions at the production persistence and HTTP boundaries.
use super::*;
use wiremock::matchers::query_param;

async fn path_fixture(server: &MockServer, rules: &str, inventory: Value, status: u16) {
    policy(server, rules).await;
    path_pr_meta(server, 1).await;
    response(
        server,
        "GET",
        "/repos/example/project/pulls/3/files",
        200,
        json!([{"filename":"src/lib.rs","status":"modified"}]),
    )
    .await;
    response(
        server,
        "GET",
        "/repos/example/project/labels",
        200,
        inventory,
    )
    .await;
    response(
        server,
        "POST",
        "/repos/example/project/issues/3/labels",
        status,
        json!([]),
    )
    .await;
}

pub(super) async fn run_paths(
    runtime: &Runtime,
    server: &MockServer,
    ctx: &EventContext,
) -> Result<()> {
    runtime
        .process(
            &client(server),
            &cfg(),
            &RepositoryConfigCache::default(),
            ctx,
        )
        .await
}

fn path_parent(runtime: &Runtime) -> Operation {
    runtime
        .store
        .list(None)
        .unwrap()
        .into_iter()
        .find(|op| op.spec.kind == "path")
        .unwrap()
}

#[tokio::test]
async fn unicode_path_labels_use_the_repository_spelling_and_coalesce() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let rules = path_rules().replace("['area/rust']", "['Ärea/Rust','ärea/rust','missing']");
    path_fixture(&server, &rules, json!([{"name":"ärea/RUST"}]), 200).await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    let requests = writes(&server).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap()["labels"],
        json!(["ärea/RUST"])
    );
}

#[tokio::test]
async fn path_file_statuses_accept_the_full_enum_and_reject_unknown_or_missing_values() {
    for status in [
        "added",
        "removed",
        "modified",
        "renamed",
        "copied",
        "changed",
        "unchanged",
        "bogus",
        "",
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
        Mock::given(method("GET")).and(path("/repos/example/project/pulls/3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"filename":"src/lib.rs","status": if status.is_empty() {Value::Null} else {json!(status)},"previous_filename":"old.rs"}
            ]))).with_priority(1).mount(&server).await;
        let valid = !matches!(status, "bogus" | "");
        assert_eq!(
            run_paths(&runtime, &server, &path_sync()).await.is_ok(),
            valid,
            "{status}"
        );
        assert_eq!(writes(&server).await.len(), usize::from(valid), "{status}");
    }
}

#[tokio::test]
async fn path_recovery_confirms_unicode_case_changes_without_resending() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let rules = path_rules().replace("area/rust", "Ärea/rust");
    path_fixture(&server, &rules, json!([{"name":"Ärea/rust"}]), 503).await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    server.reset().await;
    policy(&server, &rules).await;
    path_pr_meta(&server, 1).await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/labels",
        200,
        json!([{"name":"ärea/rust"}]),
    )
    .await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert!(writes(&server).await.is_empty());
    assert_eq!(path_parent(&runtime).state, State::Succeeded);
}

#[tokio::test]
async fn disabled_path_events_remain_empty_after_restart_and_policy_repair() {
    for initial in [
        "",
        "[invalid TOML",
        "[path_triggers]\nevents=['issues.opened']",
        "[path_triggers]\nevents=['pull_request.opened']",
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        policy(&server, initial).await;
        run_paths(&runtime, &server, &path_sync()).await.unwrap();
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().contains("/pulls/")));
        drop(runtime);
        let runtime = Runtime::open(&dir.0).unwrap();
        server.reset().await;
        path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
        let mut redelivery = path_sync();
        redelivery.delivery = "redelivery".into();
        run_paths(&runtime, &server, &redelivery).await.unwrap();
        assert!(writes(&server).await.is_empty(), "{initial}");
        // A later supported source event can subscribe normally.
        redelivery.source_time = "2026-10-04T01:00:00Z".into();
        run_paths(&runtime, &server, &redelivery).await.unwrap();
        assert_eq!(writes(&server).await.len(), 1);
    }
}

#[tokio::test]
async fn path_inventory_outage_retries_and_unclaimed_plan_revocation_is_permanent() {
    for revoke in [false, true] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        path_fixture(&server, path_rules(), json!([]), 200).await;
        Mock::given(method("GET"))
            .and(path("/repos/example/project/labels"))
            .respond_with(ResponseTemplate::new(403))
            .with_priority(1)
            .mount(&server)
            .await;
        assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
        assert!(runtime.store.list(None).unwrap().is_empty());
        drop(runtime);
        let runtime = Runtime::open(&dir.0).unwrap();
        server.reset().await;
        if revoke {
            // No PR endpoint: revocation must persist before further API reads.
            policy(&server, "").await;
            run_paths(&runtime, &server, &path_sync()).await.unwrap();
            assert_eq!(path_parent(&runtime).state, State::Superseded);
            server.reset().await;
        }
        path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
        run_paths(&runtime, &server, &path_sync()).await.unwrap();
        assert_eq!(writes(&server).await.len(), usize::from(!revoke));
    }
}

#[tokio::test]
async fn path_subscription_revocation_survives_an_initial_diff_outage() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    policy(&server, path_rules()).await;
    path_pr_meta(&server, 1).await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3/files",
        403,
        json!({}),
    )
    .await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    assert!(runtime.store.path_plans(&path_sync()).unwrap().is_empty());
    server.reset().await;
    policy(&server, "").await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    server.reset().await;
    path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn an_excluded_event_cannot_resume_another_event_type() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    let rules = path_rules().replace("pull_request.synchronize", "pull_request.opened");
    path_fixture(&server, &rules, json!([{"name":"area/rust"}]), 200).await;
    Mock::given(method("GET"))
        .and(path("/repos/example/project/labels"))
        .respond_with(ResponseTemplate::new(403))
        .with_priority(1)
        .mount(&server)
        .await;
    let ctx = opened(true);
    assert!(run_paths(&runtime, &server, &ctx).await.is_err());
    server.reset().await;
    policy(&server, &rules).await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| !request.url.path().contains("/pulls/")));
    path_fixture(&server, &rules, json!([{"name":"area/rust"}]), 200).await;
    run_paths(&runtime, &server, &ctx).await.unwrap();
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn path_revocation_survives_failed_reconciliation_and_restored_policy() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 503).await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    let parent = path_parent(&runtime);
    assert_eq!(parent.state, State::Unknown);
    server.reset().await;
    policy(&server, "").await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/labels",
        403,
        json!({}),
    )
    .await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    assert_eq!(path_parent(&runtime).state, State::Superseded);
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    server.reset().await;
    path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert!(writes(&server).await.is_empty());
    assert!(runtime
        .store
        .children(&parent.spec.key)
        .unwrap()
        .iter()
        .all(|op| op.state == State::Superseded));
}

#[tokio::test]
async fn path_label_retries_cannot_recreate_deleted_repository_labels() {
    for keep_one in [false, true] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let rules = path_rules().replace("['area/rust']", "['area/rust','deleted']");
        path_fixture(
            &server,
            &rules,
            json!([{"name":"area/rust"},{"name":"deleted"}]),
            503,
        )
        .await;
        assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
        let parent = path_parent(&runtime);
        let child = runtime.store.children(&parent.spec.key).unwrap().remove(0);
        server.reset().await;
        path_fixture(
            &server,
            &rules,
            if keep_one {
                json!([{"name":"area/rust"}])
            } else {
                json!([])
            },
            200,
        )
        .await;
        response(
            &server,
            "GET",
            "/repos/example/project/issues/3/labels",
            200,
            json!([]),
        )
        .await;
        // An old reconciliation time makes the production backoff already due.
        reconcile(&runtime.store, &client(&server), 1, &child, 0)
            .await
            .unwrap();
        run_paths(&runtime, &server, &path_sync()).await.unwrap();
        let requests = writes(&server).await;
        assert_eq!(requests.len(), usize::from(keep_one));
        if keep_one {
            assert_eq!(
                serde_json::from_slice::<Value>(&requests[0].body).unwrap()["labels"],
                json!(["area/rust"])
            );
        }
        assert_eq!(path_parent(&runtime).state, State::Succeeded);
        assert!(!runtime.store.unresolved(&path_sync().delivery).unwrap());
    }
}

#[tokio::test]
async fn a_new_snapshot_retires_old_unknown_writes_before_matching_the_current_diff() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    let server = MockServer::start().await;
    path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 503).await;
    assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
    let old = path_parent(&runtime);
    server.reset().await;
    path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
    response(
        &server,
        "GET",
        "/repos/example/project/issues/3/labels",
        200,
        json!([]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/example/project/pulls/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"head":{"sha":"latest"},"base":{"sha":"base"},"changed_files":1}),
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    run_paths(&runtime, &server, &path_sync()).await.unwrap();
    assert_eq!(
        runtime.store.get(&old.spec.key).unwrap().unwrap().state,
        State::Superseded
    );
    assert!(runtime
        .store
        .children(&old.spec.key)
        .unwrap()
        .iter()
        .all(|op| op.state == State::Superseded));
    assert_eq!(writes(&server).await.len(), 1);
    assert!(!runtime.store.unresolved(&path_sync().delivery).unwrap());
}

#[tokio::test]
async fn complete_path_diff_pagination_and_api_limit_are_verified_before_writes() {
    for (count, second_status) in [(101, 200), (101, 403), (102, 200), (3001, 200)] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
        Mock::given(method("GET"))
            .and(path("/repos/example/project/pulls/3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"head":{"sha":"new-head"},"base":{"sha":"base"},"changed_files":count}),
            ))
            .with_priority(1)
            .mount(&server)
            .await;
        let files: Vec<_> = (0..100)
            .map(|i| json!({"filename":format!("other/{i}"),"status":"modified"}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/repos/example/project/pulls/3/files"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(files)
                    .insert_header(
                        "Link",
                        format!(
                            "<{}/repos/example/project/pulls/3/files?page=2>; rel=\"next\"",
                            server.uri()
                        ),
                    ),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/example/project/pulls/3/files"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(second_status)
                    .set_body_json(json!([{"filename":"src/lib.rs","status":"modified"}])),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        let valid = count == 101 && second_status == 200;
        assert_eq!(
            run_paths(&runtime, &server, &path_sync()).await.is_ok(),
            valid
        );
        assert_eq!(writes(&server).await.len(), usize::from(valid));
        if count > 3000 {
            assert!(server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| !request.url.path().ends_with("/files")));
        }
    }
}

#[tokio::test]
async fn path_control_label_rules_are_refused_without_blocking_valid_rules() {
    for control in ["queued", "testing", "codeql"] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        let rules = format!(
            "{}\n[[path_triggers.rules]]\nid='control'\ninclude=['src/**']\nlabels=['{control}']",
            path_rules()
        );
        let mut config = cfg();
        config.label_merge_queue_queued = "queued".into();
        config.label_merge_queue_testing = "testing".into();
        config.codeql_label = "codeql".into();
        path_fixture(
            &server,
            &rules,
            json!([{"name":"area/rust"},{"name":control}]),
            200,
        )
        .await;
        runtime
            .process(
                &client(&server),
                &config,
                &RepositoryConfigCache::default(),
                &path_sync(),
            )
            .await
            .unwrap();
        let requests = writes(&server).await;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).unwrap()["labels"],
            json!(["area/rust"])
        );
    }
}

#[tokio::test]
async fn path_snapshot_changes_are_bounded_and_the_last_write_fence_is_retryable() {
    for continuous in [false, true] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
        let reads = Arc::new(AtomicU64::new(0));
        let counter = reads.clone();
        Mock::given(method("GET")).and(path("/repos/example/project/pulls/3"))
            .respond_with(move |_: &wiremock::Request| {
                let call = counter.fetch_add(1, Ordering::SeqCst);
                let sha = if continuous {call} else {u64::from(call >= 2)};
                ResponseTemplate::new(200).set_body_json(json!({"head":{"sha":format!("h{sha}")},"base":{"sha":"base"},"changed_files":1}))
            }).with_priority(1).mount(&server).await;
        assert!(run_paths(&runtime, &server, &path_sync()).await.is_err());
        assert!(writes(&server).await.is_empty());
        if continuous {
            assert_eq!(
                reads.load(Ordering::SeqCst),
                6,
                "initial try plus two immediate retries"
            );
            assert!(runtime.store.list(None).unwrap().is_empty());
        } else {
            assert_eq!(path_parent(&runtime).state, State::Superseded);
            run_paths(&runtime, &server, &path_sync()).await.unwrap();
            assert_eq!(writes(&server).await.len(), 1);
        }
    }
}

#[tokio::test]
async fn malformed_complete_diff_records_and_snapshots_never_produce_partial_labels() {
    let server = MockServer::start().await;
    let gh = client(&server);
    for files in [
        json!([{"filename":"src/lib.rs","status":"modified"},{"filename":"src/lib.rs","status":"modified"}]),
        json!([{"status":"modified"}]),
        json!([{"filename":"src/lib.rs","status":"renamed","previous_filename":""}]),
        json!([{"filename":"src/lib.rs","status":"renamed","previous_filename":42}]),
    ] {
        server.reset().await;
        let snapshot = json!({"head":{"sha":"h"},"base":{"sha":"b"},"changed_files":files.as_array().unwrap().len()});
        response(
            &server,
            "GET",
            "/repos/example/project/pulls/3",
            200,
            snapshot.clone(),
        )
        .await;
        response(
            &server,
            "GET",
            "/repos/example/project/pulls/3/files",
            200,
            files,
        )
        .await;
        assert!(gh
            .list_pr_files_complete("example/project", 3, &snapshot)
            .await
            .is_err());
    }
    for snapshot in [
        json!({}),
        json!({"head":{"sha":""},"base":{"sha":"b"},"changed_files":0}),
    ] {
        assert!(gh
            .list_pr_files_complete("example/project", 3, &snapshot)
            .await
            .is_err());
    }
    server.reset().await;
    let snapshot = json!({"head":{"sha":"h"},"base":{"sha":"b"},"changed_files":3000});
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3",
        200,
        snapshot.clone(),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example/project/pulls/3/files",
        200,
        json!((0..3000)
            .map(|i| json!({"filename":format!("f{i}"),"status":"modified"}))
            .collect::<Vec<_>>()),
    )
    .await;
    assert_eq!(
        gh.list_pr_files_complete("example/project", 3, &snapshot)
            .await
            .unwrap()
            .len(),
        3000
    );
}

/// A push invalidating policy while label inventory is read must veto the
/// imminent write. Outages stay retryable; disabled rules become superseded.
#[tokio::test]
async fn audit_path_labels_recheck_policy_after_inventory() {
    for outage in [false, true] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        path_fixture(&server, path_rules(), json!([{"name":"area/rust"}]), 200).await;
        let cache = Arc::new(RepositoryConfigCache::default());
        let invalidator = cache.clone();
        let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let changed_read = changed.clone();
        Mock::given(method("GET"))
            .and(path("/repos/example/project/labels"))
            .respond_with(move |_: &wiremock::Request| {
                changed.store(true, Ordering::SeqCst);
                invalidator.invalidate(crate::config::cache::RepositoryKey {
                    installation_id: 7,
                    repository_id: 9,
                });
                ResponseTemplate::new(200).set_body_json(json!([{"name":"area/rust"}]))
            })
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/repos/example/project/contents/.github/xero-bot.toml"))
            .respond_with(move |_: &wiremock::Request| {
                let changed = changed_read.load(Ordering::SeqCst);
                if changed && outage { return ResponseTemplate::new(503); }
                let text = if changed { "[command_triggers]\nlabel={mode='disabled'}" } else { path_rules() };
                ResponseTemplate::new(200).set_body_json(json!({"sha":"blob","type":"file","encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode(text)}))
            }).with_priority(1).mount(&server).await;
        let result = runtime
            .process(&client(&server), &cfg(), &cache, &path_sync())
            .await;
        assert_eq!(result.is_err(), outage);
        assert!(
            writes(&server).await.is_empty(),
            "revoked or unverified labels were posted"
        );
        if !outage {
            assert_eq!(path_parent(&runtime).state, State::Superseded);
        }
    }
}
