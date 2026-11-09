use super::*;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cfg() -> Config {
    let mut cfg = Config::from_env();
    cfg.bot_access_policy = json!({"github":[
        {"installation_id":7,"owner_id":11,"repository_id":21,"repository":"team/one"},
        {"installation_id":8,"owner_id":12,"repository_id":22,"repository":"elsewhere/two"}
    ],"forgejo":[{"owner_id":31,"repository_id":41,"repository":"mirror/one"}]})
    .to_string();
    cfg
}
fn gh(server: &MockServer) -> Client {
    Client {
        crab: crate::github::client_builder()
            .personal_token("test-token")
            .base_uri(server.uri())
            .unwrap()
            .build()
            .unwrap(),
        app_slug: "bot".into(),
    }
}
fn meta() -> Value {
    json!({"id":21,"full_name":"team/one","owner":{"id":11}})
}

#[test]
fn malformed_ambiguous_and_wildcard_policies_fail_closed() {
    for raw in [
        "null",
        "[]",
        "{",
        r#"{"githbu":[]}"#,
        r#"{"github":[{"installation_id":7,"owner_id":11,"repository_id":0,"repository":"team/one"}]}"#,
        r#"{"github":[{"installation_id":7,"owner_id":11,"repository_id":21,"repository":"team/*"}]}"#,
        r#"{"forgejo":[{"owner_id":31,"repository_id":41,"repository":"mirror/one","enabled":true}]}"#,
        r#"{"github":[],"github":[]}"#,
    ] {
        assert!(Policy::parse(raw).is_err(), "{raw}");
        let mut cfg = cfg();
        cfg.bot_access_policy = raw.into();
        assert!(!github_webhook(
            &cfg,
            &json!({"installation":{"id":7},"repository":meta()})
        ));
        assert!(cfg.validate().unwrap_err().contains("BOT_ACCESS_POLICY"));
    }
    let mut raw: Value = serde_json::from_str(&cfg().bot_access_policy).unwrap();
    raw["github"][1] = raw["github"][0].clone();
    assert!(Policy::parse(&raw.to_string()).is_err());
    raw["github"][1]["repository"] = json!("another/name");
    assert!(Policy::parse(&raw.to_string()).is_err());
    raw["github"][1]["repository_id"] = json!(99);
    raw["github"][1]["owner_id"] = json!(99);
    assert!(Policy::parse(&raw.to_string()).is_err());
}
#[test]
fn complete_provider_identity_is_required_not_independent_id_lists() {
    let cfg = cfg();
    assert!(github_metadata(&cfg, 7, &meta()));
    let mut renamed = meta();
    renamed["full_name"] = json!("TEAM/ONE");
    assert!(github_metadata(&cfg, 7, &renamed));
    for (installation, value) in [
        (8, meta()),
        (0, meta()),
        (7, json!({"id":22,"full_name":"team/one","owner":{"id":11}})),
        (7, json!({"id":21,"full_name":"team/one","owner":{"id":12}})),
        (
            7,
            json!({"id":21,"full_name":"elsewhere/two","owner":{"id":11}}),
        ),
        (7, json!({"id":21,"full_name":"team/one"})),
    ] {
        assert!(!github_metadata(&cfg, installation, &value));
    }
    assert!(!forgejo_metadata(&cfg, &meta()));
    assert!(forgejo_metadata(
        &cfg,
        &json!({"id":41,"full_name":"mirror/one","owner":{"id":31}})
    ));
}
#[tokio::test]
async fn empty_or_revoked_policy_prevents_client_and_token_creation() {
    let server = MockServer::start().await;
    let gh = gh(&server);
    let mut cfg = cfg();
    cfg.private_key_pem = None;
    for policy in ["", "{}", "{\"github\":[],\"forgejo\":[]}"] {
        cfg.bot_access_policy = policy.into();
        assert!(matches!(
            Client::installation(&cfg, 7, "bot"),
            Err(GhError::Api { status: 403, .. })
        ));
        assert!(matches!(
            Client::installation_resolved(&cfg, 7).await,
            Err(GhError::Api { status: 403, .. })
        ));
        assert!(matches!(
            gh.repository_token(&cfg, 7, "team/one").await,
            Err(GhError::Api { status: 403, .. })
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn replay_checks_live_identity_and_rejects_transfers_and_recreation() {
    let server = MockServer::start().await;
    let gh = gh(&server);
    let cfg = cfg();
    assert!(github_repository(&cfg, &gh, 7, "team/two", Some(21))
        .await
        .is_err());
    assert!(github_repository(&cfg, &gh, 8, "team/one", Some(21))
        .await
        .is_err());
    assert!(github_repository(&cfg, &gh, 7, "team/one", Some(22))
        .await
        .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
    for value in [
        meta(),
        json!({"id":21,"full_name":"team/one","owner":{"id":99}}),
        json!({"id":99,"full_name":"team/one","owner":{"id":11}}),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/repos/team/one"))
            .respond_with(ResponseTemplate::new(200).set_body_json(value.clone()))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            github_repository(&cfg, &gh, 7, "team/one", Some(21))
                .await
                .is_ok(),
            value == meta()
        );
    }
}
#[tokio::test]
async fn discovery_and_git_tokens_are_limited_to_admitted_repositories() {
    let server = MockServer::start().await;
    let gh = gh(&server);
    let cfg = cfg();
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"repositories":[meta(),
            {"id":22,"full_name":"elsewhere/two","owner":{"id":12}},
            {"id":99,"full_name":"team/other","owner":{"id":11}},
            {"id":21,"full_name":"team/one","owner":{"id":99}}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        gh.admitted_repositories(&cfg, 7).await.unwrap(),
        vec!["team/one"]
    );
    assert_eq!(github_token_repository(&cfg, 7, "team/one").unwrap(), 21);
    assert!(github_token_repository(&cfg, 7, "elsewhere/two").is_err());
    assert!(gh.admitted_repositories(&cfg, 999).await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
#[tokio::test]
async fn forgejo_targets_require_both_policy_and_live_identity() {
    let server = MockServer::start().await;
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let cfg = cfg();
    assert!(forgejo_repository(&cfg, &client, "mirror/two", Some(41))
        .await
        .is_err());
    assert!(forgejo_repository(&cfg, &client, "mirror/one", Some(42))
        .await
        .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
    for owner in [31, 99] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/mirror/one"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":41,"full_name":"mirror/one","owner":{"id":owner}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            forgejo_repository(&cfg, &client, "mirror/one", Some(41))
                .await
                .is_ok(),
            owner == 31
        );
    }
}
