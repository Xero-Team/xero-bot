use super::*;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cfg() -> Config {
    let mut cfg = Config::from_env();
    cfg.app_id = "123".into();
    cfg.private_key_pem = Some(include_str!("../../tests/fixtures/rsa_test_key.txt").into());
    cfg.bot_access_policy = json!({"github":[
        {"installation_id":7,"owner_id":11,"repository_id":21,"repository":"team/one"},
        {"installation_id":7,"owner_id":11,"repository_id":99,"repository":"team/deleted"}
    ]})
    .to_string();
    cfg
}
fn client(server: &MockServer) -> Client {
    Client {
        crab: client_builder()
            .base_uri(server.uri())
            .unwrap()
            .personal_token("test-token")
            .build()
            .unwrap(),
        app_slug: "bot".into(),
    }
}
fn metadata() -> Value {
    // Admitted private repositories still work for internal Git operations.
    json!({"id":21,"full_name":"Team/One","owner":{"id":11},"private":true})
}
async fn serve_metadata(server: &MockServer, meta: Value) {
    Mock::given(method("GET"))
        .and(path("/repos/team/one"))
        .respond_with(ResponseTemplate::new(200).set_body_json(meta))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn inaccessible_sibling_does_not_poison_the_exact_repository_token_request() {
    let server = MockServer::start().await;
    serve_metadata(&server, metadata()).await;
    // Reproduce the production 422 if unrelated stale repository IDs are sent.
    Mock::given(method("POST"))
        .and(path("/app/installations/7/access_tokens"))
        .respond_with(ResponseTemplate::new(422).set_body_json(
            json!({"message":"A repository is not accessible to this installation"}),
        ))
        .with_priority(2)
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/7/access_tokens"))
        .and(body_json(json!({"repository_ids":[21]})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"token":"single-repository-test-token"})),
        )
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        client(&server)
            .repository_token_at(&cfg(), 7, "team/one", &server.uri())
            .await
            .unwrap(),
        "single-repository-test-token"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "POST");
}

#[tokio::test]
async fn unadmitted_names_or_installations_never_read_metadata_or_mint_tokens() {
    let server = MockServer::start().await;
    let gh = client(&server);
    for (installation, name) in [(7, "team/unknown"), (8, "team/one"), (0, "team/one")] {
        assert!(matches!(
            gh.repository_token_at(&cfg(), installation, name, &server.uri())
                .await,
            Err(GhError::Api { status: 403, .. })
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn changed_live_repository_identity_is_denied_before_signing_or_minting() {
    for meta in [
        json!({"id":21,"full_name":"team/one","owner":{"id":99}}),
        json!({"id":99,"full_name":"team/one","owner":{"id":11}}),
        json!({"id":21,"full_name":"team/renamed","owner":{"id":11}}),
    ] {
        let server = MockServer::start().await;
        serve_metadata(&server, meta).await;
        let mut cfg = cfg();
        cfg.private_key_pem = None;
        assert!(matches!(
            client(&server)
                .repository_token_at(&cfg, 7, "team/one", &server.uri())
                .await,
            Err(GhError::Api { status: 403, .. })
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn token_issuer_failure_is_not_retried_with_broader_permissions() {
    let server = MockServer::start().await;
    serve_metadata(&server, metadata()).await;
    Mock::given(method("POST"))
        .and(path("/app/installations/7/access_tokens"))
        .and(body_json(json!({"repository_ids":[21]})))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"message":"unavailable"})))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server)
            .repository_token_at(&cfg(), 7, "team/one", &server.uri())
            .await,
        Err(GhError::Api { status: 500, .. })
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
