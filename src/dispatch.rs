//! Shared webhook dispatch used by both entry modes (Vercel function and
//! self-hosted axum server).

use serde_json::Value;

use crate::commands::parse_commands;
use crate::config::Config;
use crate::github::{normalize_login, Client};
use crate::handlers::{handle_comment, CommentContext};
use crate::webhook::{classify, WebhookEvent};

/// Route a verified webhook payload. Returns the JSON body to answer GitHub
/// with. Long work must be spawned by the caller (wait_until / tokio::spawn).
pub fn route_event(cfg: &Config, event_header: &str, payload: &Value) -> Routing {
    match classify(event_header, payload) {
        WebhookEvent::Ping => Routing::Respond(serde_json::json!({"ok": "pong"})),
        WebhookEvent::Ignored(why) => Routing::Respond(serde_json::json!({"ignored": why})),
        WebhookEvent::PrComment {
            repo,
            pr_number,
            comment_body,
            commenter,
            installation_id,
            via_app_id,
            commenter_is_bot,
            pr_author,
            is_pr,
        } => {
            // Don't react to our own comments, or we execute the commands listed
            // in our own help text. Two independent checks:
            //
            // 1. The App id, which is name-independent — this still holds when
            //    BOT_NAME is misconfigured, which is how the loop got shipped.
            // 2. The login, which needs the `[bot]` suffix stripped: a GitHub App
            //    comments as `name[bot]`, so comparing against a bare BOT_NAME
            //    never matched.
            if let (Some(via), Ok(own)) = (via_app_id, cfg.app_id.parse::<i64>()) {
                if via == own {
                    return Routing::Respond(serde_json::json!({"ignored": "self comment"}));
                }
            }
            if commenter_is_bot && !commenter.is_empty() {
                let own = normalize_login(&cfg.bot_name);
                let configured = normalize_login(&cfg.app_slug);
                let me = normalize_login(&commenter);
                if me == own || (!configured.is_empty() && me == configured) {
                    return Routing::Respond(serde_json::json!({"ignored": "self comment"}));
                }
            }

            // The parser indexes into arbitrary user text. A panic here would kill
            // the webhook response, and GitHub redelivers on failure — so a single
            // bad comment becomes a loop. Contain it at the only entry point both
            // the axum server and the Vercel function share.
            let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                parse_commands(&cfg.bot_name, &comment_body)
            }));
            let parsed = match parsed {
                Ok(p) => p,
                Err(_) => {
                    tracing::error!(
                        "command parser panicked on {repo}#{pr_number} ({} bytes); ignoring",
                        comment_body.len()
                    );
                    return Routing::Respond(serde_json::json!({"ignored": "parse error"}));
                }
            };
            // Rendered here, so `Work` carries plain strings and the background
            // task needn't know the parser's types.
            let diagnostics: Vec<String> =
                parsed.diagnostics.iter().map(|d| d.message()).collect();

            // A comment can be worth answering without containing a command:
            // `@bot reviwe` produces nothing to run and one thing to say. Saying
            // it is the point — silently dropping near-misses is what left users
            // believing a mistyped command had worked.
            if parsed.commands.is_empty() && diagnostics.is_empty() {
                return Routing::Respond(serde_json::json!({"ignored": "no command"}));
            }
            if !is_pr {
                // commands other than review/label work on issues too; keep it
                // simple and require a PR (matching the Python bot)
                return Routing::Respond(serde_json::json!({"ignored": "not a PR"}));
            }

            Routing::Act(Work::Comment {
                repo,
                pr_number,
                installation_id,
                commenter,
                pr_author,
                commands: parsed.commands,
                diagnostics,
            })
        }
        WebhookEvent::PullRequest {
            repo,
            pr_number,
            action,
            installation_id,
        } => Routing::Act(Work::RebaseCheck {
            repo,
            pr_number,
            action,
            installation_id,
        }),
        WebhookEvent::PrLabeled {
            repo,
            pr_number,
            label,
            installation_id,
        } => {
            if !cfg.codeql_label.is_empty() && label == cfg.codeql_label {
                Routing::Act(Work::Codeql {
                    repo,
                    pr_number,
                    installation_id,
                })
            } else {
                Routing::Respond(serde_json::json!({"ignored": "label not configured"}))
            }
        }
    }
}

#[derive(Debug)]
pub enum Routing {
    /// immediate response; nothing to do in the background
    Respond(Value),
    /// background work needed
    Act(Work),
}

#[derive(Debug)]
pub enum Work {
    Comment {
        repo: String,
        pr_number: i64,
        installation_id: i64,
        commenter: String,
        pr_author: String,
        commands: Vec<crate::commands::Command>,
        /// Pre-rendered complaints about what couldn't be understood; may be
        /// non-empty even when `commands` is empty.
        diagnostics: Vec<String>,
    },
    RebaseCheck {
        repo: String,
        pr_number: i64,
        action: String,
        installation_id: i64,
    },
    Codeql {
        repo: String,
        pr_number: i64,
        installation_id: i64,
    },
}

/// Execute background work. Never panics; all errors are logged.
pub async fn execute_work(cfg: &Config, work: Work) {
    let result = execute_work_inner(cfg, work).await;
    if let Err(e) = result {
        tracing::error!("background work failed: {e}");
    }
}

async fn execute_work_inner(cfg: &Config, work: Work) -> Result<(), String> {
    match work {
        Work::Comment {
            repo,
            pr_number,
            installation_id,
            commenter,
            pr_author,
            commands,
            diagnostics,
        } => {
            let gh = Client::installation_resolved(cfg, installation_id)
                .await
                .map_err(|e| format!("installation client: {e}"))?;
            let ctx = CommentContext {
                repo: repo.clone(),
                pr_number,
                commenter,
                pr_author,
                installation_id,
            };
            let results = handle_comment(&gh, cfg, &ctx, commands, diagnostics).await;
            tracing::info!("comment commands on {repo}#{pr_number}: {results:?}");
            Ok(())
        }
        Work::RebaseCheck {
            repo,
            pr_number,
            action,
            installation_id,
        } => {
            let gh = Client::installation(cfg, installation_id, "")
                .map_err(|e| format!("installation client: {e}"))?;
            crate::rebase::handle_push_event(&gh, cfg, &repo, pr_number, &action).await;
            Ok(())
        }
        Work::Codeql {
            repo,
            pr_number,
            installation_id,
        } => {
            let gh = Client::installation(cfg, installation_id, "")
                .map_err(|e| format!("installation client: {e}"))?;
            let status = crate::codeql::run_codeql_report(&gh, cfg, &repo, pr_number).await;
            tracing::info!("codeql report {repo}#{pr_number}: {status}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Config {
        let mut c = Config::from_env();
        c.app_id = "4768775".into();
        c.bot_name = "xero-team-bot".into();
        c.webhook_secret = "whsec".into();
        c
    }

    /// `comment` overrides are merged over a valid PR-comment payload.
    fn payload(comment: serde_json::Value) -> serde_json::Value {
        json!({
            "action": "created",
            "installation": {"id": 42},
            "repository": {"full_name": "Xero-Team/xero-bot"},
            "issue": {"number": 1, "pull_request": {"url": "x"}, "user": {"login": "alice"}},
            "comment": comment
        })
    }

    fn ignored_reason(r: &Routing) -> Option<String> {
        match r {
            Routing::Respond(v) => v
                .get("ignored")
                .and_then(|s| s.as_str())
                .map(String::from),
            Routing::Act(_) => None,
        }
    }

    /// The help text lists every command as `@bot <verb>`, so reacting to our own
    /// comments executed all of them. The App id catches this regardless of how
    /// BOT_NAME is configured.
    #[test]
    fn self_comment_ignored_via_app_id() {
        let p = payload(json!({
            "body": "| `@xero-team-bot review` | `@xero-team-bot label +a -b` |",
            "user": {"login": "anything-at-all", "type": "Bot"},
            "performed_via_github_app": {"id": 4768775}
        }));
        let r = route_event(&cfg(), "issue_comment", &p);
        assert_eq!(ignored_reason(&r).as_deref(), Some("self comment"));
    }

    /// A GitHub App comments as `name[bot]`; comparing that against a bare
    /// BOT_NAME never matched, which is how the loop shipped.
    #[test]
    fn self_comment_ignored_via_bot_suffix_login() {
        let p = payload(json!({
            "body": "@xero-team-bot ping",
            "user": {"login": "xero-team-bot[bot]", "type": "Bot"}
        }));
        let r = route_event(&cfg(), "issue_comment", &p);
        assert_eq!(ignored_reason(&r).as_deref(), Some("self comment"));
    }

    /// The guard must not swallow humans (or other bots) with similar names.
    #[test]
    fn similar_login_not_treated_as_self() {
        for (login, kind) in [
            ("xero-team-bot-helper", "User"),
            ("xero-team-bot-helper[bot]", "Bot"),
            ("alice", "User"),
        ] {
            let p = payload(json!({
                "body": "@xero-team-bot ping",
                "user": {"login": login, "type": kind}
            }));
            let r = route_event(&cfg(), "issue_comment", &p);
            assert!(
                matches!(r, Routing::Act(_)),
                "{login} must not be treated as the bot itself, got {r:?}"
            );
        }
    }

    /// A comment with nothing to run but something to say must still be
    /// dispatched, or the diagnostic never reaches the PR.
    #[test]
    fn typo_only_comment_is_dispatched_to_be_answered() {
        let p = payload(json!({
            "body": "@xero-team-bot reviwe",
            "user": {"login": "alice", "type": "User"}
        }));
        match route_event(&cfg(), "issue_comment", &p) {
            Routing::Act(Work::Comment {
                commands,
                diagnostics,
                ..
            }) => {
                assert!(commands.is_empty(), "nothing should run");
                assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
                assert!(diagnostics[0].contains("review"), "{diagnostics:?}");
            }
            other => panic!("expected Act(Comment), got {other:?}"),
        }
    }

    /// Prose is still dropped without a round trip to GitHub.
    #[test]
    fn prose_comment_is_ignored_without_work() {
        for body in [
            "@xero-team-bot 谢谢!🎉",
            "@xero-team-bot 这个 PR 很好",
            "cc @xero-team-bot about this",
            "看起来不错",
        ] {
            let p = payload(json!({"body": body, "user": {"login": "alice", "type": "User"}}));
            let r = route_event(&cfg(), "issue_comment", &p);
            assert_eq!(
                ignored_reason(&r).as_deref(),
                Some("no command"),
                "for {body:?}"
            );
        }
    }

    /// Bodies that used to panic the parser mid-codepoint must route cleanly.
    /// A panic here killed the webhook response, and GitHub redelivers on
    /// failure — so one bad comment became a loop.
    #[test]
    fn pathological_body_routes_cleanly() {
        for body in [
            "@xero-team-bot \u{212A} x",
            "@xero-team-bot \u{2126}\u{2126} 中文",
            "@xero-team-bot cc \u{130} @alice",
            "@xero-team-bot 谢谢!🎉",
            "r? \u{212B}",
        ] {
            let p = payload(json!({"body": body, "user": {"login": "alice", "type": "User"}}));
            // The assertion is that this returns at all rather than unwinding.
            let _ = route_event(&cfg(), "issue_comment", &p);
        }
    }
}
