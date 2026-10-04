//! Installation-scoped lifetime accounting, CLI ownership and v3 migration.
use super::*;
use rusqlite::{params, Connection};

/// Reproduce the pre-scope schema, retaining the same operation and inbox tables.
fn legacy_database(dir: &Dir) -> Connection {
    drop(Store::open(&dir.0).unwrap());
    let db = Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    db.execute_batch(
        "DROP TABLE recipients;
        CREATE TABLE recipients(repository INTEGER NOT NULL,pr INTEGER NOT NULL,login TEXT NOT NULL,
            operation TEXT NOT NULL REFERENCES operations(key),committed INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(repository,pr,login));
        DROP TABLE path_notification_ledgers;
        CREATE TABLE path_notification_ledgers(repository INTEGER NOT NULL,pr INTEGER NOT NULL,
            verified_at INTEGER NOT NULL,evidence TEXT NOT NULL,PRIMARY KEY(repository,pr));
        DROP TABLE path_notification_migration_blocks;
        UPDATE path_notification_epoch SET initialized_at=1;
        UPDATE trigger_meta SET version=3;",
    )
    .unwrap();
    db
}

/// Save the original marker-bearing request exactly as a v3 server would.
fn legacy_notification(db: &Connection, installation: i64) -> OperationSpec {
    let key = json!(["path-cc", 9, 88, "first", "base"]).to_string();
    let body = format!("cc @alice\n\n{}", operation_marker(&key));
    let mut ctx = path_sync();
    ctx.installation_id = installation;
    ctx.head_sha = Some("first".into());
    let op = OperationSpec {
        key: key.clone(),
        parent: None,
        kind: "comment".into(),
        context: ctx,
        config_sha: Some("config".into()),
        request: json!({"method":"POST","route":"/repos/example/project/issues/3/comments","path_notification":true,"body":{"body":body}}),
    };
    db.execute("INSERT INTO operations(key,delivery,spec,state,sent,attempts,updated_at) VALUES(?1,'sync-1',?2,'unknown',1,1,1)",params![key,serde_json::to_string(&op).unwrap()]).unwrap();
    db.execute("INSERT INTO recipients VALUES(9,88,'alice',?1,0)", [&key])
        .unwrap();
    op
}

/// Freeze a pre-migration match whose head and marker must survive migration.
fn legacy_plan(db: &Connection) {
    let key = json!([
        "path-plan",
        9,
        88,
        "pull_request.synchronize",
        "first",
        "base"
    ])
    .to_string();
    let plan = json!([{"rules":[{"id":"rust","include":["src/**"],"exclude":[],"labels":[],"cc":["alice"]}],
        "labels":[],"event":"pull_request.synchronize","head_sha":"first","base_sha":"base","config_sha":"config",
        "matches":[{"id":"rust","labels":[],"paths":["src/lib.rs"]}]}]);
    db.execute(
        "INSERT INTO opened_plans VALUES(?1,?2)",
        params![key, plan.to_string()],
    )
    .unwrap();
}

/// Successful and uncertain slots in one installation do not consume another
/// installation's budget, even when concurrent claims target the same PR/login.
#[test]
fn concurrent_installations_reserve_independent_budgets_and_survive_restart() {
    let dir = Dir::new();
    let store = Arc::new(Store::open(&dir.0).unwrap());
    let barrier = Arc::new(Barrier::new(4));
    let mut workers = Vec::new();
    for n in 0..4 {
        let store = store.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let mut op = spec(&format!("scope-{n}"), "comment");
            op.context.installation_id = 7 + n % 2;
            barrier.wait();
            let claim = store
                .claim_notification(&op, &["alice".into(), "bob".into()], 1, 1)
                .unwrap()
                .unwrap();
            let count = claim.operation.recipients.len();
            if count > 0 {
                store.mark_sent(&claim, 1).unwrap();
            }
            let state = if op.context.installation_id == 7 && count > 0 {
                State::Unknown
            } else {
                State::Succeeded
            };
            store
                .finish(&claim, state, None, "test receipt", 2)
                .unwrap();
            (op.context.installation_id, count)
        }));
    }
    let counts = workers
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<Vec<_>>();
    for installation in [7, 8] {
        assert_eq!(
            counts
                .iter()
                .filter(|(i, _)| *i == installation)
                .map(|(_, n)| n)
                .sum::<usize>(),
            1
        );
    }
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    for installation in [7, 8] {
        let mut op = spec(&format!("later-{installation}"), "comment");
        op.context.installation_id = installation;
        assert!(store
            .claim_notification(&op, &["bob".into()], 1, 3)
            .unwrap()
            .unwrap()
            .operation
            .recipients
            .is_empty());
    }
    assert_ne!(
        recipient_key(7, 9, 88, "ALICE").unwrap(),
        recipient_key(8, 9, 88, "alice").unwrap()
    );
    for installation in [0, -1] {
        assert!(recipient_key(installation, 9, 88, "alice").is_err());
        let mut op = spec("invalid-scope", "comment");
        op.context.installation_id = installation;
        assert!(store.claim_notification(&op, &[], 0, 4).is_err());
    }
}

/// A reused operation key cannot borrow another installation's reservations.
#[test]
fn notification_claim_rejects_scope_mismatch_before_changing_the_lease() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let op = spec("same-key", "comment");
    let claim = store
        .claim_notification(&op, &["alice".into()], 1, 1)
        .unwrap()
        .unwrap();
    store
        .finish(&claim, State::Unknown, None, "not sent", 2)
        .unwrap();
    let old = store.get(&op.key).unwrap().unwrap();
    store
        .resolve_unknown(&old, "not_sent", None, "before send", 3)
        .unwrap();
    let mut wrong = op.clone();
    wrong.context.installation_id += 1;
    assert!(store
        .claim_notification(&wrong, &["bob".into()], 1, 4)
        .is_err());
    let current = store.get(&op.key).unwrap().unwrap();
    assert_eq!(current.state, State::Pending);
    assert_eq!(current.attempts, 1);
    assert_eq!(current.spec.context.installation_id, 7);
}

/// An explicit restoration attests only the specified installation's old PR.
#[test]
fn restored_ledgers_and_imported_recipients_are_scoped_to_installation() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    store
        .restore_notification_ledger(7, 9, 88, &["alice".into()], "Complete install 7 history", 1)
        .unwrap();
    assert!(store.notification_ledger_ready(7, 9, 88, 0).unwrap());
    assert!(!store.notification_ledger_ready(8, 9, 88, 0).unwrap());
    assert!(!store.notification_ledger_ready(7, 10, 88, 0).unwrap());
    assert!(!store.notification_ledger_ready(7, 9, 89, 0).unwrap());
    assert!(store
        .restore_notification_ledger(0, 9, 88, &[], "no scope", 2)
        .is_err());
    let mut other = spec("other-installation", "comment");
    other.context.installation_id = 8;
    let claim = store
        .claim_notification(&other, &["alice".into()], 1, 2)
        .unwrap()
        .unwrap();
    assert_eq!(claim.operation.recipients, vec!["alice"]);
    store
        .finish(&claim, State::Succeeded, None, "accepted", 3)
        .unwrap();
    store
        .restore_notification_ledger(7, 9, 88, &[], "still complete", 4)
        .unwrap();
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    let old = store
        .claim_notification(&spec("old-install", "comment"), &["bob".into()], 1, 5)
        .unwrap()
        .unwrap();
    assert!(old.operation.recipients.is_empty());
    assert!(!store.notification_ledger_ready(8, 9, 88, 0).unwrap());
}

/// The full path planner must not share snapshot/subscription keys or markers
/// across installations, and an unknown send in one cannot block the other.
#[tokio::test]
async fn same_snapshot_notifications_and_unknown_recovery_are_installation_scoped() {
    let dir = Dir::new();
    let runtime = Runtime::open(&dir.0).unwrap();
    verified(&runtime);
    runtime
        .store
        .restore_notification_ledger(8, 9, 88, &[], "Complete install 8 history", 1)
        .unwrap();
    let server = MockServer::start().await;
    fixture(&server, &cc_rules(1, &["alice"]), "head", files(), 503).await;
    let ctx = path_sync();
    assert!(run_paths(&runtime, &server, &ctx).await.is_err());
    let first = notifications(&runtime).remove(0);
    assert_eq!(first.state, State::Unknown);
    server.reset().await;
    fixture(&server, &cc_rules(1, &["alice"]), "head", files(), 201).await;
    let mut other = ctx.clone();
    other.installation_id = 8;
    other.delivery = "install-8".into();
    run_paths(&runtime, &server, &other).await.unwrap();
    run_paths(&runtime, &server, &other).await.unwrap();
    assert_eq!(comments(&server).await.len(), 1);
    let ops = notifications(&runtime);
    assert_eq!(ops.len(), 2);
    assert!(ops.iter().all(|o| o.recipients == vec!["alice"]));
    assert_ne!(
        operation_marker(&ops[0].spec.key),
        operation_marker(&ops[1].spec.key)
    );
    assert!(ops
        .iter()
        .any(|o| o.spec.context.installation_id == 7 && o.state == State::Unknown));
    assert!(ops
        .iter()
        .any(|o| o.spec.context.installation_id == 8 && o.state == State::Succeeded));
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method != "GET" || !r.url.path().ends_with("/comments")));
    assert_eq!(runtime.store.path_plans(&ctx).unwrap().len(), 1);
    assert_eq!(runtime.store.path_plans(&other).unwrap().len(), 1);
}

/// Migration keeps the old key/body so an accepted legacy comment is reconciled
/// rather than sent again under a new installation-bearing marker.
#[tokio::test]
async fn legacy_unknown_recipient_and_marker_migrate_to_the_recorded_installation() {
    let dir = Dir::new();
    let db = legacy_database(&dir);
    let old = legacy_notification(&db, 7);
    legacy_plan(&db);
    // A later installation's unrelated head must neither inherit this plan
    // nor make its recorded owner ambiguous.
    let mut later = path_sync();
    later.installation_id = 8;
    db.execute(
        "INSERT INTO inbox(delivery,context,updated_at) VALUES('later',?1,1)",
        [serde_json::to_string(&later).unwrap()],
    )
    .unwrap();
    drop(db);
    let runtime = Runtime::open(&dir.0).unwrap();
    let migrated = runtime.store.get(&old.key).unwrap().unwrap();
    assert_eq!(migrated.recipients, vec!["alice"]);
    assert_eq!(migrated.spec.request, old.request);
    let ctx = path_sync();
    let mut other = ctx.clone();
    other.installation_id = 8;
    assert_eq!(
        runtime.store.path_operation_key(&ctx, &old.key).unwrap(),
        old.key
    );
    assert_ne!(
        runtime.store.path_operation_key(&other, &old.key).unwrap(),
        old.key
    );
    assert_eq!(runtime.store.path_plans(&ctx).unwrap().len(), 1);
    assert!(runtime.store.path_plans(&other).unwrap().is_empty());
    let server = MockServer::start().await;
    fixture(&server, &cc_rules(1, &["alice"]), "second", files(), 201).await;
    response(&server,"GET","/repos/example/project/issues/3/comments",200,json!([{
        "id":123,"body":old.request["body"]["body"],"user":{"type":"Bot","login":"bot[bot]"},"performed_via_github_app":{"id":1}
    }])).await;
    run_paths(&runtime, &server, &ctx).await.unwrap();
    assert!(comments(&server).await.is_empty());
    assert_eq!(
        runtime.store.get(&old.key).unwrap().unwrap().state,
        State::Succeeded
    );
    run_paths(&runtime, &server, &other).await.unwrap();
    assert_eq!(comments(&server).await.len(), 1);
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    run_paths(&runtime, &server, &ctx).await.unwrap();
    assert_eq!(comments(&server).await.len(), 1);
}

/// A retained event for another snapshot cannot establish ownership of an
/// orphaned plan, even if it is the only remaining event for that PR.
#[test]
fn unrelated_legacy_context_does_not_assign_a_plan_to_another_installation() {
    for inbox in [true, false] {
        let dir = Dir::new();
        let db = legacy_database(&dir);
        legacy_plan(&db);
        if inbox {
            let mut unrelated = path_sync();
            unrelated.installation_id = 8;
            db.execute(
                "INSERT INTO inbox(delivery,context,updated_at) VALUES('later',?1,1)",
                [serde_json::to_string(&unrelated).unwrap()],
            )
            .unwrap();
        } else {
            // Even a manual command that observed the same head is unrelated.
            let mut manual = spec("manual-action", "comment");
            manual.context.head_sha = Some("first".into());
            manual.context.base_sha = Some("base".into());
            db.execute(
                "INSERT INTO operations(key,delivery,spec,updated_at) VALUES(?1,'manual',?2,1)",
                params![manual.key, serde_json::to_string(&manual).unwrap()],
            )
            .unwrap();
        }
        drop(db);
        let store = Store::open(&dir.0).unwrap();
        for installation in [7, 8] {
            let mut ctx = path_sync();
            ctx.installation_id = installation;
            assert!(store.path_plans(&ctx).unwrap().is_empty());
            assert!(!store
                .notification_ledger_ready(installation, 9, 88, i64::MAX)
                .unwrap());
        }
    }
}

/// Source subscriptions use the original event time/head/base, not a later
/// operation's refreshed snapshot or another event on the same PR.
#[test]
fn legacy_subscription_migration_matches_the_complete_source_event() {
    for matching in [true, false] {
        let dir = Dir::new();
        let db = legacy_database(&dir);
        let mut ctx = path_sync();
        let key = json!([
            "path-event",
            9,
            88,
            "pull_request.synchronize",
            ctx.source_time,
            ctx.head_sha,
            ctx.base_sha
        ])
        .to_string();
        let original = json!({"rules":[],"config_sha":"old-config"});
        db.execute(
            "INSERT INTO opened_plans VALUES(?1,?2)",
            params![key, original.to_string()],
        )
        .unwrap();
        if !matching {
            ctx.source_time = "2026-10-04T01:00:00Z".into();
        }
        db.execute(
            "INSERT INTO inbox(delivery,context,updated_at) VALUES('source',?1,1)",
            [serde_json::to_string(&ctx).unwrap()],
        )
        .unwrap();
        drop(db);
        let store = Store::open(&dir.0).unwrap();
        let mut scoped: Vec<Value> = serde_json::from_str(&key).unwrap();
        scoped.push(json!(ctx.installation_id));
        let proposed = json!({"rules":[{"id":"new-rule"}],"config_sha":"new-config"});
        let subscription = store
            .path_subscription(&serde_json::to_string(&scoped).unwrap(), &proposed)
            .unwrap();
        assert_eq!(subscription, if matching { original } else { proposed });
        assert_eq!(
            store.notification_ledger_ready(7, 9, 88, i64::MAX).unwrap(),
            matching
        );
    }
}

/// Legacy offline imports have no installation to infer. Keep their evidence
/// and block every unverified scope instead of assuming an empty budget.
#[test]
fn unowned_legacy_records_block_cc_until_scoped_restoration() {
    let dir = Dir::new();
    let db = legacy_database(&dir);
    legacy_notification(&db, 0);
    db.execute(
        "INSERT INTO path_notification_ledgers VALUES(9,88,1,'legacy completeness evidence')",
        [],
    )
    .unwrap();
    drop(db);
    let store = Store::open(&dir.0).unwrap();
    let future = crate::github::chrono_now_secs() + 100;
    for installation in [7, 8] {
        assert!(!store
            .notification_ledger_ready(installation, 9, 88, future)
            .unwrap());
    }
    store
        .restore_notification_ledger(
            7,
            9,
            88,
            &["alice".into()],
            "Full legacy evidence assigned to install 7",
            1,
        )
        .unwrap();
    assert!(store.notification_ledger_ready(7, 9, 88, future).unwrap());
    assert!(!store.notification_ledger_ready(8, 9, 88, future).unwrap());
    assert!(store
        .claim_notification(&spec("after-restoration", "comment"), &["bob".into()], 1, 2)
        .unwrap()
        .unwrap()
        .operation
        .recipients
        .is_empty());
    drop(store);
    let db = Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM legacy_recipients", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT evidence FROM legacy_path_notification_ledgers",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "legacy completeness evidence"
    );
    assert_eq!(
        db.query_row("SELECT version FROM trigger_meta", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        4
    );
}

/// A failed plan migration rolls back the schema, recipient copy and version;
/// restarting after the fault is removed can safely retry the entire migration.
#[test]
fn notification_scope_migration_is_atomic() {
    let dir = Dir::new();
    let db = legacy_database(&dir);
    legacy_notification(&db, 7);
    legacy_plan(&db);
    db.execute_batch("CREATE TRIGGER fail_scope BEFORE INSERT ON opened_plans WHEN json_array_length(NEW.key)=7 AND json_extract(NEW.key,'$[0]')='path-plan' BEGIN SELECT RAISE(ABORT,'scope migration fault'); END;").unwrap();
    drop(db);
    assert!(Store::open(&dir.0).is_err());
    let db = Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT version FROM trigger_meta", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM pragma_table_info('recipients') WHERE name='installation'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_scope;").unwrap();
    drop(db);
    let store = Store::open(&dir.0).unwrap();
    assert_eq!(store.path_plans(&path_sync()).unwrap().len(), 1);
    assert_eq!(
        store
            .get(&json!(["path-cc", 9, 88, "first", "base"]).to_string())
            .unwrap()
            .unwrap()
            .recipients,
        vec!["alice"]
    );
}
