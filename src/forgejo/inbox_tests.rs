use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Dir(std::path::PathBuf);
impl Dir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "xero-forgejo-inbox-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn cfg(url: &str) -> Config {
    let mut cfg = Config::from_env();
    cfg.bot_name = "bot".into();
    cfg.forgejo_base_url = url.into();
    cfg.forgejo_token = "test-token".into();
    cfg.forgejo_webhook_secret = "test-secret".into();
    cfg
}
fn event(id: i64, text: &str) -> ForgejoEvent {
    ForgejoEvent::IssueComment {
        repo: "team/repo".into(),
        number: 7,
        repository_id: Some(1),
        issue_id: Some(70),
        comment_id: Some(id),
        user_id: Some(20),
        source_at: Some((chrono::Utc::now() - chrono::Duration::seconds(1000 - id)).to_rfc3339()),
        body: text.into(),
        commenter: "alice".into(),
        author: "bob".into(),
        is_pr: true,
    }
}
async fn policy(server: &MockServer, text: &str) {
    use base64::Engine;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login":"bot"})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"default_branch":"main"})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo/branches/main"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"commit":{"id":"a".repeat(40)}})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/contents/.github/xero-bot.toml"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type":"file","encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode(text)}))).mount(server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/team/repo/issues/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":999})))
        .mount(server)
        .await;
}

#[test]
fn audit_inbox_is_durable_deduplicated_and_fails_on_storage_errors() {
    let dir = Dir::new();
    let cfg = cfg("https://forgejo.example");
    let job = event(10, "@bot ping");
    let inbox = Inbox::open(&dir.0).unwrap();
    assert!(inbox.enqueue(&cfg, &job).unwrap());
    drop(inbox);
    let inbox = Inbox::open(&dir.0).unwrap();
    assert!(!inbox.enqueue(&cfg, &job).unwrap());
    let mut equivalent = cfg.clone();
    equivalent.forgejo_base_url = "https://FORGEJO.example/api/v1/".into();
    assert!(!inbox.enqueue(&equivalent, &job).unwrap());
    assert_eq!(inbox.status().unwrap().len(), 1);
    inbox.db().unwrap().execute_batch("CREATE TRIGGER disk_failure BEFORE INSERT ON comments BEGIN SELECT RAISE(ABORT,'disk failure');END;").unwrap();
    assert!(inbox.enqueue(&cfg, &event(11, "@bot ping")).is_err());
    assert_eq!(inbox.status().unwrap().len(), 1);
}

#[tokio::test]
async fn audit_queued_wakes_precede_bare_commands_and_duplicates_have_no_writes() {
    let server = MockServer::start().await;
    policy(&server, "[command_triggers]\nping={mode='mention_once'}").await;
    let dir = Dir::new();
    let cfg = cfg(&server.uri());
    let store = Store::open(&dir.0).unwrap();
    let inbox = Arc::new(Inbox::open(&dir.0).unwrap());
    let later = event(20, "ping");
    let earlier = event(10, "@bot ping");
    inbox.enqueue(&cfg, &later).unwrap();
    inbox.enqueue(&cfg, &earlier).unwrap();
    assert_eq!(inbox.pump(&cfg, &store).await.unwrap(), 2);
    assert!(inbox
        .status()
        .unwrap()
        .iter()
        .all(|s| s["state"] == "succeeded"));
    assert!(!inbox.enqueue(&cfg, &earlier).unwrap());
    assert_eq!(inbox.pump(&cfg, &store).await.unwrap(), 0);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.iter().filter(|r| r.method == "POST").count(), 2);
}

#[tokio::test]
async fn audit_bare_command_waits_for_a_delayed_wake_without_a_false_refusal() {
    let server = MockServer::start().await;
    policy(&server, "[command_triggers]\nping={mode='mention_once'}").await;
    let dir = Dir::new();
    let cfg = cfg(&server.uri());
    let store = Store::open(&dir.0).unwrap();
    let inbox = Arc::new(Inbox::open(&dir.0).unwrap());
    inbox.enqueue(&cfg, &event(20, "ping")).unwrap();
    assert_eq!(inbox.pump(&cfg, &store).await.unwrap(), 1);
    assert_eq!(inbox.status().unwrap()[0]["state"], "pending");
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
    inbox.enqueue(&cfg, &event(10, "@bot ping")).unwrap();
    inbox.pump(&cfg, &store).await.unwrap();
    inbox
        .db()
        .unwrap()
        .execute("UPDATE comments SET next_at=0 WHERE state='pending'", [])
        .unwrap();
    inbox.pump(&cfg, &store).await.unwrap();
    assert!(inbox
        .status()
        .unwrap()
        .iter()
        .all(|s| s["state"] == "succeeded"));
}

#[tokio::test]
async fn audit_concurrent_claims_preserve_both_assignees() {
    let server = MockServer::start().await;
    policy(&server, "").await;
    let members = Arc::new(Mutex::new(Vec::<String>::new()));
    let read = Arc::clone(&members);
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/issues/7"))
        .respond_with(move |_:&wiremock::Request|ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(20)).set_body_json(json!({"assignees":read.lock().unwrap().iter().map(|s|json!({"login":s})).collect::<Vec<_>>()}))).mount(&server).await;
    let write = Arc::clone(&members);
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/team/repo/issues/7"))
        .respond_with(move |r: &wiremock::Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            let mut users = write.lock().unwrap();
            *users = b["assignees"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            ResponseTemplate::new(200).set_body_json(
                json!({"assignees":users.iter().map(|s|json!({"login":s})).collect::<Vec<_>>()}),
            )
        })
        .mount(&server)
        .await;
    let dir = Dir::new();
    let cfg = cfg(&server.uri());
    let store = Store::open(&dir.0).unwrap();
    let inbox = Arc::new(Inbox::open(&dir.0).unwrap());
    let mut second = event(20, "claim");
    if let ForgejoEvent::IssueComment {
        commenter, user_id, ..
    } = &mut second
    {
        *commenter = "carol".into();
        *user_id = Some(21);
    }
    inbox.enqueue(&cfg, &second).unwrap();
    inbox.enqueue(&cfg, &event(10, "claim")).unwrap();
    let (a, b) = tokio::join!(inbox.pump(&cfg, &store), inbox.pump(&cfg, &store));
    assert_eq!(a.unwrap() + b.unwrap(), 2);
    assert_eq!(*members.lock().unwrap(), vec!["alice", "carol"]);
}

#[tokio::test]
async fn audit_late_old_status_cannot_overwrite_newer_status() {
    let server = MockServer::start().await;
    policy(&server, "").await;
    let cfg = cfg(&server.uri());
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id":1,"name":cfg.label_waiting_review},{"id":2,"name":cfg.label_waiting_author},{"id":3,"name":cfg.label_blocked}
        ]))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/team/repo/issues/7/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(2)
        .mount(&server)
        .await;
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let inbox = Arc::new(Inbox::open(&dir.0).unwrap());
    inbox.enqueue(&cfg, &event(20, "@bot ready")).unwrap();
    inbox.pump(&cfg, &store).await.unwrap();
    inbox.enqueue(&cfg, &event(10, "@bot blocked")).unwrap();
    inbox.pump(&cfg, &store).await.unwrap();
    assert!(inbox
        .status()
        .unwrap()
        .iter()
        .all(|s| s["state"] == "succeeded"));
}

#[tokio::test]
async fn audit_interrupted_review_is_retained_without_replay_after_restart() {
    let server = MockServer::start().await;
    policy(&server, "").await;
    Mock::given(method("GET"))
        .and(path(
            "/api/v1/repos/team/repo/collaborators/alice/permission",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission":"write"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/team/repo/pulls/7/reviews"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":8}))
                .set_delay(std::time::Duration::from_secs(1)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = Dir::new();
    let cfg = cfg(&server.uri());
    let store = Store::open(&dir.0).unwrap();
    let inbox = Arc::new(Inbox::open(&dir.0).unwrap());
    let job = event(10, "@bot r+");
    inbox.enqueue(&cfg, &job).unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(200),
        inbox.pump(&cfg, &store)
    )
    .await
    .is_err());
    drop(inbox);
    let inbox = Arc::new(Inbox::open(&dir.0).unwrap());
    assert_eq!(inbox.status().unwrap()[0]["state"], "unknown");
    assert!(!inbox.enqueue(&cfg, &job).unwrap());
    assert_eq!(inbox.pump(&cfg, &store).await.unwrap(), 0);
    let key = inbox.status().unwrap()[0]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(inbox.resolve(&key, "done", "").is_err());
    inbox
        .resolve(&key, "done", "review 8 verified in Forgejo")
        .unwrap();
    assert_eq!(inbox.status().unwrap()[0]["state"], "succeeded");
}
