//! Issue #18 crosses parsing, effective configuration, SQLite and GitHub writes.
//! Existing event/path/queue suites supply the failure and concurrency matrix.
use super::*;
use crate::config::repository::{CommandId, ManualMode};

fn comment(id: i64, body: &str, source_at: i64) -> EventContext {
    let mut ctx = context();
    ctx.delivery = format!("acceptance-{id}");
    ctx.comment_id = Some(id);
    ctx.body = Some(body.into());
    ctx.source_time = chrono::DateTime::from_timestamp_millis(source_at)
        .unwrap()
        .to_rfc3339();
    ctx
}
async fn replies(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == "POST" && r.url.path().ends_with("/comments"))
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["body"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn default_variants_run_issue_commands_show_effective_help_and_never_auto_act() {
    for config in [
        None,
        Some(""),
        Some("[idle_workflows]\nenabled=false"),
        Some("[command_triggers]\nassign={mode='always_mention'}"),
    ] {
        let dir = Dir::new();
        let runtime = Runtime::open(&dir.0).unwrap();
        let server = MockServer::start().await;
        policy(&server, config.unwrap_or("")).await;
        if config.is_none() {
            Mock::given(method("GET"))
                .and(path(
                    "/repos/example/project/contents/.github/xero-bot.toml",
                ))
                .respond_with(ResponseTemplate::new(404))
                .with_priority(1)
                .mount(&server)
                .await;
        }
        response(
            &server,
            "POST",
            "/repos/example/project/issues/3/comments",
            201,
            json!({"id":44}),
        )
        .await;
        response(
            &server,
            "POST",
            "/repos/example/project/issues/3/assignees",
            201,
            json!({"assignees":[{"login":"alice"},{"login":"bob"}]}),
        )
        .await;
        response(
            &server,
            "GET",
            "/repos/example/project/issues/3",
            200,
            json!({"assignees":[{"login":"alice"}]}),
        )
        .await;
        response(
            &server,
            "DELETE",
            "/repos/example/project/issues/3/assignees",
            200,
            json!({"assignees":[]}),
        )
        .await;
        let gh = client(&server);
        let cfg = cfg();
        let cache = RepositoryConfigCache::default();
        let at = chrono::Utc::now().timestamp_millis() - 10_000;
        for (index, body) in [
            "take",
            "untake",
            "cc @bob",
            "r? @bob",
            "claim 是什么意思？",
            "@bot help",
        ]
        .iter()
        .enumerate()
        {
            runtime
                .process(
                    &gh,
                    &cfg,
                    &cache,
                    &comment(20 + index as i64, body, at + index as i64),
                )
                .await
                .unwrap();
        }
        let bodies = replies(&server).await;
        assert_eq!(bodies.len(), 5, "{config:?}: {bodies:?}");
        assert!(bodies[0].contains("claimed"));
        assert!(bodies[1].contains("released"));
        assert!(bodies[2].contains("cc @bob"));
        assert!(bodies[3].contains("Assigned"));
        let help = &bodies[4];
        assert!(help.contains("Active for later comments"));
        for id in CommandId::ALL {
            let row = help
                .lines()
                .find(|line| line.starts_with(&format!("| `{}`", id.name())))
                .unwrap();
            let expected = if config.is_some_and(|s| s.contains("always_mention"))
                && id == CommandId::Assign
            {
                ManualMode::AlwaysMention
            } else {
                id.default_mode()
            };
            let mode = match expected {
                ManualMode::NoMention => "no_mention",
                ManualMode::MentionOnce => "mention_once",
                ManualMode::AlwaysMention => "always_mention",
                ManualMode::Disabled => "disabled",
            };
            assert!(row.contains(mode), "{row}");
        }
        assert_eq!(help.matches("None configured.").count(), 2);
        let writes_before = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method != "GET")
            .count();
        for pr in [false, true] {
            let mut opened = comment(100, "", at);
            opened.comment_id = None;
            opened.event = if pr { "pull_request" } else { "issues" }.into();
            opened.action = "opened".into();
            opened.is_pr = pr;
            opened.delivery = format!("open-{pr}");
            runtime.process(&gh, &cfg, &cache, &opened).await.unwrap();
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.iter().filter(|r| r.method != "GET").count(),
            writes_before
        );
        assert!(requests.iter().all(|r| !r.url.path().contains("/pulls/")));
        assert!(requests
            .iter()
            .all(|r| r.method != "GET" || !r.url.path().ends_with("/comments")));
    }
}

#[tokio::test]
async fn help_session_survives_restart_but_never_authorizes_older_comments_or_other_users() {
    let dir = Dir::new();
    let server = MockServer::start().await;
    policy(&server, "").await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":44}),
    )
    .await;
    let cfg = cfg();
    let gh = client(&server);
    let at = chrono::Utc::now().timestamp_millis() - 86_400_000;
    let runtime = Runtime::open(&dir.0).unwrap();
    runtime
        .process(
            &gh,
            &cfg,
            &RepositoryConfigCache::default(),
            &comment(20, "@bot help", at),
        )
        .await
        .unwrap();
    drop(runtime);
    let runtime = Runtime::open(&dir.0).unwrap();
    runtime
        .process(
            &gh,
            &cfg,
            &RepositoryConfigCache::default(),
            &comment(21, "help", at + 1),
        )
        .await
        .unwrap();
    let mut older = comment(19, "help", at - 1);
    runtime
        .process(&gh, &cfg, &RepositoryConfigCache::default(), &older)
        .await
        .unwrap();
    older = comment(22, "help", at + 2);
    older.user_id = Some(999);
    runtime
        .process(&gh, &cfg, &RepositoryConfigCache::default(), &older)
        .await
        .unwrap();
    let bodies = replies(&server).await;
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b.contains("Active for later comments"))
            .count(),
        2
    );
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b.contains("SessionRequired"))
            .count(),
        2
    );
    assert!(bodies[1].contains("expires"));
    assert!(bodies[1].contains("same installation, repository, Issue/PR thread and GitHub user"));
    // Reducing TTL changes both execution and help's remaining-validity claim.
    server.reset().await;
    policy(
        &server,
        "[command_triggers]\nhelp={mode='no_mention'}\n[command_sessions]\nttl_days=1",
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example/project/issues/3/comments",
        201,
        json!({"id":45}),
    )
    .await;
    runtime
        .process(
            &gh,
            &cfg,
            &RepositoryConfigCache::default(),
            &comment(23, "help", at + 3),
        )
        .await
        .unwrap();
    let bodies = replies(&server).await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("No applicable unexpired session"));
    assert!(bodies[0].contains("TTL: 1 days"));
}
