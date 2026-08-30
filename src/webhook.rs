//! Webhook signature verification and event routing.

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

pub type HmacSha256 = Hmac<Sha256>;

/// Verify GitHub's `X-Hub-Signature-256` header (`sha256=<hex>`) against the body.
pub fn verify_signature(secret: &str, body: &[u8], signature_header: Option<&str>) -> bool {
    let Some(sig) = signature_header else {
        return false;
    };
    let Some(hex_part) = sig.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_part) else {
        return false;
    };
    let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

// ---------------------------------------------------------------------------
// Payload field extraction helpers (serde_json::Value navigation)
// ---------------------------------------------------------------------------

pub fn jstr<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str()
}

pub fn jstr_or<'a>(v: &'a Value, path: &[&str], default: &'a str) -> &'a str {
    jstr(v, path).unwrap_or(default)
}

pub fn ji64(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_i64()
}

pub fn jbool(v: &Value, path: &[&str]) -> Option<bool> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_bool()
}

// ---------------------------------------------------------------------------
// Event classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum WebhookEvent {
    Ping,
    /// issue_comment (action=created) on a PR
    PrComment {
        repo: String,
        pr_number: i64,
        comment_body: String,
        commenter: String,
        installation_id: i64,
        app_slug: String,
        /// author of the PR (for r+ checks)
        pr_author: String,
        /// issue is a PR?
        is_pr: bool,
    },
    /// pull_request synchronize/reopened/opened — rebase detection
    PullRequest {
        repo: String,
        pr_number: i64,
        action: String,
        installation_id: i64,
    },
    /// labeled event — codeql label trigger
    PrLabeled {
        repo: String,
        pr_number: i64,
        label: String,
        installation_id: i64,
    },
    Ignored(String),
}

/// Classify a webhook payload. Returns the parsed event or why it was ignored.
pub fn classify(event_header: &str, payload: &Value) -> WebhookEvent {
    match event_header {
        "ping" => WebhookEvent::Ping,
        "issue_comment" => {
            if jstr(payload, &["action"]) != Some("created") {
                return WebhookEvent::Ignored("action != created".into());
            }
            let Some(installation_id) = ji64(payload, &["installation", "id"]) else {
                return WebhookEvent::Ignored("no installation".into());
            };
            let repo = jstr_or(payload, &["repository", "full_name"], "");
            if repo.is_empty() {
                return WebhookEvent::Ignored("no repo".into());
            }
            let Some(pr_number) = ji64(payload, &["issue", "number"]) else {
                return WebhookEvent::Ignored("no issue number".into());
            };
            let is_pr = payload
                .get("issue")
                .and_then(|i| i.get("pull_request"))
                .is_some();
            let comment_body = jstr_or(payload, &["comment", "body"], "");
            let commenter = jstr_or(payload, &["comment", "user", "login"], "");
            let app_slug = jstr_or(payload, &["installation", "app_slug"], "");
            let pr_author = jstr_or(payload, &["issue", "user", "login"], "");
            WebhookEvent::PrComment {
                repo: repo.to_string(),
                pr_number,
                comment_body: comment_body.to_string(),
                commenter: commenter.to_string(),
                installation_id,
                app_slug: app_slug.to_string(),
                pr_author: pr_author.to_string(),
                is_pr,
            }
        }
        "pull_request" => {
            let action = jstr_or(payload, &["action"], "");
            // labeled → possible codeql trigger
            if action == "labeled" {
                let label = jstr_or(payload, &["label", "name"], "");
                return match (
                    ji64(payload, &["installation", "id"]),
                    ji64(payload, &["pull_request", "number"]),
                    jstr(payload, &["repository", "full_name"]),
                ) {
                    (Some(id), Some(n), Some(repo)) => WebhookEvent::PrLabeled {
                        repo: repo.to_string(),
                        pr_number: n,
                        label: label.to_string(),
                        installation_id: id,
                    },
                    _ => WebhookEvent::Ignored("bad labeled payload".into()),
                };
            }
            if !matches!(action, "synchronize" | "reopened" | "opened") {
                return WebhookEvent::Ignored(format!("action {action} not handled"));
            }
            match (
                ji64(payload, &["installation", "id"]),
                ji64(payload, &["pull_request", "number"]),
                jstr(payload, &["repository", "full_name"]),
            ) {
                (Some(installation_id), Some(pr_number), Some(repo)) => WebhookEvent::PullRequest {
                    repo: repo.to_string(),
                    pr_number,
                    action: action.to_string(),
                    installation_id,
                },
                _ => WebhookEvent::Ignored("bad pull_request payload".into()),
            }
        }
        other => WebhookEvent::Ignored(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_signature_roundtrip() {
        let secret = "s3cr3t";
        let body = b"{\"hello\": \"world\"}";
        let mut mac = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_signature(secret, body, Some(&sig)));
        assert!(!verify_signature(secret, b"tampered", Some(&sig)));
        assert!(!verify_signature("wrong", body, Some(&sig)));
        assert!(!verify_signature(secret, body, None));
        assert!(!verify_signature(secret, body, Some("garbage")));
    }

    #[test]
    fn test_classify_ping() {
        assert_eq!(classify("ping", &json!({})), WebhookEvent::Ping);
    }

    #[test]
    fn test_classify_pr_comment() {
        let payload = json!({
            "action": "created",
            "installation": {"id": 42, "app_slug": "xero-review"},
            "repository": {"full_name": "octocat/hello"},
            "issue": {"number": 7, "pull_request": {"url": "x"}, "user": {"login": "alice"}},
            "comment": {"body": "@xero-review ping", "user": {"login": "bob"}}
        });
        match classify("issue_comment", &payload) {
            WebhookEvent::PrComment {
                repo,
                pr_number,
                commenter,
                installation_id,
                is_pr,
                ..
            } => {
                assert_eq!(repo, "octocat/hello");
                assert_eq!(pr_number, 7);
                assert_eq!(commenter, "bob");
                assert_eq!(installation_id, 42);
                assert!(is_pr);
            }
            other => panic!("expected PrComment, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_ignores_issue_comments() {
        let payload = json!({
            "action": "created",
            "installation": {"id": 42},
            "repository": {"full_name": "octocat/hello"},
            "issue": {"number": 7},
            "comment": {"body": "@xero-review ping", "user": {"login": "bob"}}
        });
        match classify("issue_comment", &payload) {
            WebhookEvent::PrComment { is_pr, .. } => assert!(!is_pr),
            other => panic!("expected PrComment, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_pull_request_sync() {
        let payload = json!({
            "action": "synchronize",
            "installation": {"id": 42},
            "repository": {"full_name": "octocat/hello"},
            "pull_request": {"number": 7}
        });
        match classify("pull_request", &payload) {
            WebhookEvent::PullRequest { action, .. } => assert_eq!(action, "synchronize"),
            other => panic!("expected PullRequest, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_pull_request_labeled() {
        let payload = json!({
            "action": "labeled",
            "installation": {"id": 42},
            "repository": {"full_name": "octocat/hello"},
            "pull_request": {"number": 7},
            "label": {"name": "codeql"}
        });
        match classify("pull_request", &payload) {
            WebhookEvent::PrLabeled { label, .. } => assert_eq!(label, "codeql"),
            other => panic!("expected PrLabeled, got {other:?}"),
        }
    }
}
