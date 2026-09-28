use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use base64::Engine;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
use xero_bot::config::cache::*;
use xero_bot::config::repository::*;
use xero_bot::github::Client;
use xero_bot::lang::Lang;

const REPO: &str = "example/project";
const KEY: RepositoryKey = RepositoryKey {
    installation_id: 7,
    repository_id: 9,
};
#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
impl TestClock {
    fn set(&self, time: u64) {
        self.0.store(time, Ordering::SeqCst);
    }
}
#[derive(Clone)]
struct Mutable(Arc<Mutex<ResponseTemplate>>);
impl Mutable {
    fn new(response: ResponseTemplate) -> Self {
        Self(Arc::new(Mutex::new(response)))
    }
    fn set(&self, response: ResponseTemplate) {
        *self.0.lock().unwrap() = response;
    }
}
impl Respond for Mutable {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0.lock().unwrap().clone()
    }
}
fn file(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("ETag", "\"blob\"")
        .set_body_json(json!({
            "type":"file", "encoding":"base64", "sha":"blob",
            "content":base64::engine::general_purpose::STANDARD.encode(text),
        }))
}
struct Fixture {
    server: MockServer,
    gh: Arc<Client>,
    cache: Arc<RepositoryConfigCache>,
    clock: Arc<TestClock>,
    metadata: Mutable,
    head: Mutable,
    content: Mutable,
}
impl Fixture {
    async fn new(text: &str) -> Self {
        let server = MockServer::start().await;
        let gh = Arc::new(Client {
            crab: xero_bot::github::client_builder()
                .personal_token("test-token")
                .base_uri(server.uri())
                .unwrap()
                .build()
                .unwrap(),
            app_slug: "bot".into(),
        });
        let metadata = Mutable::new(
            ResponseTemplate::new(200).set_body_json(json!({"id":9,"default_branch":"main"})),
        );
        let head = Mutable::new(
            ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"commit-a"}})),
        );
        let content = Mutable::new(file(text));
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}")))
            .respond_with(metadata.clone())
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/git/ref/heads/main")))
            .respond_with(head.clone())
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/contents/{CONFIG_PATH}")))
            .and(query_param("ref", "commit-a"))
            .respond_with(content.clone())
            .mount(&server)
            .await;
        let clock = Arc::new(TestClock::default());
        let cache = Arc::new(RepositoryConfigCache::with_clock(clock.clone()));
        Self {
            server,
            gh,
            cache,
            clock,
            metadata,
            head,
            content,
        }
    }
    async fn load(&self) -> ConfigState {
        self.cache.load(&self.gh, KEY, REPO).await
    }
    async fn count(&self) -> usize {
        self.server.received_requests().await.unwrap().len()
    }
}
fn reason(state: &ConfigState) -> ReasonCode {
    state.snapshot().unwrap_err().code
}

#[tokio::test]
async fn immutable_default_branch_snapshot_and_exact_sixty_second_ttl() {
    let f = Fixture::new("[command_triggers]\nreview={mode='disabled'}").await;
    let state = f.load().await;
    let s = state.snapshot().unwrap();
    assert_eq!(s.key, KEY);
    assert_eq!(s.default_branch, "main");
    assert_eq!(s.commit_sha, "commit-a");
    assert_eq!(s.blob_sha.as_deref(), Some("blob"));
    assert_eq!(
        s.config.comments.as_ref().unwrap().mode(CommandId::Review),
        ManualMode::Disabled
    );
    assert_eq!(f.count().await, 3);
    f.clock.set(59);
    assert!(f.load().await.snapshot().is_ok());
    assert_eq!(f.count().await, 3);
    f.clock.set(60);
    assert_eq!(f.load().await.snapshot().unwrap().verified_at, 60);
    assert_eq!(f.count().await, 6);
}

#[tokio::test]
async fn concurrent_refreshes_share_one_request_sequence_and_failures_share_backoff() {
    let f = Fixture::new("").await;
    f.content.set(file("").set_delay(Duration::from_millis(20)));
    let states = futures::future::join_all((0..40).map(|_| f.load())).await;
    assert!(states.iter().all(|s| s.snapshot().is_ok()));
    assert_eq!(f.count().await, 3);
    f.clock.set(60);
    f.metadata.set(ResponseTemplate::new(503));
    let states = futures::future::join_all((0..40).map(|_| f.load())).await;
    assert!(states.iter().all(|s| reason(s) == ReasonCode::ApiFailure));
    assert_eq!(f.count().await, 4);
}

#[tokio::test]
async fn keys_include_installation_and_repository_id() {
    let f = Fixture::new("").await;
    assert!(f.load().await.snapshot().is_ok());
    let other = RepositoryKey {
        installation_id: 8,
        ..KEY
    };
    assert!(f.cache.load(&f.gh, other, REPO).await.snapshot().is_ok());
    assert_eq!(f.count().await, 6);
    let other = RepositoryKey {
        repository_id: 10,
        ..KEY
    };
    assert_eq!(
        reason(&f.cache.load(&f.gh, other, REPO).await),
        ReasonCode::InvalidResponse
    );
    assert_eq!(f.count().await, 7);
}

#[tokio::test]
async fn confirmed_absence_and_empty_file_use_defaults_but_keep_distinct_identity() {
    let f = Fixture::new("").await;
    assert_eq!(
        f.load().await.snapshot().unwrap().blob_sha.as_deref(),
        Some("blob")
    );
    f.cache.invalidate(KEY);
    f.content.set(ResponseTemplate::new(404));
    let state = f.load().await;
    let s = state.snapshot().unwrap();
    assert!(s.blob_sha.is_none());
    assert_eq!(
        s.config.comments.as_ref().unwrap().mode(CommandId::Approve),
        ManualMode::AlwaysMention
    );
    assert!(s.config.events.as_ref().unwrap().is_empty());
}

#[tokio::test]
async fn repository_or_branch_404_never_means_absent_config() {
    let f = Fixture::new("").await;
    f.metadata.set(ResponseTemplate::new(404));
    assert_eq!(reason(&f.load().await), ReasonCode::RepositoryUnavailable);
    assert_eq!(f.count().await, 1);
    f.clock.set(15);
    f.metadata
        .set(ResponseTemplate::new(200).set_body_json(json!({"id":9,"default_branch":"main"})));
    f.head.set(ResponseTemplate::new(404));
    assert_eq!(reason(&f.load().await), ReasonCode::BranchUnavailable);
    assert_eq!(f.count().await, 3);
}

#[tokio::test]
async fn unreadable_contents_dont_create_permissive_defaults_or_use_stale_snapshot() {
    let f = Fixture::new("").await;
    assert!(f.load().await.snapshot().is_ok());
    f.content.set(ResponseTemplate::new(403));
    f.clock.set(59);
    assert!(f.load().await.snapshot().is_ok());
    f.clock.set(60);
    let state = f.load().await;
    assert_eq!(reason(&state), ReasonCode::Forbidden);
    assert!(matches!(
        &state,
        ConfigState::Unavailable {
            stale_reference: Some(_),
            ..
        }
    ));
    assert!(state
        .diagnostic(Lang::En)
        .unwrap()
        .contains("expired reference"));
    assert!(state.diagnostic(Lang::Zh).unwrap().contains("过期参考"));
    f.clock.set(74);
    assert_eq!(reason(&f.load().await), ReasonCode::Forbidden);
    assert_eq!(f.count().await, 6);
    f.content
        .set(file("[command_triggers]\nreview={mode='disabled'}"));
    f.clock.set(75);
    assert_eq!(
        f.load()
            .await
            .snapshot()
            .unwrap()
            .config
            .comments
            .as_ref()
            .unwrap()
            .mode(CommandId::Review),
        ManualMode::Disabled
    );
    assert_eq!(f.count().await, 9);
}

#[tokio::test]
async fn invalid_toml_is_unavailable_and_retried_after_fifteen_seconds() {
    let f = Fixture::new("[command_triggers]\nreview={mode='disabled'}").await;
    assert!(f.load().await.snapshot().is_ok());
    f.cache.invalidate(KEY);
    f.content.set(file("["));
    assert_eq!(reason(&f.load().await), ReasonCode::InvalidDocument);
    f.content.set(file(""));
    f.clock.set(14);
    assert_eq!(reason(&f.load().await), ReasonCode::InvalidDocument);
    assert_eq!(f.count().await, 6);
    f.clock.set(15);
    assert!(f.load().await.snapshot().is_ok());
    assert_eq!(f.count().await, 9);
}

#[tokio::test]
async fn retry_after_extends_backoff_even_when_pushes_arrive() {
    let f = Fixture::new("").await;
    f.metadata
        .set(ResponseTemplate::new(429).insert_header("retry-after", "120"));
    let state = f.load().await;
    assert_eq!(reason(&state), ReasonCode::RateLimited);
    assert!(matches!(
        state,
        ConfigState::Unavailable {
            retry_after_secs: 120,
            ..
        }
    ));
    f.clock.set(119);
    f.cache.invalidate(KEY);
    assert_eq!(reason(&f.load().await), ReasonCode::RateLimited);
    assert_eq!(f.count().await, 1);
    f.clock.set(120);
    assert_eq!(reason(&f.load().await), ReasonCode::RateLimited);
    assert_eq!(f.count().await, 2);
}

#[tokio::test]
async fn retry_after_http_date_and_rate_limit_reset_are_respected() {
    for date_header in [true, false] {
        let f = Fixture::new("").await;
        let future = chrono::Utc::now() + chrono::Duration::seconds(120);
        let template = if date_header {
            ResponseTemplate::new(403).insert_header(
                "retry-after",
                future.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            )
        } else {
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", future.timestamp().to_string())
        };
        f.metadata.set(template);
        match f.load().await {
            ConfigState::Unavailable {
                problem,
                retry_after_secs,
                ..
            } => {
                assert_eq!(problem.code, ReasonCode::RateLimited);
                assert!((115..=120).contains(&retry_after_secs));
            }
            _ => panic!("rate limit was accepted"),
        }
    }
}

#[tokio::test]
async fn conditional_304_revalidates_only_the_same_immutable_representation() {
    let f = Fixture::new("[command_triggers]\nreview={mode='disabled'}").await;
    assert!(f.load().await.snapshot().is_ok());
    f.clock.set(60);
    f.content.set(ResponseTemplate::new(304));
    let state = f.load().await;
    let s = state.snapshot().unwrap();
    assert_eq!(s.verified_at, 60);
    assert_eq!(
        s.config.comments.as_ref().unwrap().mode(CommandId::Review),
        ManualMode::Disabled
    );
    let requests = f.server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .last()
            .unwrap()
            .headers
            .get("if-none-match")
            .unwrap(),
        "\"blob\""
    );
    f.clock.set(120);
    f.head
        .set(ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"commit-b"}})));
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/contents/{CONFIG_PATH}")))
        .and(query_param("ref", "commit-b"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&f.server)
        .await;
    assert_eq!(reason(&f.load().await), ReasonCode::InvalidResponse);
    assert!(!f
        .server
        .received_requests()
        .await
        .unwrap()
        .last()
        .unwrap()
        .headers
        .contains_key("if-none-match"));
}

#[tokio::test]
async fn initial_304_and_bad_file_shapes_are_failures() {
    for response in [
        ResponseTemplate::new(304),
        ResponseTemplate::new(200).set_body_json(json!([])),
        ResponseTemplate::new(200).set_body_json(json!({"type":"dir"})),
        ResponseTemplate::new(200)
            .set_body_json(json!({"type":"file","sha":"x","encoding":"none","content":""})),
        ResponseTemplate::new(200)
            .set_body_json(json!({"type":"file","sha":"x","encoding":"base64","content":"!"})),
        ResponseTemplate::new(200)
            .set_body_json(json!({"type":"file","sha":"x","encoding":"base64","content":"/w=="})),
        ResponseTemplate::new(200)
            .set_body_json(json!({"type":"file","encoding":"base64","content":""})),
        ResponseTemplate::new(200).set_body_string("invalid json"),
    ] {
        let f = Fixture::new("").await;
        f.content.set(response);
        assert_eq!(reason(&f.load().await), ReasonCode::InvalidResponse);
    }
}

#[tokio::test]
async fn push_and_observed_default_branch_change_invalidate_without_waiting_for_ttl() {
    let f = Fixture::new("").await;
    assert!(f.load().await.snapshot().is_ok());
    let mut payload = json!({"installation":{"id":7},"repository":{"id":9,"default_branch":"main"},"ref":"refs/heads/topic"});
    f.cache.observe_webhook("push", &payload);
    assert!(f.load().await.snapshot().is_ok());
    assert_eq!(f.count().await, 3);
    payload["ref"] = json!("refs/heads/main");
    f.cache.observe_webhook("push", &payload);
    assert!(f.load().await.snapshot().is_ok());
    assert_eq!(f.count().await, 6);
    f.metadata.set(
        ResponseTemplate::new(200).set_body_json(json!({"id":9,"default_branch":"release/v2"})),
    );
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/git/ref/heads/release%2Fv2")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"commit-b"}})),
        )
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/contents/{CONFIG_PATH}")))
        .and(query_param("ref", "commit-b"))
        .respond_with(file("[command_triggers]\nreview={mode='disabled'}"))
        .mount(&f.server)
        .await;
    payload["repository"]["default_branch"] = json!("release/v2");
    f.cache.observe_webhook("repository", &payload);
    let state = f.load().await;
    let s = state.snapshot().unwrap();
    assert_eq!(s.default_branch, "release/v2");
    assert_eq!(s.commit_sha, "commit-b");
    assert_eq!(
        s.config.comments.as_ref().unwrap().mode(CommandId::Review),
        ManualMode::Disabled
    );
    assert_eq!(f.count().await, 9);
}

#[tokio::test]
async fn missed_webhook_is_recovered_on_ttl_and_fork_fields_are_ignored() {
    let f = Fixture::new("").await;
    assert!(f.load().await.snapshot().is_ok());
    f.cache.observe_webhook("pull_request", &json!({"installation":{"id":7},"repository":{"id":9,"default_branch":"main"},"pull_request":{"head":{"ref":"evil","repo":{"id":99,"default_branch":"evil"}}}}));
    f.head
        .set(ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"commit-b"}})));
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/contents/{CONFIG_PATH}")))
        .and(query_param("ref", "commit-b"))
        .respond_with(file("[command_triggers]\nreview={mode='disabled'}"))
        .mount(&f.server)
        .await;
    f.clock.set(59);
    assert_eq!(f.load().await.snapshot().unwrap().commit_sha, "commit-a");
    f.clock.set(60);
    assert_eq!(f.load().await.snapshot().unwrap().commit_sha, "commit-b");
    assert_eq!(f.count().await, 6);
}

#[tokio::test]
async fn in_flight_refresh_cannot_overwrite_an_invalidation() {
    let f = Fixture::new("").await;
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/contents/{CONFIG_PATH}")))
        .respond_with(move |_: &Request| {
            signal.notify_one();
            file("").set_delay(Duration::from_millis(50))
        })
        .with_priority(1)
        .mount(&f.server)
        .await;
    let cache = f.cache.clone();
    let gh = f.gh.clone();
    let task = tokio::spawn(async move { cache.load(&gh, KEY, REPO).await });
    started.notified().await;
    f.cache.invalidate(KEY);
    assert_eq!(reason(&task.await.unwrap()), ReasonCode::Invalidated);
    assert!(f.load().await.snapshot().is_ok());
    assert_eq!(f.count().await, 6);
}

#[tokio::test]
async fn transport_failure_is_typed_and_backed_off() {
    let f = Fixture::new("").await;
    let cache = f.cache.clone();
    let gh = f.gh.clone();
    // A reachable TCP listener with no HTTP responder, closed before the request.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let gh = Client {
        crab: xero_bot::github::client_builder()
            .personal_token("test-token")
            .base_uri(format!("http://{addr}"))
            .unwrap()
            .build()
            .unwrap(),
        app_slug: gh.app_slug.clone(),
    };
    assert_eq!(
        reason(&cache.load(&gh, KEY, REPO).await),
        ReasonCode::Transport
    );
    assert_eq!(reason(&f.load().await), ReasonCode::Transport);
    assert_eq!(f.count().await, 0);
}

#[test]
fn diagnostic_budget_is_per_repository_thread_reason_and_ten_minutes() {
    let clock = Arc::new(TestClock::default());
    let cache = RepositoryConfigCache::with_clock(clock.clone());
    assert!(cache.claim_diagnostic(KEY, 1, ReasonCode::Disabled));
    assert!(!cache.claim_diagnostic(KEY, 1, ReasonCode::Disabled));
    assert!(cache.claim_diagnostic(KEY, 2, ReasonCode::Disabled));
    assert!(cache.claim_diagnostic(KEY, 1, ReasonCode::Forbidden));
    assert!(cache.claim_diagnostic(
        RepositoryKey {
            installation_id: 8,
            ..KEY
        },
        1,
        ReasonCode::Disabled
    ));
    clock.set(599);
    assert!(!cache.claim_diagnostic(KEY, 1, ReasonCode::Disabled));
    clock.set(600);
    assert!(cache.claim_diagnostic(KEY, 1, ReasonCode::Disabled));
}

#[tokio::test]
async fn request_timeout_is_typed_and_backed_off_with_a_controlled_clock() {
    let f = Fixture::new("").await;
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}")))
        .respond_with(move |_: &Request| {
            signal.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":9,"default_branch":"main"}))
                .set_delay(Duration::from_secs(60))
        })
        .with_priority(1)
        .mount(&f.server)
        .await;
    let cache = f.cache.clone();
    let gh = f.gh.clone();
    let task = tokio::spawn(async move { cache.load(&gh, KEY, REPO).await });
    started.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(16)).await;
    assert_eq!(reason(&task.await.unwrap()), ReasonCode::Transport);
    tokio::time::resume();
    assert_eq!(reason(&f.load().await), ReasonCode::Transport);
    assert_eq!(f.count().await, 1);
}

#[tokio::test]
async fn invalidation_during_a_failed_refresh_preserves_retry_after() {
    let f = Fixture::new("").await;
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}")))
        .respond_with(move |_: &Request| {
            signal.notify_one();
            ResponseTemplate::new(429)
                .insert_header("retry-after", "120")
                .set_delay(Duration::from_millis(50))
        })
        .with_priority(1)
        .mount(&f.server)
        .await;
    let cache = f.cache.clone();
    let gh = f.gh.clone();
    let task = tokio::spawn(async move { cache.load(&gh, KEY, REPO).await });
    started.notified().await;
    f.cache.invalidate(KEY);
    assert!(matches!(
        task.await.unwrap(),
        ConfigState::Unavailable {
            retry_after_secs: 120,
            ..
        }
    ));
    f.clock.set(119);
    assert_eq!(reason(&f.load().await), ReasonCode::RateLimited);
    assert_eq!(f.count().await, 1);
}
