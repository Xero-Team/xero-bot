use super::*;
use base64::Engine;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn policy_server(policy: Option<&str>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"default_branch":"main"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo/branches/main"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"commit":{"id":"a".repeat(40)}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let response = match policy {
        Some(text) => ResponseTemplate::new(200).set_body_json(json!({
            "type":"file", "encoding":"base64",
            "content":base64::engine::general_purpose::STANDARD.encode(text)
        })),
        None => ResponseTemplate::new(404),
    };
    Mock::given(method("GET"))
        .and(path(
            "/api/v1/repos/team/repo/contents/.github/xero-bot.toml",
        ))
        .and(query_param("ref", "a".repeat(40)))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/repo/contents/.github"))
        .and(query_param("ref", "a".repeat(40)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn complete_forgejo_policy_preserves_mirror_and_comment_settings() {
    let policy = "[command_triggers]\nping = {mode='mention_once'}\n[forgejo.mirror]\nenabled=true\ndestination='mirror/repo'";
    let server = policy_server(Some(policy)).await;
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let actual = client.repository_config("team/repo").await.unwrap();
    let expected = RepositoryConfig::parse(policy, "team/repo").unwrap();
    assert_eq!(actual.forgejo.unwrap(), expected.forgejo.unwrap());
    use crate::config::repository::CommandId;
    assert_eq!(
        actual.comments.unwrap().mode(CommandId::Ping),
        expected.comments.unwrap().mode(CommandId::Ping)
    );
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[tokio::test]
async fn proven_absence_returns_the_complete_disabled_policy() {
    let server = policy_server(None).await;
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let config = client.repository_config("team/repo").await.unwrap();
    assert_eq!(
        config.forgejo.unwrap(),
        crate::config::repository::ForgejoMirrorConfig::disabled()
    );
    assert!(config.comments.is_ok());
}

#[tokio::test]
async fn forgejo_loader_preserves_mirror_validation_errors() {
    let server = policy_server(Some(
        "[forgejo.mirror]\nenabled=true\ndestination='owner/..'",
    ))
    .await;
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let config = client.repository_config("team/repo").await.unwrap();
    assert_eq!(
        config.forgejo.unwrap_err().code,
        crate::config::repository::ReasonCode::InvalidForgejoSync
    );
    assert!(config.comments.is_ok());
}

#[tokio::test]
async fn forgejo_loader_keeps_unknown_fields_strict() {
    let server = policy_server(Some(
        "[forgejo.mirror]\nenabled=false\nsecret='never-print-this'",
    ))
    .await;
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let error = client.repository_config("team/repo").await.unwrap_err();
    assert!(!error.contains("never-print-this"));
}
