//! Exercise the production ingress with an injected installation client. A
//! blocked command may only read config and send a bounded diagnostic.
use super::*;
use crate::config::cache::Clock;
use crate::config::repository::{CommandId, ReasonCode, CONFIG_PATH};
use base64::Engine;
use serde_json::json;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPO: &str = "example/project";
#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
struct Fixture {
    server: MockServer,
    gh: Client,
    cfg: Config,
    cache: RepositoryConfigCache,
    clock: Arc<TestClock>,
}
impl Fixture {
    async fn new(text: &str) -> Self {
        let server = MockServer::start().await;
        let gh = Client {
            crab: crate::github::client_builder()
                .personal_token("test-token")
                .base_uri(server.uri())
                .unwrap()
                .build()
                .unwrap(),
            app_slug: "bot".into(),
        };
        let mut cfg = Config::from_env();
        cfg.bot_name = "bot".into();
        cfg.app_id = "1".into();
        cfg.codeql_label = "codeql".into();
        cfg.idle_workflows_enabled = false;
        let clock = Arc::new(TestClock::default());
        let cache = RepositoryConfigCache::with_clock(clock.clone());
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id":9,"default_branch":"main"})),
            )
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/git/ref/heads/main")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"commit"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path(format!("/repos/{REPO}/contents/{CONFIG_PATH}"))).and(query_param("ref", "commit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sha":"blob", "type":"file", "encoding":"base64", "content":base64::engine::general_purpose::STANDARD.encode(text)})))
            .mount(&server).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":99})))
            .mount(&server)
            .await;
        Self {
            server,
            gh,
            cfg,
            cache,
            clock,
        }
    }
    fn route(&self, text: &str, number: i64) -> Routing {
        route_event(
            &self.cfg,
            "issue_comment",
            &json!({
                "action":"created", "installation":{"id":7}, "repository":{"id":9,"full_name":REPO},
                "issue":{"number":number,"pull_request":{"url":"pr"},"user":{"login":"alice"}},
                "comment":{"body":text,"user":{"login":"alice","type":"User"}}
            }),
        )
    }
    async fn comment(&self, text: &str, number: i64) {
        let Routing::Act(work) = self.route(text, number) else {
            panic!("did not route {text}");
        };
        execute_comment_with_client(&self.gh, &self.cfg, &self.cache, work)
            .await
            .unwrap();
    }
    async fn assert_only_config_and_diagnostics(&self) -> usize {
        let requests = self.server.received_requests().await.unwrap();
        let mut posts = 0;
        for r in requests {
            let path = r.url.path();
            if r.method == "GET" {
                assert!(
                    [
                        format!("/repos/{REPO}"),
                        format!("/repos/{REPO}/git/ref/heads/main"),
                        format!("/repos/{REPO}/contents/{CONFIG_PATH}")
                    ]
                    .contains(&path.to_string()),
                    "unexpected read: {path}"
                );
            } else {
                assert_eq!(r.method, "POST");
                assert!(path.ends_with("/comments"), "unexpected action: {path}");
                let body: Value = serde_json::from_slice(&r.body).unwrap();
                let text = body["body"].as_str().unwrap();
                assert!(
                    text.contains("configuration") || text.contains("配置"),
                    "executed a command: {text}"
                );
                posts += 1;
            }
        }
        posts
    }
}

#[tokio::test]
async fn disabled_all_aliases_and_compounds_never_query_sessions_or_call_actions() {
    let settings = CommandId::ALL
        .iter()
        .map(|id| format!("'{}'={{mode='disabled'}}\n", id.name()))
        .collect::<String>();
    let f = Fixture::new(&format!("[command_triggers]\n{settings}")).await;
    let mut number = 1;
    for id in CommandId::ALL {
        for alias in id.aliases() {
            let args = if *alias == "r=" {
                " @bob"
            } else {
                match id {
                    CommandId::RequestReview | CommandId::Cc | CommandId::Assign => " @bob",
                    CommandId::Label => " +triage",
                    _ => "",
                }
            };
            f.comment(&format!("@bot {alias}{args}"), number).await;
            number += 1;
        }
    }
    for text in [
        "review",
        "codeql",
        "ready",
        "author",
        "blocked",
        "ping",
        "help",
        "r+",
        "r-",
        "r? @bob",
        "?r",
        "@bot take; review; cc @bob",
        "@bot r+ as @bob",
        "@bot r+ @bob",
    ] {
        f.comment(text, number).await;
        number += 1;
    }
    assert_eq!(
        f.assert_only_config_and_diagnostics().await,
        (number - 1) as usize
    );
    assert!(!f.cfg.idle_workflows_enabled);
}

#[tokio::test]
async fn disabled_bare_command_cannot_use_even_an_existing_session() {
    let f = Fixture::new("[command_triggers]\nreview={mode='disabled'}").await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues/1/comments")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"user":{"login":"alice"},"body":"@bot help"}])),
        )
        .expect(0)
        .mount(&f.server)
        .await;
    f.comment("review", 1).await;
    assert_eq!(f.assert_only_config_and_diagnostics().await, 1);
}

#[tokio::test]
async fn disabled_history_does_not_open_a_legacy_session_for_another_command() {
    let f = Fixture::new("[command_triggers]\nhelp={mode='disabled'}").await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues/1/comments")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"user":{"login":"alice"},"body":"@bot help"}])),
        )
        .mount(&f.server)
        .await;
    let state = f
        .cache
        .load(
            &f.gh,
            RepositoryKey {
                installation_id: 7,
                repository_id: 9,
            },
            REPO,
        )
        .await;
    let policy = state.snapshot().unwrap().config.comments.as_ref().unwrap();
    assert!(!session_open(&f.gh, &f.cfg, REPO, 1, "alice", policy)
        .await
        .unwrap());
}

#[tokio::test]
async fn fault_help_ping_and_mixed_commands_only_receive_status_and_obey_budget() {
    let f = Fixture::new("").await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}")))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&f.server)
        .await;
    for text in ["@bot help; claim", "@bot ping; review", "@bot claim"] {
        f.comment(text, 1).await;
    }
    assert_eq!(f.assert_only_config_and_diagnostics().await, 1);
    f.comment("@bot help", 2).await;
    assert_eq!(f.assert_only_config_and_diagnostics().await, 2);
    f.clock.0.store(600, Ordering::SeqCst);
    f.comment("@bot help", 1).await;
    assert_eq!(f.assert_only_config_and_diagnostics().await, 3);
}

#[tokio::test]
async fn invalid_comment_domain_blocks_even_when_other_domains_are_valid() {
    let f =
        Fixture::new("[command_triggers]\nclaim={mode='disabled'}\ntake={mode='no_mention'}").await;
    f.comment("@bot claim", 1).await;
    assert_eq!(f.assert_only_config_and_diagnostics().await, 1);
}

#[tokio::test]
async fn ordinary_chat_code_and_quotes_never_load_config() {
    let f = Fixture::new("").await;
    for text in [
        "hello world",
        "claim 是什么意思？",
        "cc @alice about this",
        "`@bot review`",
        "> @bot review",
        "```\n@bot claim\n```",
    ] {
        assert!(matches!(f.route(text, 1), Routing::Respond(_)), "{text}");
    }
    assert!(f.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn disabled_codeql_label_event_cannot_bypass_configuration() {
    let f = Fixture::new("[command_triggers]\ncodeql={mode='disabled'}").await;
    let route = route_event(
        &f.cfg,
        "pull_request",
        &json!({
            "action":"labeled", "installation":{"id":7},"repository":{"id":9,"full_name":REPO},
            "pull_request":{"number":1},"label":{"name":"codeql"}
        }),
    );
    let Routing::Act(work @ Work::Codeql { .. }) = route else {
        panic!("not a codeql event");
    };
    let err = execute_codeql_with_client(&f.gh, &f.cfg, &f.cache, work)
        .await
        .unwrap_err();
    assert!(err.contains(&format!("{:?}", ReasonCode::Disabled)));
    assert_eq!(f.assert_only_config_and_diagnostics().await, 0);
}

#[tokio::test]
async fn unsupported_event_rules_are_reported_by_help_without_disabling_comments() {
    let f = Fixture::new(
        "[[event_triggers]]\nid='review-on-open'\nevent='pull_request.opened'\ncommand='review'",
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/pulls/1/commits")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&f.server)
        .await;
    f.comment("@bot help", 1).await;
    let bodies: Vec<_> = f
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["body"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies.iter().any(|b| b.contains("Unsupported")));
    assert!(bodies.iter().any(|b| b.contains("xero-bot commands")));
}
