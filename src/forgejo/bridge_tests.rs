use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "xero-bridge-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn object(number: i64, is_pr: bool, repo: &str) -> Value {
    let mut value = json!({"number":number,"title":"A title","body":"A body","state":"open", "labels":[{"id":8,"name":"bug","color":"aabbcc","description":"bug report"}]});
    if is_pr {
        value["head"] = json!({"ref":"topic","repo":{"full_name":repo}});
        value["base"] = json!({"ref":"main","repo":{"full_name":repo}});
    }
    value
}
fn event_item(value: &Value, is_pr: bool) -> Item {
    github_item(
        if is_pr { "pull_request" } else { "issues" },
        &json!({"action":"opened", "issue":value,"pull_request":value}),
    )
    .unwrap()
}
fn gh(server: &MockServer) -> Client {
    Client {
        crab: crate::github::client_builder()
            .personal_token("github-test-token")
            .base_uri(server.uri())
            .unwrap()
            .build()
            .unwrap(),
        app_slug: "bridge-bot".into(),
    }
}
fn cfg(server: &MockServer) -> Config {
    let mut cfg = Config::from_env();
    cfg.forgejo_base_url = server.uri();
    cfg.forgejo_token = "forgejo-test-token".into();
    cfg.forgejo_webhook_secret = "bridge-webhook-secret".into();
    cfg
}
async fn respond(server: &MockServer, verb: &str, route: &str, value: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(server)
        .await;
}
fn prefix(target: Target) -> &'static str {
    if target == Target::Forgejo {
        "/api/v1"
    } else {
        ""
    }
}

#[tokio::test]
async fn both_providers_issue_and_pr_lifecycle_deduplicate_and_reopen() {
    for target in [Target::GitHub, Target::Forgejo] {
        for is_pr in [false, true] {
            let github = MockServer::start().await;
            let forgejo = MockServer::start().await;
            let gh = gh(&github);
            let fj = ForgejoClient::new(&forgejo.uri(), "forgejo-token").unwrap();
            let dir = Dir::new();
            let store = Store::open(&dir.0).unwrap();
            let (source, dest) = if target == Target::Forgejo {
                (&github, &forgejo)
            } else {
                (&forgejo, &github)
            };
            let source_prefix = if target == Target::GitHub {
                "/api/v1"
            } else {
                ""
            };
            let target_prefix = prefix(target);
            let object_type = if is_pr { "pulls" } else { "issues" };
            let live = Arc::new(Mutex::new(object(4, is_pr, "source/repo")));
            let snapshot = live.clone();
            Mock::given(method("GET"))
                .and(path(format!(
                    "{source_prefix}/repos/source/repo/{object_type}/4"
                )))
                .respond_with(move |_: &Request| {
                    ResponseTemplate::new(200).set_body_json(snapshot.lock().unwrap().clone())
                })
                .mount(source)
                .await;
            Mock::given(method("POST"))
                .and(path(format!(
                    "{target_prefix}/repos/dest/repo/{object_type}"
                )))
                .and(body_partial_json(json!({"title":"A title"})))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number":93})))
                .expect(1)
                .mount(dest)
                .await;
            respond(
                dest,
                "PATCH",
                &format!("{target_prefix}/repos/dest/repo/{object_type}/93"),
                json!({"number":93}),
            )
            .await;
            respond(
                dest,
                "GET",
                &format!("{target_prefix}/repos/dest/repo/labels"),
                json!([{"id":32,"name":"bug"}]),
            )
            .await;
            respond(
                dest,
                "PUT",
                &format!("{target_prefix}/repos/dest/repo/issues/93/labels"),
                json!([]),
            )
            .await;
            let input = event_item(&live.lock().unwrap(), is_pr);
            let run = || {
                forward_item(
                    &gh.crab,
                    &fj,
                    &store,
                    "secret-key",
                    target,
                    "dest/repo",
                    "source/repo",
                    &input,
                    "scope",
                    &[],
                )
            };
            let (first, duplicate) = tokio::join!(run(), run());
            assert_eq!(first, Ok(BridgeOutcome::Synced));
            assert_eq!(duplicate, Ok(BridgeOutcome::AlreadyCurrent));
            assert_eq!(
                store
                    .bridge_peer("scope", kind(is_pr).as_str(), peer_side(target), 4)
                    .unwrap(),
                Some(93)
            );
            assert_eq!(
                store
                    .bridge_peer("scope", kind(is_pr).as_str(), side(target), 93)
                    .unwrap(),
                Some(4)
            );
            // A stale `opened` delivery reflects the live closed/reopened state.
            live.lock().unwrap()["state"] = json!("closed");
            assert_eq!(run().await, Ok(BridgeOutcome::Synced));
            live.lock().unwrap()["state"] = json!("open");
            live.lock().unwrap()["title"] = json!("Edited live title");
            live.lock().unwrap()["body"] = json!("Edited live body");
            live.lock().unwrap()["labels"] = json!([]);
            assert_eq!(run().await, Ok(BridgeOutcome::Synced));
            let requests = dest.received_requests().await.unwrap();
            let patches = requests
                .iter()
                .filter(|r| r.method == "PATCH")
                .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(patches.len(), 3);
            assert_eq!(patches[1]["state"], "closed");
            assert_eq!(patches[2]["state"], "open");
            assert_eq!(patches[2]["title"], "Edited live title");
            let marker =
                parse_marker(patches[2]["body"].as_str().unwrap(), "secret-key", "scope").unwrap();
            assert_eq!(marker.peer, 4);
            let puts = requests
                .iter()
                .filter(|r| r.method == "PUT")
                .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                puts[0]["labels"],
                if target == Target::Forgejo {
                    json!([32])
                } else {
                    json!(["bug"])
                }
            );
            assert_eq!(puts[2]["labels"], json!([]));
            if is_pr {
                assert_eq!(patches[2]["base"], "main");
            }
            // Equal canonical content on the peer is an echo, even with a different number.
            let mut echoed = live.lock().unwrap().clone();
            echoed["number"] = json!(93);
            echoed["body"] = patches[2]["body"].clone();
            respond(
                dest,
                "GET",
                &format!("{target_prefix}/repos/dest/repo/{object_type}/93"),
                echoed.clone(),
            )
            .await;
            let echo = event_item(&echoed, is_pr);
            let reverse = if target == Target::Forgejo {
                Target::GitHub
            } else {
                Target::Forgejo
            };
            assert_eq!(
                forward_item(
                    &gh.crab,
                    &fj,
                    &store,
                    "secret-key",
                    reverse,
                    "source/repo",
                    "dest/repo",
                    &echo,
                    "scope",
                    &[]
                )
                .await,
                Ok(BridgeOutcome::AlreadyCurrent)
            );
            assert!(source
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method == "GET"));
        }
    }
}

#[tokio::test]
async fn uncertain_create_survives_restart_and_only_verified_receipt_unblocks_it() {
    let github = MockServer::start().await;
    let forgejo = MockServer::start().await;
    let gh = gh(&github);
    let fj = ForgejoClient::new(&forgejo.uri(), "forgejo-token").unwrap();
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let raw = object(4, false, "source/repo");
    let item = event_item(&raw, false);
    respond(&forgejo, "GET", "/api/v1/repos/source/repo/issues/4", raw).await;
    Mock::given(method("POST"))
        .and(path("/repos/dest/repo/issues"))
        .respond_with(ResponseTemplate::new(201).set_body_string("broken JSON"))
        .expect(1)
        .mount(&github)
        .await;
    assert!(forward_item(
        &gh.crab,
        &fj,
        &store,
        "secret",
        Target::GitHub,
        "dest/repo",
        "source/repo",
        &item,
        "scope",
        &[]
    )
    .await
    .is_err());
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    assert_eq!(
        forward_item(
            &gh.crab,
            &fj,
            &store,
            "secret",
            Target::GitHub,
            "dest/repo",
            "source/repo",
            &item,
            "scope",
            &[]
        )
        .await,
        Err(BridgeOutcome::NeedsReconciliation)
    );
    assert_eq!(store.bridge_status().unwrap().len(), 1);
    store
        .bridge_resolve(
            "scope",
            "issue",
            "forgejo",
            4,
            93,
            "verified remote object exists",
        )
        .unwrap();
    respond(&github, "PATCH", "/repos/dest/repo/issues/93", json!({})).await;
    respond(
        &github,
        "GET",
        "/repos/dest/repo/labels",
        json!([{"id":32,"name":"bug"}]),
    )
    .await;
    respond(
        &github,
        "PUT",
        "/repos/dest/repo/issues/93/labels",
        json!([]),
    )
    .await;
    assert_eq!(
        forward_item(
            &gh.crab,
            &fj,
            &store,
            "secret",
            Target::GitHub,
            "dest/repo",
            "source/repo",
            &item,
            "scope",
            &[]
        )
        .await,
        Ok(BridgeOutcome::Synced)
    );
    assert!(store.bridge_status().unwrap().is_empty());
}

#[tokio::test]
async fn partial_create_resumes_metadata_without_repeating_post_or_patching_source() {
    let github = MockServer::start().await;
    let forgejo = MockServer::start().await;
    let gh = gh(&github);
    let fj = ForgejoClient::new(&forgejo.uri(), "forgejo-token").unwrap();
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let raw = object(4, false, "source/repo");
    let item = event_item(&raw, false);
    respond(&github, "GET", "/repos/source/repo/issues/4", raw).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/dest/repo/issues"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number":93})))
        .expect(1)
        .mount(&forgejo)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/dest/repo/issues/93"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"message":"unavailable"})))
        .up_to_n_times(1)
        .expect(1)
        .with_priority(1)
        .mount(&forgejo)
        .await;
    respond(
        &forgejo,
        "PATCH",
        "/api/v1/repos/dest/repo/issues/93",
        json!({}),
    )
    .await;
    respond(
        &forgejo,
        "GET",
        "/api/v1/repos/dest/repo/labels",
        json!([{"id":32,"name":"bug"}]),
    )
    .await;
    respond(
        &forgejo,
        "PUT",
        "/api/v1/repos/dest/repo/issues/93/labels",
        json!([]),
    )
    .await;
    let run = || {
        forward_item(
            &gh.crab,
            &fj,
            &store,
            "secret",
            Target::Forgejo,
            "dest/repo",
            "source/repo",
            &item,
            "scope",
            &[],
        )
    };
    assert_eq!(run().await, Err(BridgeOutcome::Rejected(503)));
    assert_eq!(
        store.bridge_peer("scope", "issue", "github", 4).unwrap(),
        Some(93)
    );
    assert_eq!(run().await, Ok(BridgeOutcome::Synced));
    assert!(github
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[tokio::test]
async fn comments_deduplicate_and_unknown_receipts_never_report_success() {
    for target in [Target::GitHub, Target::Forgejo] {
        for success in [false, true] {
            let github = MockServer::start().await;
            let forgejo = MockServer::start().await;
            let gh = gh(&github);
            let fj = ForgejoClient::new(&forgejo.uri(), "forgejo-token").unwrap();
            let dir = Dir::new();
            let store = Store::open(&dir.0).unwrap();
            let dest = if target == Target::Forgejo {
                &forgejo
            } else {
                &github
            };
            Mock::given(method("POST"))
                .and(path(format!(
                    "{}/repos/dest/repo/issues/93/comments",
                    prefix(target)
                )))
                .respond_with(
                    ResponseTemplate::new(if success { 201 } else { 503 }).set_body_json(
                        if success {
                            json!({"id":888})
                        } else {
                            json!({"message":"unavailable"})
                        },
                    ),
                )
                .expect(1)
                .mount(dest)
                .await;
            let comment = Comment {
                id: 13,
                number: 4,
                is_pr: false,
                body: "hello".into(),
                author: "alice".into(),
            };
            for attempt in 0..2 {
                let result = if target == Target::Forgejo {
                    forward_comment_to_forgejo(
                        &fj,
                        &store,
                        "secret",
                        "dest/repo",
                        "scope",
                        93,
                        &comment,
                    )
                    .await
                } else {
                    forward_comment_to_github(
                        &gh,
                        &store,
                        "secret",
                        "dest/repo",
                        "scope",
                        93,
                        &comment,
                    )
                    .await
                };
                if success {
                    assert_eq!(
                        result,
                        Ok(if attempt == 0 {
                            BridgeOutcome::Synced
                        } else {
                            BridgeOutcome::AlreadyCurrent
                        })
                    );
                } else if attempt == 1 {
                    assert_eq!(result, Err(BridgeOutcome::NeedsReconciliation));
                } else {
                    assert!(result.is_err());
                }
            }
            let body: Value =
                serde_json::from_slice(&dest.received_requests().await.unwrap()[0].body).unwrap();
            assert!(body["body"].as_str().unwrap().contains("**@alice**"));
            assert!(is_bridge_comment(
                body["body"].as_str().unwrap(),
                "secret",
                "scope"
            ));
        }
    }
}

#[tokio::test]
async fn copied_markers_and_fork_prs_never_authorize_a_create_or_update() {
    let github = MockServer::start().await;
    let forgejo = MockServer::start().await;
    let gh = gh(&github);
    let fj = ForgejoClient::new(&forgejo.uri(), "forgejo-token").unwrap();
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let mut copied = object(4, false, "source/repo");
    copied["body"] = json!(marker("secret", "scope", 99, Kind::Issue, &"a".repeat(64)));
    respond(
        &github,
        "GET",
        "/repos/source/repo/issues/4",
        copied.clone(),
    )
    .await;
    assert_eq!(
        forward_item(
            &gh.crab,
            &fj,
            &store,
            "secret",
            Target::Forgejo,
            "dest/repo",
            "source/repo",
            &event_item(&copied, false),
            "scope",
            &[]
        )
        .await,
        Ok(BridgeOutcome::Ignored("unmapped-marked-object"))
    );
    let fork = object(5, true, "fork/repo");
    respond(&github, "GET", "/repos/source/repo/pulls/5", fork.clone()).await;
    assert_eq!(
        forward_item(
            &gh.crab,
            &fj,
            &store,
            "secret",
            Target::Forgejo,
            "dest/repo",
            "source/repo",
            &event_item(&fork, true),
            "scope",
            &[]
        )
        .await,
        Ok(BridgeOutcome::Ignored("fork-pr-not-mirrored"))
    );
    assert!(forgejo.received_requests().await.unwrap().is_empty());
}

#[test]
fn bridge_mapping_is_bijective_and_instance_scoped() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    store
        .bridge_link("a", "issue", "github", 4, "forgejo", 93, "rev", 1)
        .unwrap();
    assert!(store
        .bridge_link("a", "issue", "github", 5, "forgejo", 93, "rev", 2)
        .is_err());
    assert!(store
        .bridge_peer("a", "issue", "github", 5)
        .unwrap()
        .is_none());
    assert_eq!(
        store.bridge_peer("a", "issue", "forgejo", 93).unwrap(),
        Some(4)
    );
    let fj = ForgejoClient::new("https://git.example.com/subpath", "forgejo-token").unwrap();
    let same =
        ForgejoClient::new("https://git.example.com/subpath/api/v1/", "forgejo-token").unwrap();
    let other = ForgejoClient::new("https://other.example.com", "forgejo-token").unwrap();
    let key = RepositoryKey {
        installation_id: 1,
        repository_id: 2,
    };
    assert_eq!(
        bridge_scope(&fj, key, 3, "A/B", "C/D"),
        bridge_scope(&same, key, 3, "a/b", "c/d")
    );
    assert_ne!(
        bridge_scope(&fj, key, 3, "a/b", "c/d"),
        bridge_scope(&other, key, 3, "a/b", "c/d")
    );
    assert_ne!(
        bridge_scope(&fj, key, 3, "a/b", "c/d"),
        bridge_scope(&fj, key, 4, "a/b", "c/d")
    );
}

const POLICY: &str="[forgejo.bridge]\nenabled=true\ndestination='dest/repo'\nissues=true\npull_requests=true\ncomments=true";
async fn policies(
    github: &MockServer,
    forgejo: &MockServer,
    github_id: i64,
    remote_policy: &str,
    origin: &str,
) {
    use base64::Engine;
    respond(
        github,
        "GET",
        "/repos/source/repo",
        json!({"id":github_id,"default_branch":"main"}),
    )
    .await;
    respond(
        github,
        "GET",
        "/repos/source/repo/git/ref/heads/main",
        json!({"object":{"sha":"a".repeat(40)}}),
    )
    .await;
    respond(
        forgejo,
        "GET",
        "/api/v1/repos/dest/repo",
        json!({"id":70,"default_branch":"main","mirror":true,"original_url":origin}),
    )
    .await;
    respond(
        forgejo,
        "GET",
        "/api/v1/repos/dest/repo/branches/main",
        json!({"commit":{"id":"a".repeat(40)}}),
    )
    .await;
    for (server, route, policy) in [
        (
            github,
            "/repos/source/repo/contents/.github/xero-bot.toml",
            POLICY,
        ),
        (
            forgejo,
            "/api/v1/repos/dest/repo/contents/.github/xero-bot.toml",
            remote_policy,
        ),
    ] {
        Mock::given(method("GET")).and(path(route)).and(query_param("ref","a".repeat(40)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type":"file","encoding":"base64","sha":"b".repeat(40),"content":base64::engine::general_purpose::STANDARD.encode(policy)})))
            .mount(server).await;
    }
}

#[tokio::test]
async fn webhook_entry_requires_bilateral_opt_in_and_matching_mirror_origin() {
    for (remote_policy, origin, expected) in [
        (
            "",
            "https://github.com/source/repo.git",
            BridgeOutcome::BridgeDisabled,
        ),
        (
            POLICY,
            "https://github.com/other/repo.git",
            BridgeOutcome::Ignored("mirror-origin-mismatch"),
        ),
        (
            "[forgejo.bridge]\nenabled=true",
            "https://github.com/source/repo.git",
            BridgeOutcome::InvalidConfiguration,
        ),
        (
            POLICY,
            "https://github.com/source/repo.git",
            BridgeOutcome::Synced,
        ),
    ] {
        static NEXT: AtomicU64 = AtomicU64::new(9000);
        let repository_id = NEXT.fetch_add(1, Ordering::SeqCst) as i64;
        let github = MockServer::start().await;
        let forgejo = MockServer::start().await;
        let gh = gh(&github);
        let cfg = cfg(&forgejo);
        let dir = Dir::new();
        let store = Store::open(&dir.0).unwrap();
        let key = RepositoryKey {
            installation_id: 901,
            repository_id,
        };
        policies(&github, &forgejo, repository_id, remote_policy, origin).await;
        let raw = object(4, false, "source/repo");
        respond(&github, "GET", "/repos/source/repo/issues/4", raw.clone()).await;
        respond(
            &forgejo,
            "POST",
            "/api/v1/repos/dest/repo/issues",
            json!({"number":93}),
        )
        .await;
        respond(
            &forgejo,
            "PATCH",
            "/api/v1/repos/dest/repo/issues/93",
            json!({}),
        )
        .await;
        respond(
            &forgejo,
            "GET",
            "/api/v1/repos/dest/repo/labels",
            json!([{"id":32,"name":"bug"}]),
        )
        .await;
        respond(
            &forgejo,
            "GET",
            "/api/v1/repos/dest/repo/issues/93/labels",
            json!([]),
        )
        .await;
        respond(
            &forgejo,
            "PUT",
            "/api/v1/repos/dest/repo/issues/93/labels",
            json!([]),
        )
        .await;
        let payload = json!({"action":"opened","issue":raw,"sender":{"login":"alice"}});
        assert_eq!(
            from_github(&gh, &cfg, &store, "source/repo", key, "issues", &payload).await,
            expected
        );
        if expected != BridgeOutcome::Synced {
            assert!(forgejo
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method == "GET"));
        } else {
            let policy = github_policy(&gh, &cfg, "source/repo", key, "different/repo").await;
            assert_eq!(policy, Err(BridgeOutcome::Ignored("destination-mismatch")));
        }
    }
}

#[tokio::test]
async fn disabled_provider_and_own_comments_do_not_fetch_policy_or_write() {
    let github = MockServer::start().await;
    let forgejo = MockServer::start().await;
    let gh = gh(&github);
    let mut cfg = cfg(&forgejo);
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let key = RepositoryKey {
        installation_id: 1,
        repository_id: 2,
    };
    let payload = json!({"action":"created","issue":{"number":4},"comment":{"id":99,"body":"reply without marker","user":{"login":"bridge-bot[bot]"}}});
    assert_eq!(
        comment_from_github(&gh, &cfg, &store, "source/repo", key, &payload).await,
        BridgeOutcome::SelfAuthored
    );
    cfg.forgejo_token.clear();
    assert_eq!(
        from_github(&gh, &cfg, &store, "source/repo", key, "issues", &json!({})).await,
        BridgeOutcome::ProviderDisabled
    );
    assert!(github.received_requests().await.unwrap().is_empty());
    assert!(forgejo.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn merge_queue_labels_are_never_imported_or_removed() {
    let github = MockServer::start().await;
    let forgejo = MockServer::start().await;
    let gh = gh(&github);
    let fj = ForgejoClient::new(&forgejo.uri(), "forgejo-token").unwrap();
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    store
        .bridge_link("scope", "pull", "forgejo", 4, "github", 93, "old", 1)
        .unwrap();
    let mut raw = object(4, true, "source/repo");
    raw["labels"] = json!([{"name":"queue: testing"},{"name":"bug"}]);
    respond(
        &forgejo,
        "GET",
        "/api/v1/repos/source/repo/pulls/4",
        raw.clone(),
    )
    .await;
    respond(
        &github,
        "GET",
        "/repos/dest/repo/issues/93/labels",
        json!([{"name":"queue: queued"},{"name":"old-label"}]),
    )
    .await;
    respond(
        &github,
        "GET",
        "/repos/dest/repo/labels",
        json!([{"name":"bug"},{"name":"queue: queued"}]),
    )
    .await;
    respond(&github, "PATCH", "/repos/dest/repo/pulls/93", json!({})).await;
    Mock::given(method("PUT"))
        .and(path("/repos/dest/repo/issues/93/labels"))
        .and(body_partial_json(json!({"labels":["bug","queue: queued"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&github)
        .await;
    assert_eq!(
        forward_item(
            &gh.crab,
            &fj,
            &store,
            "secret",
            Target::GitHub,
            "dest/repo",
            "source/repo",
            &event_item(&raw, true),
            "scope",
            &["queue: queued", "queue: testing"]
        )
        .await,
        Ok(BridgeOutcome::Synced)
    );
}

#[test]
fn inbox_restarts_deduplicates_blocks_only_the_failed_thread_and_prunes_bodies() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let payload = json!({"body":"private text"});
    assert!(store.bridge_enqueue("first", "lane-a", &payload).unwrap());
    assert!(!store.bridge_enqueue("first", "lane-a", &payload).unwrap());
    assert_eq!(
        store.bridge_claim_delivery().unwrap(),
        Some(("first".into(), payload.clone()))
    );
    store.bridge_enqueue("second", "lane-a", &payload).unwrap();
    assert!(store.bridge_claim_delivery().unwrap().is_none());
    drop(store);
    let store = Store::open(&dir.0).unwrap();
    assert_eq!(store.bridge_claim_delivery().unwrap().unwrap().0, "first");
    store
        .bridge_finish_delivery("first", false, "needs-reconciliation")
        .unwrap();
    store.bridge_enqueue("second", "lane-a", &payload).unwrap();
    store
        .bridge_enqueue("independent", "lane-b", &payload)
        .unwrap();
    assert_eq!(
        store.bridge_claim_delivery().unwrap().unwrap().0,
        "independent"
    );
    store
        .bridge_finish_delivery("independent", true, "synced")
        .unwrap();
    assert!(store.bridge_claim_delivery().unwrap().is_none());
    store.bridge_retry_delivery("first").unwrap();
    assert_eq!(store.bridge_claim_delivery().unwrap().unwrap().0, "first");
    store
        .bridge_finish_delivery("first", true, "synced")
        .unwrap();
    assert_eq!(store.bridge_claim_delivery().unwrap().unwrap().0, "second");
    drop(store);
    let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
    let text: String = db
        .query_row(
            "SELECT payload FROM forgejo_bridge_deliveries WHERE key='first'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(text, "{}");
}

#[tokio::test]
async fn admission_discards_envelope_secrets_and_keeps_distinct_forgejo_revisions() {
    let forgejo = MockServer::start().await;
    let cfg = cfg(&forgejo);
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let github = json!({"action":"opened","repository":{"id":3,"full_name":"source/repo","token":"never-store-this"},"installation":{"id":7},"sender":{"login":"alice","email":"never-store-this"},"issue":object(4,false,"source/repo")});
    assert!(inbox::enqueue_github(&store, &cfg, "issues", &github, None).unwrap());
    assert!(!inbox::enqueue_github(&store, &cfg, "issues", &github, None).unwrap());
    let (key, value) = store.bridge_claim_delivery().unwrap().unwrap();
    assert!(!value.to_string().contains("never-store-this"));
    store.bridge_finish_delivery(&key, true, "done").unwrap();
    for (title, time) in [
        ("A", "2026-11-05T01:00:00Z"),
        ("B", "2026-11-05T01:00:01Z"),
        ("A", "2026-11-05T01:00:02Z"),
    ] {
        let event = crate::forgejo::classify(
            "issues",
            &json!({"action":"edited","repository":{"id":5,"full_name":"dest/repo"},"sender":{"login":"alice"},"issue":{"number":4,"title":title,"updated_at":time}}),
        );
        assert!(inbox::enqueue_forgejo(&store, &cfg, &event, None).unwrap());
    }
    assert_eq!(store.bridge_deliveries().unwrap().len(), 3);
    // Even byte-identical edits in the same second are distinct deliveries.
    let event = crate::forgejo::classify(
        "issues",
        &json!({"action":"edited","repository":{"id":5,"full_name":"dest/repo"},"sender":{"login":"alice"},"issue":{"number":4,"title":"A","updated_at":"2026-11-05T01:00:02Z"}}),
    );
    assert!(inbox::enqueue_forgejo(&store, &cfg, &event, Some("delivery-one")).unwrap());
    assert!(inbox::enqueue_forgejo(&store, &cfg, &event, Some("delivery-two")).unwrap());
    assert!(!inbox::enqueue_forgejo(&store, &cfg, &event, Some("delivery-two")).unwrap());
    assert_eq!(store.bridge_deliveries().unwrap().len(), 5);
    assert!(forgejo.received_requests().await.unwrap().is_empty());
}

#[test]
fn classifier_and_origin_validation_cover_supported_events_only() {
    let payload = json!({"action":"label_updated","repository":{"id":8,"full_name":"dest/repo"},"sender":{"login":"alice"},"issue":object(4,false,"dest/repo"),"pull_request":object(5,true,"dest/repo")});
    assert!(matches!(
        crate::forgejo::classify("issue_label", &payload),
        ForgejoEvent::Issue { number: 4, .. }
    ));
    assert!(matches!(
        crate::forgejo::classify("pull_request_label", &payload),
        ForgejoEvent::PullRequest { number: 5, .. }
    ));
    for action in [
        "deleted",
        "assigned",
        "review_requested",
        "synchronized",
        "synchronize",
    ] {
        let mut payload = payload.clone();
        payload["action"] = json!(action);
        assert!(github_item("issues", &payload).is_none());
        assert!(forgejo_item(&crate::forgejo::classify("pull_request", &payload)).is_none());
    }
    assert!(github_comment(
        &json!({"action":"edited","issue":{"number":1},"comment":{"id":2,"body":"edited"}})
    )
    .is_none());
    for bad in [
        "http://github.com/a/b",
        "https://user:token@github.com/a/b",
        "https://github.com:9999/a/b",
        "https://github.com/a/b?token=secret",
        "https://github.com/a/b#frag",
        "ssh://git@github.com/a/b",
    ] {
        assert!(parse_github_origin(bad).is_none(), "{bad}");
    }
}
