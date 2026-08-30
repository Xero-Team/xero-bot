//! Shared webhook dispatch used by both entry modes (Vercel function and
//! self-hosted axum server).

use serde_json::Value;

use crate::commands::parse_commands;
use crate::config::Config;
use crate::github::Client;
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
            app_slug,
            pr_author,
            is_pr,
        } => {
            // don't react to the bot's own comments
            let self_login = if app_slug.is_empty() {
                cfg.bot_name.to_lowercase()
            } else {
                app_slug.to_lowercase()
            };
            if !commenter.is_empty() && commenter.to_lowercase() == self_login {
                return Routing::Respond(serde_json::json!({"ignored": "self comment"}));
            }

            let commands = parse_commands(&cfg.bot_name, &comment_body);
            if commands.is_empty() {
                return Routing::Respond(serde_json::json!({"ignored": "no command"}));
            }
            if !is_pr {
                // keep it simple and require a PR (matching the Python bot)
                return Routing::Respond(serde_json::json!({"ignored": "not a PR"}));
            }

            Routing::Act(Work::Comment {
                repo,
                pr_number,
                installation_id,
                app_slug,
                commenter,
                pr_author,
                commands: commands.into_iter().map(|c| c.command).collect(),
            })
        }
        // pull_request / labeled events: rebase + codeql wiring lands later
        other => Routing::Respond(serde_json::json!({"ignored": format!("{other:?}")})),
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
        app_slug: String,
        commenter: String,
        pr_author: String,
        commands: Vec<crate::commands::Command>,
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
            app_slug,
            commenter,
            pr_author,
            commands,
        } => {
            let gh = Client::installation(cfg, installation_id, &app_slug)
                .map_err(|e| format!("installation client: {e}"))?;
            let ctx = CommentContext {
                repo: repo.clone(),
                pr_number,
                commenter,
                pr_author,
                app_slug,
                installation_id,
            };
            let results = handle_comment(&gh, cfg, &ctx, commands).await;
            tracing::info!("comment commands on {repo}#{pr_number}: {results:?}");
            Ok(())
        }
    }
}
