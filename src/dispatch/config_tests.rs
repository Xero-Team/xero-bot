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
    /// Expose the controlled clock used to test diagnostic suppression without wall-clock waits.
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
    /// Build mock production ingress with idle disabled and no live GitHub credentials.
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
            .with_priority(10)
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
    /// Route a human comment carrying the fixture's target repository and installation identity.
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
    /// Execute routed comment work through the production configuration gate using mocks.
    async fn comment(&self, text: &str, number: i64) {
        let Routing::Act(work) = self.route(text, number) else {
            panic!("did not route {text}");
        };
        execute_comment_with_client(&self.gh, &self.cfg, &self.cache, work)
            .await
            .unwrap();
    }
    /// Reject session/permission reads and business writes, allowing only config reads and diagnoses.
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

/// Every supported disabled spelling and compound must stop before session lookup or action APIs.
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
        "@bot r= @bob.",
        "@bot r= @bob。",
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

/// Existing history cannot bypass disabled or justify querying the session endpoint.
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

/// A currently disabled historical command cannot qualify as a legacy session opener.
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

/// Configuration failures permit bounded status replies while suppressing every bundled command.
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

/// A comment-domain error must block execution even when other configuration domains remain valid.
#[tokio::test]
async fn invalid_comment_domain_blocks_even_when_other_domains_are_valid() {
    let f =
        Fixture::new("[command_triggers]\nclaim={mode='disabled'}\ntake={mode='no_mention'}").await;
    f.comment("@bot claim", 1).await;
    assert_eq!(f.assert_only_config_and_diagnostics().await, 1);
}

/// Non-command content must stay silent and cause no configuration HTTP reads.
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

/// The existing CodeQL label trigger must honor the same disabled veto without posting per-event replies.
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

/// Help must expose unsupported automatic rules while valid comment commands remain usable.
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

/// Syntax errors produce a diagnostic only: no configuration, commit, history,
/// permission lookup or business endpoint is consulted.
#[tokio::test]
async fn parser_errors_never_reach_github_action_or_lookup_apis() {
    let f = Fixture::new("").await;
    for (i, text) in [
        "@bot r=",
        "r+ as",
        "@bot r+ @bad_name",
        "@bot r+ @alice extra",
        "@bot assign @alice @bob",
        "@bot cc @alice about this",
        "@bot reviwe",
    ]
    .iter()
    .enumerate()
    {
        f.comment(text, i as i64 + 1).await;
    }
    let requests = f.server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 7);
    assert!(requests
        .iter()
        .all(|r| r.method == "POST" && r.url.path().ends_with("/comments")));
    assert!(requests
        .iter()
        .all(|r| !String::from_utf8_lossy(&r.body).contains("APPROVE")));
}

/// Existing symbolic shortcuts are candidates, not exemptions from the gate.
#[tokio::test]
async fn symbols_cannot_bypass_always_mention_even_in_compounds() {
    let f = Fixture::new("[command_triggers]\nready={mode='always_mention'}\n'r?'={mode='always_mention'}\ncc={mode='always_mention'}").await;
    for (i, text) in ["?r", "r? @bob", "?r @bob cc @carol", "r+", "r= @bob", "r-"]
        .iter()
        .enumerate()
    {
        f.comment(text, i as i64 + 1).await;
    }
    assert_eq!(f.assert_only_config_and_diagnostics().await, 6);
}

/// A denied duplicate must not erase the candidate with an explicit mention.
#[tokio::test]
async fn denied_bare_duplicate_does_not_erase_explicit_approval() {
    let mut f = Fixture::new("").await;
    f.cfg.merge_queue_enabled = false;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/pulls/1/commits")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{REPO}/collaborators/alice/permission"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission":"write"})))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/pulls/1/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":1})))
        .expect(1)
        .mount(&f.server)
        .await;
    let Routing::Act(mut work) = f.route("r+\n@bot r+", 1) else {
        panic!("not routed")
    };
    if let Work::Comment { pr_author, .. } = &mut work {
        *pr_author = "bob".into();
    }
    execute_comment_with_client(&f.gh, &f.cfg, &f.cache, work)
        .await
        .unwrap();
    let requests = f.server.received_requests().await.unwrap();
    assert!(!requests
        .iter()
        .any(|r| r.method == "GET" && r.url.path().ends_with("/comments")));
    let approvals: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.url.path().ends_with("/reviews"))
        .collect();
    assert_eq!(approvals.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&approvals[0].body).unwrap()["event"],
        "APPROVE"
    );
}

/// A disabled conflicting status must be removed before status resolution.
#[tokio::test]
async fn disabled_blocked_does_not_cancel_permitted_ready() {
    let f = Fixture::new("[command_triggers]\nblocked={mode='disabled'}").await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues/1/labels")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&f.server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/issues/1/labels")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&f.server)
        .await;
    let Routing::Act(mut work) = f.route("@bot ready; blocked", 1) else {
        panic!("not routed")
    };
    if let Work::Comment { is_pr, .. } = &mut work {
        *is_pr = false;
    }
    execute_comment_with_client(&f.gh, &f.cfg, &f.cache, work)
        .await
        .unwrap();
    let requests = f.server.received_requests().await.unwrap();
    let label_write = requests
        .iter()
        .find(|r| r.method == "POST" && r.url.path().ends_with("/labels"))
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&label_write.body).unwrap()["labels"],
        json!(["waiting-on-review"])
    );
}

/// History queries are shared by candidates, and bare history cannot establish
/// a session merely because the new parser now recognizes it.
#[tokio::test]
async fn bare_history_cannot_self_authorize_a_block_of_session_commands() {
    let f = Fixture::new("").await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/issues/1/comments")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"user":{"login":"alice"}, "body":"help\nping"},
            {"user":{"login":"alice"}, "body":"r? @bob"},
            {"user":{"login":"bob"}, "body":"@bot help"}
        ])))
        .expect(1)
        .mount(&f.server)
        .await;
    f.comment("ping\nhelp", 1).await;
    let requests = f.server.received_requests().await.unwrap();
    let posts: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert_eq!(posts.len(), 1);
    assert!(String::from_utf8_lossy(&posts[0].body).contains("SessionRequired"));
}
