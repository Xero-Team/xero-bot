//! Git commit identity belongs to the authenticated App's bot user, not its
//! App ID, a mention alias, or a similarly named account on another provider.
use super::{classify_octo_error, enc_seg, Client, GhError};
use crate::config::Config;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
}

impl Client {
    pub async fn commit_identity(&self, cfg: &Config) -> Result<CommitIdentity, GhError> {
        // Unlike display/self-review matching, commit attribution must never
        // fall back to BOT_NAME or an APP_SLUG override after an API failure.
        resolve(&Self::app_client(cfg)?, self).await
    }
}

async fn resolve(app: &octocrab::Octocrab, gh: &Client) -> Result<CommitIdentity, GhError> {
    let metadata: Value = app
        .get("/app", None::<&()>)
        .await
        .map_err(classify_octo_error)?;
    let slug = metadata["slug"]
        .as_str()
        .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
        .ok_or_else(|| GhError::BadShape("authenticated App has no valid slug".into()))?;
    let expected = format!("{slug}[bot]");
    let user = gh.get(&format!("/users/{}", enc_seg(&expected))).await?;
    let login = user["login"]
        .as_str()
        .filter(|s| s.eq_ignore_ascii_case(&expected));
    let id = user["id"].as_u64().filter(|id| *id > 0);
    match (login, id, user["type"].as_str()) {
        (Some(login), Some(id), Some("Bot")) => Ok(CommitIdentity {
            name: login.into(),
            // GitHub resolves this ID-based address to the same bot account.
            // Omitting [bot] from the email local part also lets Forgejo's
            // email validator accept it as a verified account alias.
            email: format!("{id}+{slug}@users.noreply.github.com"),
        }),
        _ => Err(GhError::BadShape(
            "authenticated App bot user could not be verified".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    fn client(server: &MockServer) -> Client {
        Client {
            crab: super::super::client_builder()
                .base_uri(server.uri())
                .unwrap()
                .personal_token("test")
                .build()
                .unwrap(),
            app_slug: "wrong-mention-alias".into(),
        }
    }

    #[tokio::test]
    async fn commit_identity_uses_authenticated_app_and_bot_user_id() {
        let server = MockServer::start().await;
        let gh = client(&server);
        Mock::given(method("GET"))
            .and(path("/app"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id":99,"slug":"real-app"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/users/real-app%5Bbot%5D"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":123,"login":"real-app[bot]","type":"Bot"})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            resolve(&gh.crab, &gh).await.unwrap(),
            CommitIdentity {
                name: "real-app[bot]".into(),
                email: "123+real-app@users.noreply.github.com".into()
            }
        );
    }

    #[tokio::test]
    async fn commit_identity_rejects_missing_mismatched_or_human_accounts() {
        for user in [
            json!({}),
            json!({"id":0,"login":"real-app[bot]","type":"Bot"}),
            json!({"id":123,"login":"somebody[bot]","type":"Bot"}),
            json!({"id":123,"login":"real-app[bot]","type":"User"}),
        ] {
            let server = MockServer::start().await;
            let gh = client(&server);
            Mock::given(method("GET"))
                .and(path("/app"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"slug":"real-app"})))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/users/real-app%5Bbot%5D"))
                .respond_with(ResponseTemplate::new(200).set_body_json(user))
                .mount(&server)
                .await;
            assert!(resolve(&gh.crab, &gh).await.is_err());
        }
    }

    #[tokio::test]
    async fn commit_identity_does_not_guess_after_app_api_failure() {
        let server = MockServer::start().await;
        let gh = client(&server);
        Mock::given(method("GET"))
            .and(path("/app"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(json!({"message":"unavailable"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert!(resolve(&gh.crab, &gh).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
