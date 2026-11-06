use super::*;
use crate::{
    commands::{parse_commands, Command},
    trigger_state::Store,
};
use serde_json::json;
fn origin() -> Origin {
    Origin {
        provider: "github".into(),
        repo: "team/repo".into(),
        number: 1,
        login: "alice".into(),
    }
}
fn repo() -> Repository {
    Repository::new("team/repo", 1, 2)
}

#[tokio::test]
async fn project_toml_selects_rollup_and_help_reports_the_effective_mode() {
    use base64::Engine;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let mut cfg = crate::config::Config::from_env();
    cfg.rollup_enabled = true;
    cfg.merge_queue_enabled = true;
    for (index, enabled) in [None, Some(false), Some(true)].into_iter().enumerate() {
        let server = MockServer::start().await;
        let name = format!("team/project-{index}");
        let id = 9_100_000 + index as i64;
        let installation = 9_100_100;
        let gh = crate::github::Client {
            crab: crate::github::client_builder()
                .base_uri(server.uri())
                .unwrap()
                .personal_token("test")
                .build()
                .unwrap(),
            app_slug: "bot".into(),
        };
        Mock::given(method("GET"))
            .and(path(format!("/repos/{name}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id":id,"default_branch":"main"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{name}/git/ref/heads/main")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"a".repeat(40)}})),
            )
            .mount(&server)
            .await;
        let response = match enabled {
            None => ResponseTemplate::new(404),
            Some(value) => {
                let text = format!("[rollup]\nenabled={value}\nrequired_checks=['test']\nforgejo_required_checks=['ci.yml']\n");
                ResponseTemplate::new(200).set_body_json(json!({"type":"file","encoding":"base64","sha":"b".repeat(40),"content":base64::engine::general_purpose::STANDARD.encode(text)}))
            }
        };
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{name}/contents/.github/xero-bot.toml"
            )))
            .respond_with(response)
            .mount(&server)
            .await;
        assert_eq!(
            ingress::enabled_for(&gh, &cfg, &name, installation)
                .await
                .unwrap(),
            enabled == Some(true)
        );
        let state = crate::config::cache::RepositoryConfigCache::shared()
            .load(
                &gh,
                crate::config::cache::RepositoryKey {
                    installation_id: installation,
                    repository_id: id,
                },
                &name,
            )
            .await;
        for lang in [crate::lang::Lang::En, crate::lang::Lang::Zh] {
            let help = crate::handlers::help::repository_help(
                state.snapshot().unwrap(),
                &cfg,
                lang,
                &crate::handlers::help::HelpSession::Inactive,
            );
            assert_eq!(
                help.contains(lang.pick("Durable rollup:", "持久化 Rollup：")),
                enabled == Some(true)
            );
        }
    }
}

fn batch() -> Batch {
    Batch {
        id: 1,
        members: vec!["github:team/repo:1".into()],
        heads: vec!["a".repeat(40)],
        approvals: vec![],
        base: "b".repeat(40),
        base_branch: "main".into(),
        branch: "automation/xero/rollup/1".into(),
        tip: "c".repeat(40),
        state: "testing".into(),
        created: now(),
        finished: None,
        detail: String::new(),
        try_only: false,
        audit_pr: None,
        audit_attempted: false,
        cleaned: false,
        forgejo_recorded: false,
        github_ci: "failed".into(),
        forgejo_ci: "pending".into(),
        fallback_started: Some(now()),
        passed_on: None,
    }
}
fn approve(r: &mut Repository, number: i64, sha: &str, at: i64) {
    let mut o = origin();
    o.number = number;
    ingress::apply(
        r,
        &Command::ApproveRollup {
            on_behalf_of: None,
            enabled: true,
        },
        o,
        number,
        sha.into(),
        "alice".into(),
        format!("approve:{number}:{at}"),
        at,
        at,
    )
    .unwrap();
}
#[test]
fn parser_rollup_is_explicit_and_never_degrades_invalid_approval() {
    for (text, enabled) in [
        ("@bot r+ rollup", true),
        ("@bot r+ as @alice rollup=never", false),
    ] {
        let p = parse_commands("bot", text);
        assert!(p.diagnostics.is_empty(), "{p:?}");
        assert!(
            matches!(&p.commands[0].command,Command::ApproveRollup{enabled:e,..} if *e==enabled)
        );
        assert!(p.commands[0].is_explicit());
    }
    for text in [
        "@bot r+ rollup=maybe",
        "@bot r+ rollup @extra",
        "@bot r= rollup",
        "@bot rollup=never trailing",
        "> @bot r+ rollup",
        "```\n@bot r+ rollup\n```",
    ] {
        assert!(parse_commands("bot", text).commands.is_empty(), "{text}");
    }
}
#[test]
fn all_queue_control_commands_require_mentions_and_prs() {
    for command in [
        Command::ApproveRollup {
            on_behalf_of: None,
            enabled: true,
        },
        Command::Rollup { enabled: false },
        Command::Try,
        Command::Retry,
    ] {
        assert!(command.requires_pr());
        assert_eq!(
            command.id().default_mode(),
            crate::config::repository::ManualMode::AlwaysMention
        );
    }
}
#[test]
fn config_requires_complete_provider_gates_and_unique_identity_mapping() {
    let mut p = Policy {
        enabled: true,
        ..Default::default()
    };
    assert!(p.clone().validate().is_err());
    p.required_checks = vec!["CI".into()];
    p.forgejo_required_checks = vec!["ci.yml".into()];
    assert!(p.clone().validate().is_ok());
    p.reviewers.insert("one".into(), "github".into());
    p.reviewers.insert("two".into(), "GitHub".into());
    assert!(p.validate().is_err());
}
#[test]
fn approval_and_retry_are_sha_bound_and_old_commands_cannot_revive_withdrawal() {
    let mut r = repo();
    approve(&mut r, 1, &"a".repeat(40), 10);
    ingress::apply(
        &mut r,
        &Command::Reject,
        origin(),
        1,
        "a".repeat(40),
        "alice".into(),
        "reject".into(),
        20,
        100,
    )
    .unwrap();
    assert!(ingress::apply(
        &mut r,
        &Command::Retry,
        origin(),
        1,
        "a".repeat(40),
        "alice".into(),
        "retry".into(),
        21,
        100,
    )
    .is_err());
    assert!(ingress::apply(
        &mut r,
        &Command::Approve { on_behalf_of: None },
        origin(),
        1,
        "a".repeat(40),
        "alice".into(),
        "late".into(),
        11,
        100,
    )
    .is_err());
    approve(&mut r, 1, &"b".repeat(40), 30);
    assert_eq!(r.entries[0].head, "b".repeat(40));
    assert!(ingress::apply(
        &mut r,
        &Command::Retry,
        origin(),
        1,
        "c".repeat(40),
        "alice".into(),
        "wrong-head".into(),
        31,
        100,
    )
    .is_err());
}
#[test]
fn duplicate_commands_and_late_rollup_changes_do_not_mutate_active_batch() {
    let mut r = repo();
    approve(&mut r, 1, &"a".repeat(40), 10);
    approve(&mut r, 1, &"a".repeat(40), 10);
    assert_eq!(r.entries.len(), 1);
    r.entries[0].state = "testing".into();
    r.entries[0].batch = Some(1);
    assert!(ingress::apply(
        &mut r,
        &Command::Rollup { enabled: false },
        origin(),
        1,
        "a".repeat(40),
        "alice".into(),
        "mode".into(),
        12,
        100,
    )
    .is_err());
    approve(&mut r, 2, &"b".repeat(40), 13);
    assert_eq!(r.entries[0].batch, Some(1));
    assert_eq!(r.entries[1].batch, None);
}
fn failed_four_member_batch() -> Repository {
    let mut r = repo();
    for n in 1..=4 {
        approve(&mut r, n, &"a".repeat(40), n);
    }
    let mut b = batch();
    b.members = r.entries.iter().map(|e| e.key.clone()).collect();
    b.heads = vec!["a".repeat(40); 4];
    r.batches.push(b);
    r.active = Some(1);
    for e in &mut r.entries {
        e.state = "testing".into();
        e.batch = Some(1);
    }
    engine::finish(&mut r, 1, "failed", "test failed");
    r
}
#[test]
fn failed_batch_preserves_both_halves_and_does_not_blame_tail() {
    let r = failed_four_member_batch();
    assert_eq!(r.split.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 2]);
    assert!(r.entries.iter().all(|e| e.state == "queued"));
}
#[test]
fn split_batches_isolate_updated_try_and_rollup_never_entries() {
    for command in [Command::Try, Command::Rollup { enabled: false }] {
        for number in 1..=4 {
            let mut r = failed_four_member_batch();
            let keys: Vec<_> = r.entries.iter().map(|e| e.key.clone()).collect();
            let mut o = origin();
            o.number = number;
            ingress::apply(
                &mut r,
                &command,
                o,
                number,
                "a".repeat(40),
                "alice".into(),
                "new-mode".into(),
                10,
                10,
            )
            .unwrap();
            // A new decision keeps its old split key, even when its queue age changes.
            let mut eligible = keys.clone();
            eligible.sort_by_key(|k| r.entry(k).unwrap().created);
            let expected = if number <= 2 {
                vec![
                    vec![keys[0].clone()],
                    vec![keys[1].clone()],
                    keys[2..].to_vec(),
                ]
            } else {
                vec![
                    keys[..2].to_vec(),
                    vec![keys[2].clone()],
                    vec![keys[3].clone()],
                ]
            };
            for group in expected {
                assert_eq!(
                    engine::select_group(&mut r, &eligible, &Policy::default()),
                    group,
                    "{command:?} on PR #{number}"
                );
            }
            assert!(r.split.is_empty());
        }
    }
}
#[test]
fn split_batches_honor_reduced_limit_and_keep_remainder_across_restart() {
    let mut r = failed_four_member_batch();
    let eligible: Vec<_> = r.entries.iter().map(|e| e.key.clone()).collect();
    let policy = Policy {
        max_batch: 1,
        ..Default::default()
    };
    assert_eq!(
        engine::select_group(&mut r, &eligible, &policy),
        eligible[..1]
    );
    assert_eq!(
        r.split,
        vec![eligible[1..2].to_vec(), eligible[2..].to_vec()]
    );
    let dir = crate::forgejo::refs::Scratch::new().unwrap();
    {
        let store = Store::open(&dir.0).unwrap();
        store.rollup_save(&r).unwrap();
    }
    let store = Store::open(&dir.0).unwrap();
    let mut r = store.rollup_load(&r.name).unwrap().unwrap();
    for key in &eligible[1..] {
        assert_eq!(
            engine::select_group(&mut r, &eligible, &policy),
            vec![key.clone()]
        );
    }
    assert!(r.split.is_empty());
}
#[test]
fn normal_batches_keep_collection_window_and_isolate_try_entries() {
    let mut r = repo();
    for n in 1..=4 {
        approve(&mut r, n, &"a".repeat(40), now());
    }
    let mut o = origin();
    o.number = 3;
    ingress::apply(
        &mut r,
        &Command::Try,
        o,
        3,
        "a".repeat(40),
        "alice".into(),
        "try".into(),
        now() + 1,
        10,
    )
    .unwrap();
    let eligible: Vec<_> = r.entries.iter().map(|e| e.key.clone()).collect();
    let mut policy = Policy::default();
    assert!(engine::select_group(&mut r, &eligible, &policy).is_empty());
    policy.collect_seconds = 0;
    assert_eq!(
        engine::select_group(&mut r, &eligible, &policy),
        eligible[..2]
    );
    r.entries[2].created -= 1;
    assert_eq!(
        engine::select_group(&mut r, &eligible[2..], &policy),
        eligible[2..3]
    );
    assert!(r.split.is_empty());
}
#[test]
fn required_checks_reject_wrong_sha_spoofed_app_partial_and_skipped_results() {
    let sha = "c".repeat(40);
    let required = vec!["build".into(), "test".into()];
    let mut checks = vec![
        json!({"id":1,"name":"build","head_sha":sha,"app":{"slug":"github-actions"},"conclusion":"success"}),
    ];
    assert_eq!(
        engine::github_verdict(&checks, &required, &sha),
        engine::Verdict::Pending
    );
    checks.push(json!({"id":2,"name":"test","head_sha":sha,"app":{"slug":"other-app"},"conclusion":"success"}));
    assert_eq!(
        engine::github_verdict(&checks, &required, &sha),
        engine::Verdict::Pending
    );
    checks[1]["app"]["slug"] = json!("github-actions");
    checks[1]["head_sha"] = json!("old");
    assert_eq!(
        engine::github_verdict(&checks, &required, &sha),
        engine::Verdict::Pending
    );
    checks[1]["head_sha"] = json!(sha);
    checks[1]["conclusion"] = json!("skipped");
    assert_eq!(
        engine::github_verdict(&checks, &required, &sha),
        engine::Verdict::Failed
    );
    checks[1]["conclusion"] = json!("success");
    assert_eq!(
        engine::github_verdict(&checks, &required, &sha),
        engine::Verdict::Green
    );
}
#[test]
fn required_checks_cannot_hide_duplicate_named_jobs() {
    let sha = "c".repeat(40);
    let required = vec!["test".into()];
    // Matrix jobs (or separate workflows) may share a name. Their IDs order
    // creation, not retries; all of these jobs belong to the current runs.
    for conclusion in ["failure", "skipped", "cancelled", "neutral"] {
        let checks = vec![
            json!({"id":1,"name":"test","head_sha":sha,"app":{"slug":"github-actions"},"conclusion":conclusion}),
            json!({"id":2,"name":"test","head_sha":sha,"app":{"slug":"github-actions"},"conclusion":"success"}),
        ];
        assert_eq!(
            engine::github_verdict(&checks, &required, &sha),
            engine::Verdict::Failed,
            "{conclusion}"
        );
    }
    let mut checks = vec![
        json!({"id":1,"name":"test","head_sha":sha,"app":{"slug":"github-actions"},"conclusion":null}),
        json!({"id":2,"name":"test","head_sha":sha,"app":{"slug":"github-actions"},"conclusion":"success"}),
    ];
    assert_eq!(
        engine::github_verdict(&checks, &required, &sha),
        engine::Verdict::Pending
    );
    checks[0]["conclusion"] = json!("success");
    assert_eq!(
        engine::github_verdict(&checks, &required, &sha),
        engine::Verdict::Green
    );
}
#[test]
fn durable_restart_retains_approval_candidate_and_fallback_progress() {
    let dir = crate::forgejo::refs::Scratch::new().unwrap();
    let mut r = repo();
    approve(&mut r, 1, &"a".repeat(40), 10);
    r.batches.push(batch());
    r.active = Some(1);
    r.next_attempt = now() + 60;
    {
        let store = Store::open(&dir.0).unwrap();
        store.rollup_save(&r).unwrap();
    }
    let store = Store::open(&dir.0).unwrap();
    let got = store.rollup_load("TEAM/REPO").unwrap().unwrap();
    assert_eq!(got.entries[0].head, r.entries[0].head);
    assert_eq!(got.batches[0].tip, r.batches[0].tip);
    assert_eq!(
        got.batches[0].fallback_started,
        r.batches[0].fallback_started
    );
    assert_eq!(got.next_attempt, r.next_attempt);
}
#[test]
fn public_view_hides_private_stale_snapshots_and_never_exposes_raw_origins() {
    let mut r = repo();
    approve(&mut r, 1, &"a".repeat(40), 10);
    r.entries[0].origin.repo = "private/secret".into();
    r.commands.push("SECRET RAW COMMENT".into());
    r.checked = now();
    assert_eq!(
        web::public_snapshot(vec![r.clone()])["repositories"],
        json!([])
    );
    r.public = true;
    let output = web::public_snapshot(vec![r.clone()]).to_string();
    assert!(!output.contains("private/secret"));
    assert!(!output.contains("SECRET RAW COMMENT"));
    r.checked = now() - 121;
    assert_eq!(web::public_snapshot(vec![r])["repositories"], json!([]));
}
#[tokio::test]
async fn forgejo_fallback_verifies_exact_candidate_and_all_jobs() {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let b = batch();
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/actions/runs")).and(query_param("head_sha",b.tip.clone())).and(query_param("workflow_id","ci.yml"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"total_count":1,"workflow_runs":[{"id":7,"commit_sha":b.tip,"workflow_id":"ci.yml","event":"push","status":"success"}]}))).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo/actions/runs/7/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{"id":1,"run_id":7,"status":"success"},{"id":2,"run_id":7,"status":"success"}]),
        ))
        .mount(&server)
        .await;
    let fj = crate::forgejo::ForgejoClient::new(&server.uri(), "test-token").unwrap();
    assert_eq!(
        engine::forgejo_ci(&fj, "team/repo", &b, &["ci.yml".into()])
            .await
            .unwrap(),
        engine::Verdict::Green
    );
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo/actions/runs/7/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{"id":1,"run_id":7,"status":"success"},{"id":2,"run_id":7,"status":"skipped"}]),
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        engine::forgejo_ci(&fj, "team/repo", &b, &["ci.yml".into()])
            .await
            .unwrap(),
        engine::Verdict::Failed
    );
}

#[test]
fn either_complete_provider_can_pass_but_partial_results_cannot_be_combined() {
    use engine::{passed_provider, Verdict::*};
    assert_eq!(
        passed_provider(Some(&Failed), Some(&Green)),
        Some("forgejo")
    );
    assert_eq!(passed_provider(Some(&Green), Some(&Failed)), Some("github"));
    assert_eq!(passed_provider(None, Some(&Green)), Some("forgejo"));
    assert_eq!(passed_provider(Some(&Failed), Some(&Failed)), None);
    assert_eq!(passed_provider(Some(&Pending), Some(&Pending)), None);
}

#[test]
fn same_second_reordered_approval_cannot_revive_a_withdrawal() {
    let mut r = repo();
    // The withdrawal arrives first; an older webhook in the same second follows.
    ingress::apply(
        &mut r,
        &Command::Reject,
        origin(),
        1,
        "a".repeat(40),
        "alice".into(),
        "withdraw".into(),
        10,
        20,
    )
    .unwrap();
    assert!(ingress::apply(
        &mut r,
        &Command::Approve { on_behalf_of: None },
        origin(),
        1,
        "a".repeat(40),
        "alice".into(),
        "delayed".into(),
        10,
        19
    )
    .is_err());
    assert_eq!(r.entries[0].state, "cancelled");
    ingress::apply(
        &mut r,
        &Command::Approve { on_behalf_of: None },
        origin(),
        1,
        "a".repeat(40),
        "alice".into(),
        "fresh".into(),
        10,
        21,
    )
    .unwrap();
    assert_eq!(r.entries[0].state, "queued");
}

#[test]
fn mapped_duplicate_respects_newest_withdrawal_and_keeps_frozen_audit() {
    let mut r = repo();
    approve(&mut r, 1, &"a".repeat(40), 10);
    let frozen = r.entries[0].clone();
    let mut later = frozen.clone();
    later.key = "forgejo:team/repo:2".into();
    later.origin.provider = "forgejo".into();
    later.decision_time = 20;
    later.state = "cancelled".into();
    r.entries.push(later);
    let mut b = batch();
    b.approvals.push(frozen);
    r.batches.push(b);
    engine::reconcile_duplicates(&mut r);
    assert_eq!(r.entries[0].state, "superseded");
    assert_eq!(r.entries[1].state, "cancelled");
    assert_eq!(r.batches[0].approvals[0].state, "queued");
}

#[test]
fn requested_changes_after_approval_revoke_until_a_new_decision() {
    let reviews = json!([
        {"id":1,"user":{"login":"Alice"},"state":"CHANGES_REQUESTED","submitted_at":"2026-01-01T00:00:10Z"},
        {"id":2,"user":{"login":"alice"},"state":"COMMENTED","submitted_at":"2026-01-01T00:00:11Z"}
    ]);
    let at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:09Z")
        .unwrap()
        .timestamp();
    assert_eq!(
        ingress::blocking_reviewers(reviews.as_array().unwrap(), at),
        vec!["alice"]
    );
    assert!(ingress::blocking_reviewers(reviews.as_array().unwrap(), at + 2).is_empty());
    let mut reviewed = reviews.as_array().unwrap().clone();
    reviewed.push(json!({"id":3,"user":{"login":"alice"},"state":"APPROVED","submitted_at":"2026-01-01T00:00:12Z"}));
    assert!(ingress::blocking_reviewers(&reviewed, at).is_empty());
}

#[tokio::test]
async fn web_defaults_to_english_and_separates_public_project_data() {
    let dir = crate::forgejo::refs::Scratch::new().unwrap();
    let store = Arc::new(Store::open(&dir.0).unwrap());
    for (name, public) in [
        ("team/one", true),
        ("team/two", true),
        ("team/secret", false),
    ] {
        let mut r = Repository::new(name, 1, 2);
        r.public = public;
        r.checked = now();
        store.rollup_save(&r).unwrap();
    }
    let cfg = crate::config::Config::from_env();
    let app: axum::Router = web::routes(web::WebState { cfg, store });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let page = client
        .get(format!("{url}/queue/team/one"))
        .send()
        .await
        .unwrap();
    assert!(page.headers().contains_key("content-security-policy"));
    let page = page.text().await.unwrap();
    assert!(page.contains("<html lang=\"en\""));
    assert!(page.contains("Rollup queue - team/one"));
    assert!(page.contains("/queue/team/one?lang=zh"));
    let zh = client
        .get(format!("{url}/queue/team/one?lang=zh"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(zh.contains("<html lang=\"zh\""));
    assert!(zh.contains("Rollup 队列 - team/one"));
    let data: serde_json::Value = client
        .get(format!("{url}/api/queue/team/one"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(data["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(data["repositories"][0]["repository"], "team/one");
    let index = client
        .get(format!("{url}/api/queue"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(index.contains("team/two"));
    assert!(!index.contains("secret"));
    let commands: serde_json::Value = client
        .get(format!("{url}/api/commands"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(commands["language"], "en");
    assert_eq!(
        commands["commands"].as_array().unwrap().len(),
        crate::config::repository::CommandId::ALL.len()
    );
    server.abort();
}

#[tokio::test]
async fn github_candidate_requires_push_run_on_its_own_branch() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let crab = crate::github::client_builder()
        .base_uri(server.uri())
        .unwrap()
        .personal_token("test")
        .build()
        .unwrap();
    let gh = crate::github::Client {
        crab,
        app_slug: "bot".into(),
    };
    let b = batch();
    Mock::given(method("GET")).and(path("/repos/team/repo/actions/runs")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"total_count":2,"workflow_runs":[
        {"id":1,"workflow_id":1,"head_sha":b.tip,"head_branch":"wrong","event":"push"},
        {"id":2,"workflow_id":2,"head_sha":b.tip,"head_branch":b.branch,"event":"pull_request"}
    ]}))).mount(&server).await;
    assert_eq!(
        engine::github_ci(&gh, "team/repo", &b.tip, &b.branch, &["test".into()])
            .await
            .unwrap(),
        engine::Verdict::Pending
    );
    Mock::given(method("GET"))
        .and(path("/repos/team/repo/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"total_count":1,"workflow_runs":[
                {"id":3,"workflow_id":1,"head_sha":b.tip,"head_branch":b.branch,"event":"push"}
            ]}),
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/repos/team/repo/actions/runs/3/jobs")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"total_count":1,"jobs":[{"id":4,"run_id":3,"head_sha":b.tip,"name":"test","conclusion":"success"}]}))).mount(&server).await;
    assert_eq!(
        engine::github_ci(&gh, "team/repo", &b.tip, &b.branch, &["test".into()])
            .await
            .unwrap(),
        engine::Verdict::Green
    );
}

#[tokio::test]
async fn github_ci_checks_duplicate_jobs_across_latest_workflows() {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let gh = crate::github::Client {
        crab: crate::github::client_builder()
            .base_uri(server.uri())
            .unwrap()
            .personal_token("test")
            .build()
            .unwrap(),
        app_slug: "bot".into(),
    };
    let b = batch();
    Mock::given(method("GET"))
        .and(path("/repos/team/repo/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"total_count":3,"workflow_runs":[
                {"id":1,"workflow_id":1,"head_sha":b.tip,"head_branch":b.branch,"event":"push"},
                {"id":2,"workflow_id":1,"head_sha":b.tip,"head_branch":b.branch,"event":"push"},
                {"id":3,"workflow_id":2,"head_sha":b.tip,"head_branch":b.branch,"event":"push"}
            ]}),
        ))
        .mount(&server)
        .await;
    // Older workflow runs must never be fetched, even after all current jobs pass.
    Mock::given(method("GET"))
        .and(path("/repos/team/repo/actions/runs/1/jobs"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    for (run, job, conclusion) in [(2, 20, "failure"), (3, 30, "success")] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/team/repo/actions/runs/{run}/jobs")))
            .and(query_param("filter", "latest"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"total_count":1,"jobs":[
                    {"id":job,"run_id":run,"head_sha":b.tip,"name":"test","conclusion":conclusion}
                ]})),
            )
            .mount(&server)
            .await;
    }
    assert_eq!(
        engine::github_ci(&gh, "team/repo", &b.tip, &b.branch, &["test".into()])
            .await
            .unwrap(),
        engine::Verdict::Failed
    );
    Mock::given(method("GET"))
        .and(path("/repos/team/repo/actions/runs/2/jobs"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"total_count":2,"jobs":[
                {"id":21,"run_id":2,"head_sha":b.tip,"name":"test","conclusion":null},
                {"id":22,"run_id":2,"head_sha":b.tip,"name":"test","conclusion":"success"}
            ]})),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    assert_eq!(
        engine::github_ci(&gh, "team/repo", &b.tip, &b.branch, &["test".into()])
            .await
            .unwrap(),
        engine::Verdict::Pending
    );
    Mock::given(method("GET"))
        .and(path("/repos/team/repo/actions/runs/2/jobs"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"total_count":2,"jobs":[
                {"id":21,"run_id":2,"head_sha":b.tip,"name":"test","conclusion":"success"},
                {"id":22,"run_id":2,"head_sha":b.tip,"name":"test","conclusion":"success"}
            ]})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        engine::github_ci(&gh, "team/repo", &b.tip, &b.branch, &["test".into()])
            .await
            .unwrap(),
        engine::Verdict::Green
    );
}

#[tokio::test]
async fn forgejo_completion_records_existing_commit_and_reconciles_restarts() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let fj = crate::forgejo::ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let mut r = repo();
    approve(&mut r, 1, &"a".repeat(40), 10);
    let e = &r.entries[0];
    let b = batch();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo/pulls/1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"merged":false,"head":{"sha":e.head},"base":{"ref":"main"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/api/v1/repos/team/repo/pulls/1/merge"))
        .and(body_partial_json(json!({"Do":"manually-merged","MergeCommitID":b.tip,"head_commit_id":e.head,"force_merge":false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).expect(1).mount(&server).await;
    engine::record_forgejo_merge(&fj, "team/repo", 1, e, &b)
        .await
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"merged":true})))
        .with_priority(1)
        .mount(&server)
        .await;
    engine::record_forgejo_merge(&fj, "team/repo", 1, e, &b)
        .await
        .unwrap();
}
#[tokio::test]
async fn forgejo_completion_cannot_close_a_changed_revision() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let fj = crate::forgejo::ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let mut r = repo();
    approve(&mut r, 1, &"a".repeat(40), 10);
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"merged":false,"head":{"sha":"new"},"base":{"ref":"main"}})),
        )
        .mount(&server)
        .await;
    assert!(
        engine::record_forgejo_merge(&fj, "team/repo", 1, &r.entries[0], &batch())
            .await
            .is_err()
    );
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[tokio::test]
async fn revoked_rollup_repository_retains_queue_without_starting_work() {
    let dir = crate::forgejo::refs::Scratch::new().unwrap();
    let r = failed_four_member_batch();
    let before = serde_json::to_value(&r).unwrap();
    {
        let store = Store::open(&dir.0).unwrap();
        store.rollup_save(&r).unwrap();
    }
    let store = Store::open(&dir.0).unwrap();
    let mut cfg = crate::config::Config::from_env();
    cfg.rollup_enabled = true;
    cfg.bot_access_policy = "{}".into();
    cfg.private_key_pem = None;
    engine::pump_all(&cfg, &store).await.unwrap();
    assert_eq!(
        serde_json::to_value(store.rollup_load(&r.name).unwrap().unwrap()).unwrap(),
        before
    );
}
