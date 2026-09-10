// Behavioral tests live here so they can exercise the real reconciler with an
// explicit clock and mocked GitHub, without starting an App or dispatching CI.
use super::*;
use base64::Engine;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPO: &str = "example/project";
const START: i64 = 1_700_000_000;
const RULES: &str = r#"
[idle_workflows]
enabled = true
idle_minutes = 1

[[idle_workflows.monitors]]
workflows = ["ci.yml"]

[[idle_workflows.tasks]]
workflow = "image.yml"
branch = "main"
retry_interval_minutes = 1
max_retries = 2
"#;

fn rules() -> Rules {
    config::parse(RULES, REPO).unwrap().unwrap()
}

fn scheduler() -> Scheduler {
    Scheduler {
        store: Store::open(Path::new(":memory:")).unwrap(),
        pump: tokio::sync::Mutex::new(()),
        boot: START,
        poll_secs: 60,
    }
}

fn observe(scheduler: &Scheduler, repo: &str, sha: &str, timestamp: i64) {
    scheduler
        .store
        .observe(
            repo,
            BTreeMap::from([("branch:main".into(), sha.into())]),
            timestamp,
            150,
        )
        .unwrap();
}

fn contents(text: &str) -> Value {
    json!({"type": "file", "encoding": "base64", "content": base64::engine::general_purpose::STANDARD.encode(text)})
}

fn run(
    id: i64,
    sha: &str,
    status: &str,
    conclusion: Option<&str>,
    attempt: u32,
    timestamp: i64,
) -> Value {
    let time = chrono::DateTime::from_timestamp(timestamp, 0)
        .unwrap()
        .to_rfc3339();
    json!({
        "id": id, "workflow_id": 20, "head_sha": sha, "head_branch": "main",
        "status": status, "conclusion": conclusion, "event": "workflow_dispatch",
        "created_at": time, "updated_at": time, "run_attempt": attempt,
        "actor": {"login": "scheduler[bot]"}
    })
}

async fn standard_mocks(server: &MockServer) {
    for (name, id) in [("ci.yml", 10), ("image.yml", 20)] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/actions/workflows/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"id": id, "path": format!(".github/workflows/{name}"), "state": "active"}),
            ))
            .with_priority(10)
            .mount(server)
            .await;
    }
    for (filename, text) in [
        (CONFIG_PATH, RULES),
        (".github/workflows/image.yml", "on:\n  workflow_dispatch:\n"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/contents/{filename}")))
            .and(query_param("ref", "main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(contents(text)))
            .with_priority(10)
            .mount(server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/git/ref/heads/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": {"sha": "aaa"}})))
        .with_priority(10)
        .mount(server)
        .await;
    for route in ["actions/workflows/20/runs", "actions/runs"] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/{route}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"total_count": 0, "workflow_runs": []})),
            )
            .with_priority(10)
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(format!(
            "/repos/{REPO}/actions/workflows/20/dispatches"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"workflow_run_id": 50})))
        .with_priority(10)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/actions/runs/50/rerun")))
        .respond_with(ResponseTemplate::new(201))
        .with_priority(10)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/branches")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"name": "main", "commit": {"sha": "aaa"}}])),
        )
        .with_priority(10)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/pulls")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .with_priority(10)
        .mount(server)
        .await;
}

async fn fixture() -> (MockServer, Scheduler, BTreeMap<String, Repository>) {
    let server = MockServer::start().await;
    standard_mocks(&server).await;
    let gh = Arc::new(Client {
        crab: crate::github::client_builder()
            .personal_token("test-token")
            .base_uri(server.uri())
            .unwrap()
            .build()
            .unwrap(),
        app_slug: "scheduler".into(),
    });
    let repositories = BTreeMap::from([(
        REPO.into(),
        Repository {
            name: REPO.into(),
            default_branch: "main".into(),
            gh,
            actions_read: true,
            actions_write: true,
        },
    )]);
    let scheduler = scheduler();
    observe(&scheduler, REPO, "aaa", START);
    (server, scheduler, repositories)
}

async fn tick(
    scheduler: &Scheduler,
    repos: &BTreeMap<String, Repository>,
    rules: &Rules,
    timestamp: i64,
) -> Result<&'static str> {
    scheduler
        .reconcile_task(&repos[REPO], &rules.tasks[0], rules, repos, timestamp)
        .await
}

async fn writes(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == "POST")
        .collect()
}

// Changing responses through one responder avoids mixing scoped mocks with
// priority sorting (wiremock 0.6 stores scoped IDs as vector indices).
#[derive(Clone)]
struct MutableResponse(Arc<std::sync::Mutex<Value>>);

impl MutableResponse {
    fn new(value: Value) -> Self {
        Self(Arc::new(std::sync::Mutex::new(value)))
    }
    fn set(&self, value: Value) {
        *self.0.lock().unwrap() = value;
    }
}

impl wiremock::Respond for MutableResponse {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(self.0.lock().unwrap().clone())
    }
}

async fn history(server: &MockServer, value: Value) -> MutableResponse {
    let response = MutableResponse::new(json!({"total_count": 1, "workflow_runs": [value]}));
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/actions/workflows/20/runs")))
        .and(query_param("head_sha", value["head_sha"].as_str().unwrap()))
        .respond_with(response.clone())
        .with_priority(1)
        .mount(server)
        .await;
    response
}

async fn detail(server: &MockServer, value: Value) -> MutableResponse {
    let response = MutableResponse::new(value.clone());
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/actions/runs/{}", value["id"])))
        .respond_with(response.clone())
        .with_priority(1)
        .mount(server)
        .await;
    response
}

#[test]
fn toml_requires_opt_in_and_keeps_monitoring_separate_from_tasks() {
    config::parse(include_str!("../../examples/idle-workflows.toml"), REPO)
        .unwrap()
        .unwrap();
    assert!(config::parse("", REPO).unwrap().is_none());
    assert!(config::parse("[idle_workflows]\nenabled = false", REPO)
        .unwrap()
        .is_none());
    let parsed = rules();
    assert_eq!(parsed.monitors[0].repository.as_deref(), Some(REPO));
    assert_eq!(parsed.monitors[0].workflows, ["ci.yml"]);
    assert_eq!(parsed.tasks[0].workflow, "image.yml");
    let defaults = config::parse("[idle_workflows]\nenabled=true\n[[idle_workflows.tasks]]\nworkflow='image.yml'\nbranch='release/v2'", REPO).unwrap().unwrap();
    assert_eq!(defaults.idle_minutes, 30);
    assert_eq!(defaults.tasks[0].retry_interval_minutes, 15);
    assert_eq!(defaults.tasks[0].max_retries, 2);
    assert!(defaults.monitors[0].workflows.is_empty());
    for bad in [
        RULES.replace("idle_minutes = 1", "idle_minutes = 0"),
        RULES.replace("idle_minutes = 1", "idle_minuts = 1"),
        RULES.replace("max_retries = 2", "max_retries = 101"),
        RULES.replace("image.yml", "../image.yml"),
        format!("{RULES}\nrun_events=['schedule']"),
        format!("{RULES}\ninputs = {{ secret = ['bad'] }}"),
    ] {
        assert!(config::parse(&bad, REPO).is_err(), "accepted {bad}");
    }
    let related = config::parse(
        &RULES.replace(
            "workflows = [\"ci.yml\"]",
            "repository = 'Other/Repo'\nworkflows = ['ci.yml']",
        ),
        REPO,
    )
    .unwrap()
    .unwrap();
    assert_eq!(related.monitors.len(), 2);
    assert_eq!(
        related.monitors[0].repository.as_deref(),
        Some("other/repo")
    );
    assert_eq!(
        config::workflow_name(".github/workflows/image.yml").unwrap(),
        "image.yml"
    );
}

#[test]
fn validate_actual_dispatch_triggers_and_required_typed_inputs() {
    let mut task = rules().tasks.remove(0);
    for yaml in [
        "on: workflow_dispatch",
        "on: [push, workflow_dispatch]",
        "on:\n  workflow_dispatch:",
    ] {
        config::validate_dispatch(yaml, &task).unwrap();
    }
    assert!(config::validate_dispatch("# workflow_dispatch\non: push", &task).is_err());
    let workflow = "on:\n  workflow_dispatch:\n    inputs:\n      channel:\n        required: true\n        type: choice\n        options: [nightly, stable]\n      publish:\n        type: boolean\n        default: false\n";
    assert!(config::validate_dispatch(workflow, &task).is_err());
    task.inputs.insert("channel".into(), json!("nightly"));
    task.inputs.insert("publish".into(), json!(true));
    config::validate_dispatch(workflow, &task).unwrap();
    task.inputs.insert("channel".into(), json!("typo"));
    assert!(config::validate_dispatch(workflow, &task).is_err());
}

#[tokio::test]
async fn waits_for_full_idle_then_dispatches_once_and_tracks_queued_run() {
    let (server, scheduler, repos) = fixture().await;
    let rules = rules();
    assert_eq!(
        tick(&scheduler, &repos, &rules, START + 59).await.unwrap(),
        "waiting for development idle period"
    );
    assert!(writes(&server).await.is_empty());
    tick(&scheduler, &repos, &rules, START + 60).await.unwrap();
    let sent = writes(&server).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&sent[0].body).unwrap(),
        json!({"ref": "main", "inputs": {}, "return_run_details": true})
    );
    let _detail = detail(&server, run(50, "aaa", "queued", None, 1, START + 60)).await;
    assert_eq!(
        tick(&scheduler, &repos, &rules, START + 61).await.unwrap(),
        "run already queued or running"
    );
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn configured_push_ci_blocks_dispatch_but_unselected_workflows_do_not() {
    let (server, scheduler, repos) = fixture().await;
    let mut queued = run(70, "staging-sha", "queued", None, 1, START);
    queued["workflow_id"] = json!(10);
    queued["event"] = json!("push");
    queued["head_branch"] = json!("staging");
    let response = MutableResponse::new(json!({"total_count":1,"workflow_runs":[queued]}));
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/actions/runs")))
        .and(query_param("status", "queued"))
        .respond_with(response.clone())
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 60)
            .await
            .unwrap(),
        "waiting for monitored CI"
    );
    assert!(writes(&server).await.is_empty());
    queued["workflow_id"] = json!(999);
    response.set(json!({"total_count":1,"workflow_runs":[queued]}));
    let outcome = tick(&scheduler, &repos, &rules(), START + 61)
        .await
        .unwrap();
    assert_eq!(outcome, "request sent; awaiting confirmation");
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn success_cancellation_and_exhausted_attempts_never_dispatch() {
    for (conclusion, attempt, expected) in [
        ("success", 1, "already succeeded"),
        ("cancelled", 1, "cancelled; automatic retries suppressed"),
        ("failure", 3, "attempt limit reached"),
        ("skipped", 1, "run needs manual attention"),
    ] {
        let (server, scheduler, repos) = fixture().await;
        let _history = history(
            &server,
            run(50, "aaa", "completed", Some(conclusion), attempt, START),
        )
        .await;
        assert_eq!(
            tick(&scheduler, &repos, &rules(), START + 600)
                .await
                .unwrap(),
            expected
        );
        assert!(writes(&server).await.is_empty());
    }
}

#[tokio::test]
async fn retries_original_run_only_after_both_idle_and_retry_interval() {
    let (server, scheduler, repos) = fixture().await;
    let _history = history(
        &server,
        run(50, "aaa", "completed", Some("failure"), 1, START + 100),
    )
    .await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 159)
            .await
            .unwrap(),
        "waiting for retry interval"
    );
    scheduler.store.activity(REPO, START + 150, None).unwrap();
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 160)
            .await
            .unwrap(),
        "waiting for development idle period"
    );
    tick(&scheduler, &repos, &rules(), START + 210)
        .await
        .unwrap();
    let sent = writes(&server).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].url.path(),
        format!("/repos/{REPO}/actions/runs/50/rerun")
    );
}

#[tokio::test]
async fn retry_attempts_are_bounded_and_confirmation_does_not_repeat_a_rerun() {
    let (server, scheduler, repos) = fixture().await;
    let failed = run(50, "aaa", "completed", Some("failure"), 1, START);
    let h = history(&server, failed.clone()).await;
    tick(&scheduler, &repos, &rules(), START + 60)
        .await
        .unwrap();
    let d = detail(&server, failed).await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 61)
            .await
            .unwrap(),
        "awaiting dispatch confirmation"
    );
    assert_eq!(writes(&server).await.len(), 1);
    let failed = run(50, "aaa", "completed", Some("failure"), 2, START + 90);
    h.set(json!({"total_count":1,"workflow_runs":[failed]}));
    d.set(failed);
    let outcome = tick(&scheduler, &repos, &rules(), START + 150)
        .await
        .unwrap();
    assert_eq!(outcome, "request sent; awaiting confirmation");
    assert_eq!(writes(&server).await.len(), 2);
    let failed = run(50, "aaa", "completed", Some("failure"), 3, START + 180);
    h.set(json!({"total_count":1,"workflow_runs":[failed]}));
    d.set(failed);
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 500)
            .await
            .unwrap(),
        "attempt limit reached"
    );
    assert_eq!(writes(&server).await.len(), 2);
}

#[tokio::test]
async fn accepted_dispatch_with_lost_response_is_adopted_without_duplicate() {
    let (server, scheduler, repos) = fixture().await;
    let _error = Mock::given(method("POST"))
        .and(path(format!(
            "/repos/{REPO}/actions/workflows/20/dispatches"
        )))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount_as_scoped(&server)
        .await;
    assert!(tick(&scheduler, &repos, &rules(), START + 60)
        .await
        .is_err());
    assert_eq!(writes(&server).await.len(), 1); // no automatic POST retry middleware
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 61)
            .await
            .unwrap(),
        "awaiting dispatch confirmation"
    );
    let queued = run(50, "aaa", "in_progress", None, 1, START + 60);
    let _recent = Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/actions/workflows/20/runs")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"total_count":1,"workflow_runs":[queued]})),
        )
        .with_priority(1)
        .mount_as_scoped(&server)
        .await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 62)
            .await
            .unwrap(),
        "run already queued or running"
    );
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn missing_dispatch_is_recovered_after_confirmation_grace() {
    let (server, scheduler, repos) = fixture().await;
    let _empty = Mock::given(method("POST"))
        .and(path(format!(
            "/repos/{REPO}/actions/workflows/20/dispatches"
        )))
        .respond_with(ResponseTemplate::new(204))
        .with_priority(1)
        .mount_as_scoped(&server)
        .await;
    tick(&scheduler, &repos, &rules(), START + 60)
        .await
        .unwrap();
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 359)
            .await
            .unwrap(),
        "awaiting dispatch confirmation"
    );
    assert_eq!(writes(&server).await.len(), 1);
    tick(&scheduler, &repos, &rules(), START + 360)
        .await
        .unwrap();
    assert_eq!(writes(&server).await.len(), 2);
}

#[test]
fn comments_reviews_and_unchanged_open_prs_do_not_reset_idle_but_pushes_do() {
    let scheduler = scheduler();
    let scope = vec![REPO.to_string()];
    let snapshot = BTreeMap::from([
        ("branch:main".into(), "aaa".into()),
        ("pr:1".into(), "pr-head".into()),
    ]);
    scheduler
        .store
        .observe(REPO, snapshot.clone(), START, 150)
        .unwrap();
    for event in [
        "issue_comment",
        "pull_request_review",
        "pull_request_review_comment",
    ] {
        scheduler.observe_webhook(event, &json!({"installation":{"id":1},"repository":{"full_name":REPO},"action":"created"}), Some(event)).unwrap();
    }
    scheduler
        .store
        .observe(REPO, snapshot, START + 50, 150)
        .unwrap();
    assert!(scheduler.store.idle(&scope, START + 60, START, 60).unwrap());
    scheduler.observe_webhook("push", &json!({"installation":{"id":1},"repository":{"full_name":REPO},"ref":"refs/heads/topic","deleted":false}), Some("push-1")).unwrap();
    assert!(!scheduler.store.idle(&scope, START + 60, START, 60).unwrap());
}

#[test]
fn duplicate_deliveries_do_not_extend_the_idle_timer_and_monitoring_gaps_do() {
    let scheduler = scheduler();
    let scope = vec![REPO.to_string()];
    observe(&scheduler, REPO, "aaa", START);
    scheduler
        .store
        .activity(REPO, START + 10, Some("delivery-1"))
        .unwrap();
    scheduler
        .store
        .activity(REPO, START + 60, Some("delivery-1"))
        .unwrap();
    assert!(scheduler.store.idle(&scope, START + 70, START, 60).unwrap());
    observe(&scheduler, REPO, "aaa", START + 300);
    assert!(!scheduler
        .store
        .idle(&scope, START + 301, START, 60)
        .unwrap());
}

#[tokio::test]
async fn latest_branch_tip_wins_and_superseded_failed_run_is_not_retried() {
    let (server, scheduler, repos) = fixture().await;
    let key = Store::target_key(REPO, 20, "main");
    let mut state = TargetState::default();
    state.builds.insert(
        "aaa".into(),
        store::Build {
            attempts: 1,
            run_id: Some(99),
            ..Default::default()
        },
    );
    scheduler.store.save_target(&key, &state).unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/git/ref/heads/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"bbb"}})))
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 60)
            .await
            .unwrap(),
        "waiting for development idle period"
    );
    tick(&scheduler, &repos, &rules(), START + 120)
        .await
        .unwrap();
    assert_eq!(writes(&server).await.len(), 1);
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.url.path().contains("runs/99")));
    assert_eq!(
        scheduler.store.target(&key).unwrap().pending.unwrap().sha,
        "bbb"
    );
}

#[tokio::test]
async fn branch_move_during_final_check_prevents_a_write() {
    let (server, scheduler, repos) = fixture().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/git/ref/heads/main")))
        .respond_with(move |_: &wiremock::Request| {
            let count = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ResponseTemplate::new(200)
                .set_body_json(json!({"object":{"sha":if count == 0 { "aaa" } else { "bbb" }}}))
        })
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 60)
            .await
            .unwrap(),
        "branch advanced; waiting for latest commit"
    );
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn credit_actual_run_sha_when_branch_moves_as_dispatch_is_accepted() {
    let (server, scheduler, repos) = fixture().await;
    tick(&scheduler, &repos, &rules(), START + 60)
        .await
        .unwrap();
    let _detail = detail(
        &server,
        run(50, "bbb", "completed", Some("success"), 1, START + 60),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/git/ref/heads/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"bbb"}})))
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 61)
            .await
            .unwrap(),
        "already succeeded"
    );
    let state = scheduler
        .store
        .target(&Store::target_key(REPO, 20, "main"))
        .unwrap();
    assert_eq!(state.builds["bbb"].terminal.as_deref(), Some("success"));
    assert!(state.builds["aaa"].terminal.is_none());
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn related_repository_activity_and_ci_both_block_dispatch() {
    let (server, scheduler, mut repos) = fixture().await;
    let other = "example/related";
    repos.insert(
        other.into(),
        Repository {
            name: other.into(),
            default_branch: "main".into(),
            gh: Arc::clone(&repos[REPO].gh),
            actions_read: true,
            actions_write: false,
        },
    );
    observe(&scheduler, other, "other-sha", START + 50);
    let mut rules = rules();
    rules.monitors.push(config::Monitor {
        repository: Some(other.into()),
        workflows: vec![],
    });
    assert_eq!(
        tick(&scheduler, &repos, &rules, START + 60).await.unwrap(),
        "waiting for development idle period"
    );
    let busy = run(80, "other-sha", "queued", None, 1, START);
    Mock::given(method("GET"))
        .and(path(format!("/repos/{other}/actions/runs")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"total_count":1,"workflow_runs":[busy]})),
        )
        .mount(&server)
        .await;
    assert_eq!(
        tick(&scheduler, &repos, &rules, START + 110).await.unwrap(),
        "waiting for monitored CI"
    );
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn actions_permission_errors_and_incomplete_responses_fail_closed() {
    for body in [
        json!({}),
        json!({"total_count":1001,"workflow_runs":[]}),
        json!({"total_count":1,"workflow_runs":[]}),
    ] {
        let (server, scheduler, repos) = fixture().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/actions/runs")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(1)
            .mount(&server)
            .await;
        assert!(tick(&scheduler, &repos, &rules(), START + 60)
            .await
            .is_err());
        assert!(writes(&server).await.is_empty());
    }
    let (server, scheduler, mut repos) = fixture().await;
    repos.get_mut(REPO).unwrap().actions_write = false;
    assert!(tick(&scheduler, &repos, &rules(), START + 60)
        .await
        .unwrap_err()
        .to_string()
        .contains("Actions: write"));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn checks_later_actions_pages_before_declaring_scope_idle() {
    let (server, scheduler, repos) = fixture().await;
    let first: Vec<_> = (100..200)
        .map(|id| {
            let mut value = run(id, "aaa", "queued", None, 1, START);
            value["workflow_id"] = json!(999);
            value
        })
        .collect();
    let mut last = run(200, "aaa", "queued", None, 1, START);
    last["workflow_id"] = json!(10);
    for (page, batch) in [("1", first), ("2", vec![last])] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/actions/runs")))
            .and(query_param("status", "queued"))
            .and(query_param("page", page))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"total_count":101,"workflow_runs":batch})),
            )
            .with_priority(1)
            .mount(&server)
            .await;
    }
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 60)
            .await
            .unwrap(),
        "waiting for monitored CI"
    );
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn discovers_default_branch_config_and_hot_reloads_disable_or_invalid_rules() {
    let (server, scheduler, repos) = fixture().await;
    let response = MutableResponse::new(contents(RULES));
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/contents/{CONFIG_PATH}")))
        .and(query_param("ref", "main"))
        .respond_with(response.clone())
        .with_priority(1)
        .mount(&server)
        .await;
    scheduler.pump_repositories(&repos, START).await.unwrap();
    assert!(writes(&server).await.is_empty());
    scheduler
        .pump_repositories(&repos, START + 60)
        .await
        .unwrap();
    assert_eq!(writes(&server).await.len(), 1);
    response.set(contents("[idle_workflows]\nenabled=false"));
    assert!(scheduler
        .pump_repositories(&repos, START + 120)
        .await
        .unwrap()
        .contains("0 tasks"));
    response.set(contents("[idle_workflows]\nenabled='typo'"));
    assert!(scheduler
        .pump_repositories(&repos, START + 180)
        .await
        .unwrap()
        .contains("1 errors"));
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn a_successful_schedule_is_not_assumed_to_have_built_the_run_sha() {
    let (server, scheduler, repos) = fixture().await;
    let mut scheduled = run(50, "aaa", "completed", Some("success"), 1, START);
    scheduled["event"] = json!("schedule");
    let _h = history(&server, scheduled).await;
    tick(&scheduler, &repos, &rules(), START + 60)
        .await
        .unwrap();
    assert_eq!(writes(&server).await.len(), 1);
}

#[tokio::test]
async fn restart_recovers_pending_run_and_retains_retry_budget_and_exclusive_ownership() {
    let (server, _, repos) = fixture().await;
    let directory = std::env::temp_dir().join(format!(
        "xero-idle-restart-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    let mut first = Scheduler::open(&directory, 60).unwrap();
    first.boot = START;
    observe(&first, REPO, "aaa", START);
    tick(&first, &repos, &rules(), START + 60).await.unwrap();
    assert!(
        Scheduler::open(&directory, 60).is_err(),
        "second writer must not share the DB"
    );
    drop(first);
    let mut second = Scheduler::open(&directory, 60).unwrap();
    second.boot = START + 90;
    let response = detail(&server, run(50, "aaa", "in_progress", None, 1, START + 60)).await;
    assert_eq!(
        tick(&second, &repos, &rules(), START + 91).await.unwrap(),
        "run already queued or running"
    );
    assert_eq!(writes(&server).await.len(), 1);
    response.set(run(50, "aaa", "completed", Some("failure"), 1, START + 100));
    tick(&second, &repos, &rules(), START + 160).await.unwrap();
    assert_eq!(writes(&server).await.len(), 2);
    drop(second);
    let mut third = Scheduler::open(&directory, 60).unwrap();
    third.boot = START + 180;
    response.set(run(50, "aaa", "completed", Some("failure"), 2, START + 180));
    tick(&third, &repos, &rules(), START + 240).await.unwrap();
    assert_eq!(writes(&server).await.len(), 3);
    response.set(run(50, "aaa", "completed", Some("failure"), 3, START + 250));
    assert_eq!(
        tick(&third, &repos, &rules(), START + 500).await.unwrap(),
        "attempt limit reached"
    );
    assert_eq!(writes(&server).await.len(), 3);
    drop(third);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn cron_and_background_ticks_share_a_single_writer() {
    let scheduler = scheduler();
    let mut cfg = Config::from_env();
    cfg.idle_workflows_enabled = true;
    let _guard = scheduler.pump.lock().await;
    assert_eq!(
        scheduler.pump_all(&cfg).await,
        "idle workflow reconciliation already running"
    );
}

#[tokio::test]
async fn cancellation_during_ci_queries_is_seen_before_rerun() {
    let (server, scheduler, repos) = fixture().await;
    let response = history(
        &server,
        run(50, "aaa", "completed", Some("failure"), 1, START),
    )
    .await;
    Mock::given(method("GET")).and(path(format!("/repos/{REPO}/actions/runs")))
        .respond_with(move |_: &wiremock::Request| {
            response.set(json!({"total_count":1,"workflow_runs":[run(50,"aaa","completed",Some("cancelled"),1,START+60)]}));
            ResponseTemplate::new(200).set_body_json(json!({"total_count":0,"workflow_runs":[]}))
        }).with_priority(1).mount(&server).await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 60)
            .await
            .unwrap(),
        "workflow state changed; reconcile again"
    );
    assert!(writes(&server).await.is_empty());
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 61)
            .await
            .unwrap(),
        "cancelled; automatic retries suppressed"
    );
}

#[tokio::test]
async fn cancellations_are_respected_even_outside_equivalent_success_events() {
    let (server, scheduler, repos) = fixture().await;
    let mut value = run(50, "aaa", "completed", Some("cancelled"), 1, START);
    value["event"] = json!("schedule");
    let _response = history(&server, value).await;
    assert_eq!(
        tick(&scheduler, &repos, &rules(), START + 60)
            .await
            .unwrap(),
        "cancelled; automatic retries suppressed"
    );
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn empty_monitor_selection_waits_for_any_workflow_including_waiting_runs() {
    let (server, scheduler, repos) = fixture().await;
    let mut rules = rules();
    rules.monitors[0].workflows.clear();
    let mut waiting = run(123, "different-sha", "waiting", None, 1, START);
    waiting["workflow_id"] = json!(999);
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/actions/runs")))
        .and(query_param("status", "waiting"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"total_count":1,"workflow_runs":[waiting]})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        tick(&scheduler, &repos, &rules, START + 60).await.unwrap(),
        "waiting for monitored CI"
    );
    assert!(writes(&server).await.is_empty());
}

#[tokio::test]
async fn successful_revision_stays_complete_after_restart_and_history_removal() {
    let (server, _, repos) = fixture().await;
    let directory = std::env::temp_dir().join(format!(
        "xero-idle-success-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    let mut first = Scheduler::open(&directory, 60).unwrap();
    first.boot = START;
    observe(&first, REPO, "aaa", START);
    let response = history(
        &server,
        run(50, "aaa", "completed", Some("success"), 1, START),
    )
    .await;
    assert_eq!(
        tick(&first, &repos, &rules(), START + 60).await.unwrap(),
        "already succeeded"
    );
    drop(first);
    response.set(json!({"total_count":0,"workflow_runs":[]}));
    let second = Scheduler::open(&directory, 60).unwrap();
    assert_eq!(
        tick(&second, &repos, &rules(), START + 3600).await.unwrap(),
        "already succeeded"
    );
    assert!(writes(&server).await.is_empty());
    drop(second);
    std::fs::remove_dir_all(directory).unwrap();
}
