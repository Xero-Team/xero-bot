//! Local testing helper: build a signed GitHub webhook payload and POST it to
//! a running xero-bot (self-hosted axum server, default http://localhost:8080).
//!
//! Usage:
//!   cargo run --example send_webhook -- [port] <event> <comment-body>
//!
//! Examples:
//!   cargo run --example send_webhook -- issue-comment "@xero-review ping"
//!   cargo run --example send_webhook -- issue-comment "r? @octocat"
//!   cargo run --example send_webhook -- issue-comment "@xero-review review"
//!   cargo run --example send_webhook -- pr-synchronize

use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!(
            "usage: send_webhook [port] <issue-comment|pr-synchronize|ping-event> [comment-body]"
        );
        exit(2);
    }

    // optional port as first arg
    let (port, rest) = match args[0].parse::<u16>() {
        Ok(p) => (p, &args[1..]),
        Err(_) => (8080u16, &args[..]),
    };
    if rest.is_empty() {
        eprintln!("missing event name");
        exit(2);
    }
    let event = &rest[0];
    let comment_body = rest.get(1).map(|s| s.as_str()).unwrap_or("");

    let secret = std::env::var("WEBHOOK_SECRET").unwrap_or_else(|_| "dev-secret".into());
    let bot_name = std::env::var("BOT_NAME").unwrap_or_else(|_| "xero-review".into());

    let (event_header, payload) = match event.as_str() {
        "issue-comment" => (
            "issue_comment",
            serde_json::json!({
                "action": "created",
                "installation": {"id": 1, "app_slug": bot_name},
                "repository": {"full_name": "octocat/hello-world"},
                "issue": {
                    "number": 1,
                    "pull_request": {"url": "https://api.github.com/repos/octocat/hello-world/pulls/1"},
                    "user": {"login": "octocat"}
                },
                "comment": {"body": comment_body, "user": {"login": "octocat"}}
            }),
        ),
        "pr-synchronize" => (
            "pull_request",
            serde_json::json!({
                "action": "synchronize",
                "installation": {"id": 1, "app_slug": bot_name},
                "repository": {"full_name": "octocat/hello-world"},
                "pull_request": {"number": 1}
            }),
        ),
        "ping-event" => ("ping", serde_json::json!({"zen": "Design for failure."})),
        other => {
            eprintln!("unknown event: {other} (use issue-comment | pr-synchronize | ping-event)");
            exit(2);
        }
    };

    let body = serde_json::to_vec(&payload).expect("serialize payload");

    // HMAC-SHA256 signature (same algorithm as GitHub)
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("hmac");
    mac.update(&body);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    let url = format!("http://localhost:{port}/webhook");
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&url)
        .header("X-GitHub-Event", event_header)
        .header("X-Hub-Signature-256", &signature)
        .header("Content-Type", "application/json")
        .header("User-Agent", "GitHub-Hookshot/secret-test")
        .body(body)
        .send();

    match resp {
        Ok(r) => {
            let status = r.status();
            let text = r.text().unwrap_or_default();
            println!("POST {url} [{event_header}]");
            println!("  -> {status}: {text}");
        }
        Err(e) => {
            eprintln!("request failed: {e}");
            exit(1);
        }
    }
}
