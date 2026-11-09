use super::*;
use base64::Engine;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cfg() -> Config {
    let mut cfg = Config::from_env();
    cfg.bot_name = "github-bot".into();
    cfg.forgejo_bot_name = "forgejo-bot".into();
    cfg.rollup_enabled = true;
    cfg.r_plus_allow_on_behalf = false;
    cfg.bot_access_policy = json!({
        "github": [{"installation_id":2,"owner_id":3,"repository_id":1,"repository":"team/repo"}],
        "forgejo": [{"owner_id":3,"repository_id":4,"repository":"team/repo"}]
    })
    .to_string();
    cfg
}
fn project(text: &str) -> ProjectPolicy {
    ProjectPolicy {
        config: RepositoryConfig::parse(text, "team/repo").unwrap(),
        source: Source {
            path: CONFIG_PATH,
            default_branch: "main".into(),
            commit: "a".repeat(40),
            file_present: true,
            cache_seconds: 60,
        },
    }
}
fn command<'a>(data: &'a Value, name: &str) -> &'a Value {
    data["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["command"] == name)
        .unwrap()
}
fn gh(server: &MockServer) -> Client {
    Client {
        crab: crate::github::client_builder()
            .base_uri(server.uri())
            .unwrap()
            .personal_token("test")
            .build()
            .unwrap(),
        app_slug: "bot".into(),
    }
}
fn key() -> RepositoryKey {
    RepositoryKey {
        installation_id: 2,
        repository_id: 1,
    }
}
async fn github_server(text: Option<&str>, private: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/repos/team/repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":1,"full_name":"team/repo","owner":{"id":3},"private":private,"archived":false,"default_branch":"main"})))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/repos/team/repo/git/ref/heads/main"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object":{"sha":"a".repeat(40)}})),
        )
        .mount(&server)
        .await;
    let response = match text {
        Some(text) => ResponseTemplate::new(200).set_body_json(json!({"type":"file","encoding":"base64","sha":"b".repeat(40),"content":base64::engine::general_purpose::STANDARD.encode(text)})),
        None => ResponseTemplate::new(404),
    };
    Mock::given(method("GET"))
        .and(path("/repos/team/repo/contents/.github/xero-bot.toml"))
        .and(query_param("ref", "a".repeat(40)))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

#[test]
fn every_registered_alias_and_documented_example_is_parsed_and_rendered() {
    let cfg = cfg();
    for lang in [Lang::En, Lang::Zh] {
        let data = payload(&cfg, lang, None);
        let html = table(&cfg, lang, None);
        assert_eq!(
            data["commands"].as_array().unwrap().len(),
            CommandId::ALL.len()
        );
        for id in CommandId::ALL {
            let row = command(&data, id.name());
            assert_eq!(row["mode"], id.default_mode().name());
            for alias in id.aliases() {
                assert!(
                    html.contains(&format!("<code>{}</code>", escape(alias))),
                    "missing alias: {alias}"
                );
            }
            for example in id.examples() {
                let parsed = crate::commands::parse_commands(
                    "github-bot",
                    &format!("@github-bot {example}"),
                );
                assert!(
                    parsed.diagnostics.is_empty(),
                    "{example}: {:?}",
                    parsed.diagnostics
                );
                assert!(
                    parsed.commands.iter().any(|c| c.id() == id),
                    "example {example} not parsed as {id:?}"
                );
                assert!(
                    html.contains(&escape(example)),
                    "missing example: {example}"
                );
            }
        }
    }
    assert!(instructions(&cfg, Lang::Zh, 30, None).contains("@user 是目标用户"));
}

#[test]
fn project_modes_aliases_ttl_and_deployment_limits_are_effective() {
    let cfg = cfg();
    let p = project("[command_triggers]\ntake={mode='always_mention'}\nhelp={mode='no_mention'}\nreview={mode='disabled'}\n[command_sessions]\nttl_days=7");
    let data = payload(&cfg, Lang::Zh, Some(("team/repo", Provider::Github, &p)));
    assert_eq!(data["session_ttl_days"], 7);
    assert_eq!(command(&data, "claim")["mode"], "always_mention");
    assert_eq!(command(&data, "help")["mention"], "无需 @");
    assert_eq!(command(&data, "review")["available"], false);
    assert_eq!(command(&data, "rollup")["available"], false);
    assert_eq!(command(&data, "r+")["available"], true);
    let approve = command(&data, "r+")["examples"].as_array().unwrap();
    assert_eq!(
        approve.iter().find(|e| e["syntax"] == "r= @user").unwrap()["available"],
        false
    );
    assert_eq!(
        approve.iter().find(|e| e["syntax"] == "r+ rollup").unwrap()["available"],
        false
    );
    let html = project_body(&cfg, "team/repo", Provider::Github, &p, Lang::Zh);
    assert!(html.contains("7 天"));
    assert!(html.contains("项目已禁用"));
    assert!(html.contains("@github-bot take"));
    assert!(html.contains("<code>help</code>"));
    // A second repository must not inherit the first repository's overrides.
    let other = project("");
    let other = payload(
        &cfg,
        Lang::En,
        Some(("team/other", Provider::Github, &other)),
    );
    assert_eq!(command(&other, "claim")["mode"], "no_mention");
    assert_eq!(command(&other, "review")["available"], true);
}

#[test]
fn compound_examples_respect_each_subcommands_project_mode() {
    let cfg = cfg();
    let p = project("[command_triggers]\n'r?'={mode='always_mention'}\ncc={mode='disabled'}");
    let data = payload(&cfg, Lang::Zh, Some(("team/repo", Provider::Github, &p)));
    let ready = command(&data, "ready");
    assert_eq!(ready["mode"], "no_mention");
    let examples = ready["examples"].as_array().unwrap();
    let request = examples.iter().find(|e| e["syntax"] == "?r @user").unwrap();
    assert_eq!(request["invocation"], "@github-bot ?r @user");
    assert_eq!(request["mention"], "每次都需要 @");
    let cc = examples
        .iter()
        .find(|e| e["syntax"] == "?r @user cc @other")
        .unwrap();
    assert_eq!(cc["available"], false);
}

#[test]
fn forgejo_support_and_rollup_variants_follow_runtime_switches() {
    let mut cfg = cfg();
    cfg.r_plus_allow_on_behalf = true;
    let p = project(
        "[rollup]\nenabled=true\nrequired_checks=['CI']\nforgejo_required_checks=['ci.yml']",
    );
    let data = payload(&cfg, Lang::En, Some(("team/repo", Provider::Forgejo, &p)));
    for name in ["review", "codeql"] {
        assert_eq!(command(&data, name)["available"], false);
    }
    for name in ["queue", "rollup", "try", "retry"] {
        assert_eq!(command(&data, name)["available"], true);
    }
    assert!(command(&data, "r+")["examples"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["available"] == true));
    assert_eq!(
        command(&data, "try")["examples"][0]["invocation"],
        "@forgejo-bot try"
    );
    cfg.rollup_enabled = false;
    let data = payload(&cfg, Lang::En, Some(("team/repo", Provider::Forgejo, &p)));
    for name in ["queue", "rollup", "try", "retry"] {
        assert_eq!(command(&data, name)["available"], false);
    }
    assert_eq!(command(&data, "r+")["available"], true);
}

#[tokio::test]
async fn github_loads_current_policy_without_any_queue_record_and_fails_closed() {
    let cfg = cfg();
    let cache = RepositoryConfigCache::default();
    let server = github_server(Some("[command_triggers]\nhelp={mode='disabled'}"), false).await;
    let gh = gh(&server);
    let p = github_policy(&cfg, &gh, &cache, key(), "team/repo")
        .await
        .unwrap();
    assert_eq!(
        p.config.comments.unwrap().mode(CommandId::Help),
        ManualMode::Disabled
    );
    assert_eq!(p.source.commit, "a".repeat(40));
    assert!(p.source.file_present);
    cache.invalidate(key());
    Mock::given(method("GET"))
        .and(path("/repos/team/repo/contents/.github/xero-bot.toml"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(matches!(
        github_policy(&cfg, &gh, &cache, key(), "team/repo").await,
        Err(LoadError::Unavailable)
    ));
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[tokio::test]
async fn missing_config_uses_defaults_but_invalid_comment_policy_does_not() {
    for (text, ok) in [
        (None, true),
        (Some("broken = ["), false),
        (Some("[command_triggers]\nhelp={mode='typo'}"), false),
    ] {
        let server = github_server(text, false).await;
        let result = github_policy(
            &cfg(),
            &gh(&server),
            &RepositoryConfigCache::default(),
            key(),
            "team/repo",
        )
        .await;
        if ok {
            let p = result.unwrap();
            assert!(!p.source.file_present);
            assert_eq!(
                p.config.comments.unwrap().mode(CommandId::Help),
                CommandId::Help.default_mode()
            );
        } else {
            assert!(matches!(result, Err(LoadError::Unavailable)));
        }
    }
}

#[tokio::test]
async fn private_unadmitted_and_replaced_repositories_never_expose_policy() {
    let cfg = cfg();
    let server = github_server(None, true).await;
    let cache = RepositoryConfigCache::default();
    assert!(matches!(
        github_policy(&cfg, &gh(&server), &cache, key(), "team/repo").await,
        Err(LoadError::NotFound)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(matches!(
        github_policy(&cfg, &gh(&server), &cache, key(), "team/unadmitted").await,
        Err(LoadError::NotFound)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let server = github_server(Some(""), false).await;
    Mock::given(method("GET")).and(path("/repos/team/repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":99,"full_name":"team/repo","owner":{"id":3},"private":false,"archived":false})))
        .with_priority(1)
        .mount(&server).await;
    assert!(matches!(
        github_policy(&cfg, &gh(&server), &cache, key(), "team/repo").await,
        Err(LoadError::NotFound)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn metadata_outages_are_unavailable_not_default_policy() {
    let server = github_server(None, false).await;
    Mock::given(method("GET"))
        .and(path("/repos/team/repo"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&server)
        .await;
    assert!(matches!(
        github_policy(
            &cfg(),
            &gh(&server),
            &RepositoryConfigCache::default(),
            key(),
            "team/repo"
        )
        .await,
        Err(LoadError::Unavailable)
    ));
}

#[tokio::test]
async fn forgejo_reads_its_own_pinned_policy_and_bot_name() {
    let cfg = cfg();
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":4,"full_name":"team/repo","owner":{"id":3},"private":false,"archived":false,"default_branch":"forgejo-main"})))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo/branches/forgejo-main"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"commit":{"id":"f".repeat(40)}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/contents/.github/xero-bot.toml"))
        .and(query_param("ref", "f".repeat(40)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type":"file","encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode("[command_triggers]\nclaim={mode='always_mention'}")})))
        .mount(&server).await;
    let fj = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let p = forgejo_policy(&cfg, &fj, "team/repo").await.unwrap();
    let data = payload(&cfg, Lang::En, Some(("team/repo", Provider::Forgejo, &p)));
    assert_eq!(data["source"]["commit"], "f".repeat(40));
    assert_eq!(data["source"]["default_branch"], "forgejo-main");
    assert_eq!(
        command(&data, "claim")["examples"][0]["invocation"],
        "@forgejo-bot claim"
    );
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[tokio::test]
async fn pages_escape_metadata_and_keep_language_and_provider_in_navigation() {
    use http_body_util::BodyExt;
    let mut cfg = cfg();
    cfg.forgejo_bot_name = "<img src=x onerror=alert(1)>".into();
    let mut p = project("");
    p.source.default_branch = "<script>bad</script>".into();
    let body = project_body(&cfg, "team/repo", Provider::Forgejo, &p, Lang::Zh);
    let response = page(
        "Project commands",
        &body,
        &project_path("team/repo", Provider::Forgejo),
        Lang::Zh,
    )
    .into_response();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(response
        .headers()
        .contains_key(header::CONTENT_SECURITY_POLICY));
    let html = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(!html.contains("<img"));
    assert!(!html.contains("<script>bad"));
    assert!(html.contains("provider=forgejo&amp;lang=en"));
    assert!(html.contains("provider=forgejo&amp;lang=zh"));
    assert!(html.contains("&lt;script&gt;bad&lt;/script&gt;"));
}

#[tokio::test]
async fn public_routes_link_projects_redirect_lookup_and_do_not_fallback_on_errors() {
    let dir = crate::forgejo::refs::Scratch::new().unwrap();
    let state = WebState {
        cfg: cfg(),
        store: std::sync::Arc::new(crate::trigger_state::Store::open(&dir.0).unwrap()),
    };
    let app: axum::Router = super::super::routes(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let defaults = client
        .get(format!("{url}/commands?lang=zh"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(defaults.contains("完整默认参考"));
    assert!(defaults.contains("release-assignment"));
    assert!(defaults.contains("每次都需要 @"));
    assert!(defaults.contains("data-project-lookup"));
    assert!(defaults.contains("/assets/projects.js"));
    assert!(defaults.contains("<select name=\"project\" required disabled>"));
    let forgejo_defaults = client
        .get(format!("{url}/commands?provider=forgejo&lang=zh"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(forgejo_defaults.contains("value=\"forgejo\" selected"));
    assert!(forgejo_defaults.contains("provider=forgejo&amp;lang=en"));
    let lookup = client
        .get(format!(
            "{url}/commands?project=team%2Frepo&provider=forgejo&lang=zh"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(lookup.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        lookup.headers()[header::LOCATION],
        "/commands/team/repo?provider=forgejo&lang=zh"
    );
    let queue = client
        .get(format!("{url}/queue/team/repo?lang=zh"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(queue.contains("/commands/team/repo?lang=zh"));
    for endpoint in [
        "/commands/team/unknown?lang=zh",
        "/api/commands/team/unknown?lang=zh",
    ] {
        let response = client.get(format!("{url}{endpoint}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(!response
            .text()
            .await
            .unwrap()
            .contains("release-assignment"));
    }
    server.abort();
}
