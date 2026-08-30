//! What actually goes on the wire: path encoding, query separation, pagination.
//!
//! Every assertion here reads `received_requests()` rather than relying on a
//! path matcher, because the bugs being pinned are *about* the request URL. A
//! matcher that fails tells you the mock went unused; the raw URL tells you
//! what was sent instead — and for the `ref` bug that distinction is the whole
//! point, since the wrong request succeeded.

use serde_json::json;
use wiremock::matchers::{any, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xero_bot::github::Client;

fn client_for(server: &MockServer) -> Client {
    let crab = octocrab::OctocrabBuilder::new()
        .personal_token("ghp_test")
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    Client {
        crab,
        app_slug: "xero-review".into(),
    }
}

/// The single request the server saw, as (path, query).
async fn only_request(server: &MockServer) -> (String, Option<String>) {
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "expected exactly one request, got {reqs:?}");
    let url = &reqs[0].url;
    (url.path().to_string(), url.query().map(String::from))
}

/// Production symptom: `remove_label` on `needs rebase` returned a bare
/// `http error: Uri` — a space is not legal in a URI, so the request was never
/// even built, and the rebase flow reported a transport failure for a label
/// name it had itself configured.
#[tokio::test]
async fn label_name_with_a_space_is_percent_encoded() {
    let server = MockServer::start().await;
    // GitHub answers a label deletion with 200 and the remaining labels, not a
    // bare 204 — `delete::<Value>` needs a body to parse.
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let gh = client_for(&server);
    gh.remove_label("octocat/hello", 7, "needs rebase")
        .await
        .expect("an encoded label name must produce a valid request");

    let (p, _) = only_request(&server).await;
    assert_eq!(p, "/repos/octocat/hello/issues/7/labels/needs%20rebase");
}

/// A non-ASCII path has to be encoded, and `ref` has to arrive as a real query
/// parameter.
///
/// The non-ASCII half is the milder case: `http::Uri` accepts high bytes, so
/// this previously went out as raw UTF-8 in the request line — invalid HTTP
/// that happened to be tolerated. The assertion pins the encoded form so it
/// stays correct by construction.
#[tokio::test]
async fn non_ascii_path_is_encoded_and_ref_is_a_query_parameter() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "file", "encoding": "base64", "content": "aGk=",
        })))
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let content = gh
        .get_file_content("octocat/hello", "文档/说明.md", "feature-x")
        .await
        .expect("an encoded path must produce a valid request");
    assert_eq!(content.as_deref(), Some("hi"));

    let (p, q) = only_request(&server).await;
    assert_eq!(
        p, "/repos/octocat/hello/contents/%E6%96%87%E6%A1%A3/%E8%AF%B4%E6%98%8E.md",
        "each path segment is encoded, and `/` stays a separator"
    );
    assert_eq!(q.as_deref(), Some("ref=feature-x"));
}

/// The regression that mattered most, because it *succeeded*.
///
/// With `?ref={reference}` interpolated into the format string, a path
/// containing `?` ended the path early and the intended `ref` became part of
/// the key of a nonsense query parameter (`b.md?ref=main`). GitHub saw no
/// `ref`, served the default branch, and returned 200 — so the agent silently
/// read a different revision of the file than it asked for, with nothing
/// logged anywhere.
#[tokio::test]
async fn question_mark_in_path_does_not_swallow_the_ref() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "file", "encoding": "base64", "content": "aGk=",
        })))
        .mount(&server)
        .await;

    let gh = client_for(&server);
    gh.get_file_content("octocat/hello", "docs/a?b.md", "feature-x")
        .await
        .expect("a `?` in the path must not break the request");

    let (p, q) = only_request(&server).await;
    assert_eq!(
        p, "/repos/octocat/hello/contents/docs/a%3Fb.md",
        "the `?` belongs to the path, not to the query"
    );
    assert_eq!(
        q.as_deref(),
        Some("ref=feature-x"),
        "`ref` must survive a path containing `?` — this is the silent-wrong-branch bug"
    );
}

/// A directory is a JSON array, so `get("type")` is None and the old code
/// answered `BadShape("no content")` — reporting a malformed response for a
/// perfectly well-formed one, and hiding the actual answer ("that's a
/// directory") from the agent.
#[tokio::test]
async fn a_directory_reads_as_none_not_a_bad_shape_error() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name": "a.rs", "type": "file", "path": "src/a.rs"},
        ])))
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let got = gh
        .get_file_content("octocat/hello", "src", "main")
        .await
        .expect("a directory is not an error");
    assert!(got.is_none(), "a directory must read as None, got {got:?}");
}

/// Over 1 MB the contents API returns `content: ""` with `encoding: "none"`.
/// That base64-decodes to an empty string, so the file read as *empty* — a
/// wrong answer the agent cannot tell from a real empty file.
#[tokio::test]
async fn an_oversized_blob_is_an_error_not_an_empty_file() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "file", "encoding": "none", "content": "", "size": 2_000_000,
        })))
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let err = gh
        .get_file_content("octocat/hello", "big.bin", "main")
        .await
        .expect_err("an unreadable blob must not read as an empty file");
    let msg = err.to_string();
    assert!(
        msg.contains("2000000") && msg.contains("not empty"),
        "the error should say why and how big, got: {msg}"
    );
}

/// `own_previous_reviews` reads `list_pr_reviews` to find the bot's own review.
/// On a PR with more than one page of reviews that review is on a later page,
/// so stopping at page one made `r-` answer "nothing to withdraw" and made
/// incremental review forget the previous round.
#[tokio::test]
async fn list_pr_reviews_follows_the_link_header_to_the_last_page() {
    let server = MockServer::start().await;
    let next = format!(
        "{}/repos/octocat/hello/pulls/7/reviews?per_page=100&page=2",
        server.uri()
    );

    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7/reviews"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Link", format!("<{next}>; rel=\"next\"").as_str())
                .set_body_json(json!([{"id": 1, "user": {"login": "alice"}}])),
        )
        // Two calls: once directly, once via `own_previous_reviews` below.
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello/pulls/7/reviews"))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id": 2, "user": {"login": "xero-review[bot]"}}])),
        )
        .expect(2)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let reviews = gh.list_pr_reviews("octocat/hello", 7).await.unwrap();
    assert_eq!(reviews.len(), 2, "both pages must be collected");

    // The point of paginating: the bot's own review lives on page two.
    let own = gh.own_previous_reviews("octocat/hello", 7).await.unwrap();
    assert_eq!(own.len(), 1, "the bot's own review is on page 2");
    assert_eq!(own[0]["id"], 2);
}

/// `/installation/repositories` wraps its list in an object. `Page<Value>`
/// unwraps that shape, so the sweep reaches past the first 100 repos.
#[tokio::test]
async fn installation_repositories_follows_the_wrapped_pagination_shape() {
    let server = MockServer::start().await;
    let next = format!("{}/installation/repositories?page=2", server.uri());

    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Link", format!("<{next}>; rel=\"next\"").as_str())
                .set_body_json(json!({
                    "total_count": 2,
                    "repositories": [{"full_name": "octocat/one"}],
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 2,
            "repositories": [{"full_name": "octocat/two"}],
        })))
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let repos = Client::installation_repositories_via(&gh).await.unwrap();
    assert_eq!(repos, vec!["octocat/one", "octocat/two"]);
}

/// The labels endpoint carried no `per_page`, so GitHub applied its default of
/// 30 and a repo with more labels than that on one issue lost the rest — which
/// reads downstream as "the label isn't there".
#[tokio::test]
async fn list_labels_asks_for_a_full_page() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"name": "bug"}])))
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let labels = gh.list_labels("octocat/hello", 7).await.unwrap();
    assert_eq!(labels, vec!["bug"]);

    let (_, q) = only_request(&server).await;
    assert_eq!(q.as_deref(), Some("per_page=100"));
}

/// Defence in depth: a login can't legally contain anything needing escaping,
/// and the parser validates one before this call is built — but the encoding
/// keeps that true by construction instead of by remembering to check.
#[tokio::test]
async fn collaborator_permission_encodes_the_login() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "write"})))
        .mount(&server)
        .await;

    let gh = client_for(&server);
    let perm = gh
        .collaborator_permission("octocat/hello", "a b")
        .await
        .unwrap();
    assert_eq!(perm, "write");

    let (p, _) = only_request(&server).await;
    assert_eq!(p, "/repos/octocat/hello/collaborators/a%20b/permission");
}
