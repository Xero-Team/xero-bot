use super::*;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cfg() -> Config {
    let mut cfg = Config::from_env();
    cfg.bot_access_policy = "{}".into();
    cfg.app_id.clear();
    cfg.forgejo_token.clear();
    cfg
}
fn allow(cfg: &mut Config, names: &[&str]) {
    let github: Vec<_> = names.iter().enumerate().map(|(i, name)| {
        json!({"installation_id":2,"owner_id":3,"repository_id":i+1,"repository":name})
    }).collect();
    let forgejo: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| json!({"owner_id":3,"repository_id":i+1,"repository":name}))
        .collect();
    cfg.bot_access_policy = json!({"github":github,"forgejo":forgejo}).to_string();
}
fn metadata(name: &str, id: usize) -> Value {
    json!({"id":id,"full_name":name,"owner":{"id":3},"private":false,"archived":false})
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

#[tokio::test]
async fn github_paginates_and_exposes_only_live_admitted_public_identities() {
    let server = MockServer::start().await;
    let mut cfg = cfg();
    let names = [
        "team/public",
        "team/private",
        "team/archived",
        "team/transferred",
        "team/replaced",
        "team/unknown",
        "team/last",
    ];
    allow(&mut cfg, &names);
    let mut metas: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| metadata(n, i + 1))
        .collect();
    metas[0]["full_name"] = json!("Team/Public");
    metas[1]["private"] = json!(true);
    metas[2]["archived"] = json!(true);
    metas[3]["owner"]["id"] = json!(99);
    metas[4]["id"] = json!(99);
    metas[5].as_object_mut().unwrap().remove("private");
    let last = metas.pop().unwrap();
    metas.push(metadata("team/unadmitted", 20));
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(query_param("per_page", "100"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"repositories":metas}))
                .insert_header(
                    "Link",
                    format!(
                        "<{}/installation/repositories?page=2>; rel=\"next\"",
                        server.uri()
                    ),
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"repositories":[last]})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(admission::github_installations(&cfg), vec![2]);
    assert_eq!(
        github_projects(&cfg, &gh(&server), 2).await.unwrap(),
        ["Team/Public", "team/last"]
    );
    assert!(github_projects(&cfg, &gh(&server), 999).await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn forgejo_filters_private_archived_changed_and_unknown_identities_without_unapproved_io() {
    let server = MockServer::start().await;
    let mut cfg = cfg();
    let names = [
        "team/public",
        "team/private",
        "team/archived",
        "team/moved",
        "team/replaced",
        "team/unknown",
        "team/deleted",
    ];
    allow(&mut cfg, &names);
    for (i, name) in names.iter().enumerate() {
        let mut meta = metadata(name, i + 1);
        match i {
            0 => meta["full_name"] = json!("Team/Public"),
            1 => meta["private"] = json!(true),
            2 => meta["archived"] = json!(true),
            3 => meta["owner"]["id"] = json!(9),
            4 => meta["id"] = json!(99),
            5 => {
                meta.as_object_mut().unwrap().remove("archived");
            }
            _ => (),
        }
        let response = if i == 6 {
            ResponseTemplate::new(404)
        } else {
            ResponseTemplate::new(200).set_body_json(meta)
        };
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/repos/{name}")))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
    }
    let fj = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let mut candidates = admission::forgejo_repositories(&cfg);
    candidates.push("team/unadmitted".into());
    assert_eq!(
        forgejo_projects(&cfg, &fj, candidates).await.unwrap(),
        ["Team/Public"]
    );
    assert_eq!(server.received_requests().await.unwrap().len(), names.len());
}

#[tokio::test]
async fn empty_or_invalid_admission_needs_no_provider_credentials() {
    let mut cfg = cfg();
    for policy in ["{}", "", "invalid", r#"{"github":[{}]}"#] {
        cfg.bot_access_policy = policy.into();
        for provider in [Provider::Github, Provider::Forgejo] {
            assert!(discover(&cfg, provider).await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn upstream_outages_are_errors_instead_of_empty_or_partial_directories() {
    let server = MockServer::start().await;
    let mut cfg = cfg();
    allow(&mut cfg, &["team/public"]);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("private provider diagnostics"))
        .expect(2)
        .mount(&server)
        .await;
    assert!(github_projects(&cfg, &gh(&server), 2).await.is_err());
    let fj = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    assert!(
        forgejo_projects(&cfg, &fj, admission::forgejo_repositories(&cfg))
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn cache_coalesces_refreshes_expires_at_ttl_and_discards_stale_names_on_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cache = Cache::default();
    let calls = AtomicUsize::new(0);
    let fetch = || async {
        calls.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        Ok(vec!["team/once-public".into()])
    };
    let (a, b) = tokio::join!(cache.get(fetch()), cache.get(fetch()));
    assert_eq!(a, b);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(TTL - Duration::from_secs(1)).await;
    assert!(cache.get(async { Err(()) }).await.is_ok());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(cache.get(async { Err(()) }).await.is_err());
    assert!(cache.get(fetch()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(BACKOFF).await;
    assert!(cache
        .get(async { Ok(Vec::new()) })
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(start_paused = true)]
async fn slow_discovery_is_bounded_and_failure_is_cached() {
    let cache = Cache::default();
    let started = Instant::now();
    assert!(cache.get(std::future::pending()).await.is_err());
    assert_eq!(started.elapsed(), TIMEOUT);
    assert!(cache
        .get(async { Ok(vec!["not-fetched".into()]) })
        .await
        .is_err());
}

#[tokio::test]
async fn directory_routes_work_without_queue_records_and_do_not_expose_provider_errors() {
    let upstream = MockServer::start().await;
    let mut cfg = cfg();
    allow(&mut cfg, &["team/public", "team/private"]);
    cfg.forgejo_base_url = upstream.uri();
    cfg.forgejo_token = "test-token".into();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/public"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("team/public", 1)))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut private = metadata("team/private", 2);
    private["private"] = json!(true);
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/team/private"))
        .respond_with(ResponseTemplate::new(200).set_body_json(private))
        .expect(1)
        .mount(&upstream)
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, routes(cfg)).await.unwrap();
    });
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let response = client
            .get(format!("{url}/api/command-projects?provider=forgejo"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"provider":"forgejo","repositories":["team/public"],"cache_seconds":60})
        );
    }
    // Invalid credentials must be an error, never a raw allowlist or leaked key path.
    let failure = client
        .get(format!("{url}/api/command-projects"))
        .send()
        .await
        .unwrap();
    assert_eq!(failure.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failure.headers()[header::CACHE_CONTROL], "no-store");
    let body = failure.text().await.unwrap();
    assert!(!body.contains("team/") && !body.contains("APP_ID"));
    assert_eq!(
        client
            .get(format!("{url}/api/command-projects?provider=unknown"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let script = client
        .get(format!("{url}/assets/projects.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(script.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(script
        .text()
        .await
        .unwrap()
        .contains("/api/command-projects"));
    server.abort();
}
