#[tokio::test]
async fn audit_ambiguous_policy_404_is_not_absence() {
    let server = MockServer::start().await;
    mock_identity_and_policy(&server).await;
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/contents/.github"))
        .respond_with(ResponseTemplate::new(404)).with_priority(1).mount(&server).await;
    assert!(load_comments(&client, "team/repo").await.is_err());
}

#[tokio::test]
async fn audit_removal_is_sent_once_with_a_numeric_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id":2,"name":"bug"}])))
        .mount(&server).await;
    // Both formats are supported by current Forgejo. Only the ID is portable
    // to older releases, and deletion must never be replayed automatically.
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204)).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":1})))
        .mount(&server).await;
    let cfg = cfg();
    let job = comment_job("label -bug", false);
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let ctx = CommandContext {client:&client,cfg:&cfg,job:&job,bot_login:"xero-review",lang:Lang::En};
    set_labels(&ctx, &[], &["bug".into()]).await;
    let requests = server.received_requests().await.unwrap();
    let deletes: Vec<_> = requests.iter().filter(|r| r.method == "DELETE").collect();
    assert_eq!(deletes.len(), 1, "DELETE must not be replayed");
    assert_eq!(deletes[0].url.path(), "/api/v1/repos/team/repo/issues/7/labels/2");
}

#[tokio::test]
async fn audit_status_delete_failure_is_not_reported_as_success() {
    let server = MockServer::start().await;
    let cfg = cfg();
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id":1,"name":cfg.label_waiting_review}, {"id":2,"name":cfg.label_waiting_author}, {"id":3,"name":cfg.label_blocked}
        ]))).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).mount(&server).await;
    Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(403).set_body_json(json!({"message":"denied"})))
        .mount(&server).await;
    let job = comment_job("@xero-review ready", true);
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let ctx = CommandContext {client:&client,cfg:&cfg,job:&job,bot_login:"xero-review",lang:Lang::En};
    set_status(&ctx, Command::Ready).await;
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| String::from_utf8_lossy(&r.body).contains("Status updated:")));
    assert!(requests.iter().any(|r| r.url.path().ends_with("/comments") && String::from_utf8_lossy(&r.body).contains("failed")));
}

#[tokio::test]
async fn audit_self_request_does_not_open_a_session() {
    let server = MockServer::start().await;
    mock_identity_and_policy(&server).await;
    // Give the command a readable, default policy independently of the 404
    // fallback test, so this isolates semantic wake eligibility.
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/contents/.github/xero-bot.toml"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type":"file","encoding":"base64","content": ({ use base64::Engine; base64::engine::general_purpose::STANDARD.encode("[command_triggers]\nassign={mode=\"mention_once\"}") })})))
        .with_priority(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":1}))).mount(&server).await;
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let cfg = cfg();
    let job = comment_job("@xero-review assign @xero-review", true);
    let mut future = session_wake(&cfg, &job).unwrap();
    future.comment_id += 1;
    future.source_at += 1000;
    handle_issue_comment_with_client(&client, &cfg, &store, job).await.unwrap();
    assert!(store.session_before_at(&future, 30, future.source_at).unwrap().is_none());
}

#[tokio::test]
async fn audit_partial_dismissal_reports_the_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!([
        {"id":1,"state":"APPROVED","body":APPROVAL_MARKER,"user":{"login":"xero-review"}},
        {"id":2,"state":"APPROVED","body":APPROVAL_MARKER,"user":{"login":"xero-review"}}
    ]))).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).mount(&server).await;
    Mock::given(method("POST")).and(path("/api/v1/repos/team/repo/pulls/7/reviews/2/dismissals"))
        .respond_with(ResponseTemplate::new(403)).with_priority(1).mount(&server).await;
    let cfg = cfg();
    let job = comment_job("@xero-review r-", true);
    let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
    let ctx = CommandContext {client:&client,cfg:&cfg,job:&job,bot_login:"xero-review",lang:Lang::En};
    reject(&ctx).await;
    let requests = server.received_requests().await.unwrap();
    let reply = requests.iter().find(|r| r.url.path().ends_with("/comments")).unwrap();
    assert!(String::from_utf8_lossy(&reply.body).contains("failed"));
}

#[tokio::test]
async fn audit_session_storage_failure_is_not_reported_as_a_missing_session() {
    use base64::Engine;
    let server=MockServer::start().await;mock_identity_and_policy(&server).await;
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/contents/.github/xero-bot.toml"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type":"file","encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode("[command_triggers]\nping={mode='mention_once'}")})))
        .with_priority(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":1}))).mount(&server).await;
    let dir=Dir::new();let store=Store::open(&dir.0).unwrap();store.inject_session_read_failure();
    let client=ForgejoClient::new(&server.uri(),"test-token").unwrap();
    assert!(handle_issue_comment_with_client(&client,&cfg(),&store,comment_job("ping",false)).await.unwrap_err().contains("storage unavailable"));
    let requests=server.received_requests().await.unwrap();
    assert!(requests.iter().any(|r|String::from_utf8_lossy(&r.body).contains("Session storage is unavailable")));
    assert!(!requests.iter().any(|r|String::from_utf8_lossy(&r.body).contains("pong")));
}

#[tokio::test]
async fn audit_actual_bot_identity_cannot_be_assigned_or_wake_a_session() {
    use base64::Engine;
    let server=MockServer::start().await;mock_identity_and_policy(&server).await;
    Mock::given(method("GET")).and(path("/api/v1/user")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"login":"forgejo-bot"}))).with_priority(1).mount(&server).await;
    Mock::given(method("GET")).and(path("/api/v1/repos/team/repo/contents/.github/xero-bot.toml"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type":"file","encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode("[command_triggers]\nassign={mode='mention_once'}")})))
        .with_priority(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":1}))).mount(&server).await;
    let dir=Dir::new();let store=Store::open(&dir.0).unwrap();let client=ForgejoClient::new(&server.uri(),"test-token").unwrap();
    let cfg=cfg();let job=comment_job("@xero-review assign @forgejo-bot",true);let mut future=session_wake(&cfg,&job).unwrap();future.comment_id+=1;future.source_at+=1000;
    handle_issue_comment_with_client(&client,&cfg,&store,job).await.unwrap();
    assert!(store.session_before_at(&future,30,future.source_at).unwrap().is_none());
    assert!(server.received_requests().await.unwrap().iter().all(|r|r.method!="PATCH"));
}
