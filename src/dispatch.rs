//! Webhook dispatch, shared by the axum server's /webhook handler.

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
            // bad comment becomes a loop. Contain it at the entry point.
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
            // What the commenter wrote is the weaker of the two language
            // signals — the PR's own commits decide — but it's the only one
            // available without an API call, so it travels as the fallback.
            let comment_lang = crate::lang::detect(&comment_body);
            let diagnostics = parsed.diagnostics;

            // A comment can be worth answering without containing a command:
            // `@bot reviwe` produces nothing to run and one thing to say. Saying
            // it is the point — silently dropping near-misses is what left users
            // believing a mistyped command had worked.
            //
            // A bare command with no mention (`review`, `r+`) is the other
            // case worth carrying: once a session has been opened on this
            // issue with one real `@bot` command, the session's commands
            // don't need the mention anymore. Whether that session is in
            // fact open is only knowable from the API, so the candidate
            // travels in the work item and is checked when it executes.
            let bare = if parsed.commands.is_empty() && diagnostics.is_empty() {
                crate::commands::bare_command_candidate(&comment_body)
            } else {
                None
            };
            if parsed.commands.is_empty() && diagnostics.is_empty() && bare.is_none() {
                return Routing::Respond(serde_json::json!({"ignored": "no command"}));
            }
            // Issues used to be rejected wholesale, which is how `@bot claim`
            // in an issue came back as `{"ignored":"not a PR"}` — the comment
            // right here said labels and assignees work on issues too, and
            // they do: GitHub serves both from the issues API. Only turn the
            // delivery away when there is nothing on it that an issue can do;
            // otherwise dispatch, and let each command answer for itself.
            if !is_pr && diagnostics.is_empty() && parsed.commands.iter().all(|c| c.requires_pr()) {
                return Routing::Respond(serde_json::json!({"ignored": "not a PR"}));
            }

            Routing::Act(Work::Comment {
                repo,
                pr_number,
                installation_id,
                commenter,
                pr_author,
                is_pr,
                commands: parsed.commands,
                diagnostics,
                comment_lang,
                bare,
            })
        }
        WebhookEvent::PullRequest {
            repo,
            pr_number,
            action,
            installation_id,
        } => {
            // `closed` exists only for the merge queue (dequeue on close);
            // without it the action is noise and stays ignored — byte-for-byte
            // the pre-bors behavior.
            if action == "closed" {
                if cfg.bors_enabled {
                    return Routing::Act(Work::BorsPrClosed {
                        repo,
                        pr_number,
                        installation_id,
                    });
                }
                return Routing::Respond(
                    serde_json::json!({"ignored": "closed: merge queue disabled"}),
                );
            }
            Routing::Act(Work::RebaseCheck {
                repo,
                pr_number,
                action,
                installation_id,
            })
        }
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
        WebhookEvent::PrReview {
            repo,
            pr_number,
            action,
            state,
            reviewer,
            reviewer_is_bot,
            via_app_id,
            installation_id,
        } => {
            // The r+ relay posts its APPROVE *as this App*, and GitHub then
            // delivers the very event we're handling. Enqueueing on our own
            // review would be an infinite loop — r+ fires a review, the
            // review fires this route, the route fires r+. Two independent
            // checks, same pattern as the self-comment guards above: the App
            // id, which is name-independent, and the login, which needs the
            // `[bot]` suffix stripped.
            if let (Some(via), Ok(own)) = (via_app_id, cfg.app_id.parse::<i64>()) {
                if via == own {
                    return Routing::Respond(serde_json::json!({"ignored": "self review"}));
                }
            }
            if reviewer_is_bot && !reviewer.is_empty() {
                let own = normalize_login(&cfg.bot_name);
                let configured = normalize_login(&cfg.app_slug);
                let them = normalize_login(&reviewer);
                if them == own || (!configured.is_empty() && them == configured) {
                    return Routing::Respond(serde_json::json!({"ignored": "self review"}));
                }
            }
            if action != "submitted" {
                return Routing::Respond(serde_json::json!({"ignored": "not submitted"}));
            }
            // COMMENTED reviews say nothing about merge-worthiness; DISMISSED
            // is an action, not a submitted state. Only the two verdicts that
            // gate a merge are worth a work item.
            if !matches!(state.as_str(), "APPROVED" | "CHANGES_REQUESTED") {
                return Routing::Respond(serde_json::json!({"ignored": "state not a verdict"}));
            }
            if !cfg.bors_enabled {
                return Routing::Respond(serde_json::json!({"ignored": "merge queue disabled"}));
            }
            Routing::Act(Work::PrReview {
                repo,
                pr_number,
                state,
                reviewer,
                installation_id,
            })
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
        /// False for an issue. `pr_number` is the issue number either way —
        /// GitHub numbers them from one sequence and serves both from the
        /// issues API.
        is_pr: bool,
        commands: Vec<crate::commands::Command>,
        /// What couldn't be understood; may be non-empty even when `commands`
        /// is empty. Carried unrendered because the wording depends on a
        /// language that isn't known until the PR's commits have been read.
        diagnostics: Vec<crate::commands::diag::Diagnostic>,
        /// The language of the triggering comment, if it said. Only consulted
        /// when the commits don't settle it.
        comment_lang: Option<crate::lang::Lang>,
        /// A command this comment *could* be, if its author's session on this
        /// issue is open (see [`crate::commands::bare_command_candidate`]).
        /// `None` for everything else, so the session check below — one
        /// paginated API call — only ever runs for comments shaped like a
        /// bare command.
        bare: Option<crate::commands::Command>,
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
    /// A human's review verdict arrived; drive the queue (approve → enqueue,
    /// changes-requested → dequeue). Only APPROVED/CHANGES_REQUESTED pass
    /// routing, so the state is one of those two by construction.
    PrReview {
        repo: String,
        pr_number: i64,
        state: String,
        reviewer: String,
        installation_id: i64,
    },
    /// A PR (or anything shaped like one) was closed; if it was queued or in
    /// the batch, take it out. `merged` is decided at execution — the queue
    /// only needs to know the PR is gone.
    BorsPrClosed {
        repo: String,
        pr_number: i64,
        installation_id: i64,
    },
}

/// Has `commenter` ever commanded the bot on this issue?
///
/// A session opens the moment one of the user's comments parses into
/// commands with the bot addressed — a mention plus a verb, or one of the
/// mention-free forms the parser accepts anywhere (`r? @user`, `?r`). It
/// never closes: the PR's comment history is the state, and GitHub keeps it
/// longer than any cache would.
///
/// The scan reads only the requester's own comments; a colleague's commands
/// don't open someone else's session. First hit wins, so on an active PR the
/// usual cost is one paginated read of recent comments.
async fn session_open(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    issue: i64,
    commenter: &str,
) -> Result<bool, String> {
    let comments = gh
        .list_issue_comments(repo, issue)
        .await
        .map_err(|e| e.to_string())?;
    for c in &comments {
        let author = c
            .pointer("/user/login")
            .and_then(|l| l.as_str())
            .unwrap_or("");
        if !author.eq_ignore_ascii_case(commenter) {
            continue;
        }
        let Some(body) = c.get("body").and_then(|b| b.as_str()) else {
            continue;
        };
        if !parse_commands(&cfg.bot_name, body).commands.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
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
            is_pr,
            commands,
            diagnostics,
            comment_lang,
            bare,
        } => {
            let gh = Client::installation_resolved(cfg, installation_id)
                .await
                .map_err(|e| format!("installation client: {e}"))?;

            // An issue has no commits, and asking for them is a guaranteed 404
            // per comment, so the comment is the only signal there is.
            let lang = if is_pr {
                crate::lang::for_pr(&gh, &repo, pr_number, comment_lang).await
            } else {
                comment_lang.unwrap_or_default()
            };

            // The no-mention path. The comment carried a bare command
            // candidate and nothing else the parser recognized, so this is
            // the one point where "does this user have a session here?" is
            // worth an API call: scan their comments on this issue for one
            // that parsed as commands when addressed to the bot. Their
            // earlier comment is the session opener; the flag lives in
            // GitHub, so it survives restarts and needs no database.
            let mut commands = commands;
            if commands.is_empty() && diagnostics.is_empty() {
                if let Some(cmd) = bare {
                    match session_open(&gh, cfg, &repo, pr_number, &commenter).await {
                        Ok(true) => commands.push(cmd),
                        Ok(false) => {
                            tracing::info!(
                                "bare command by @{commenter} on {repo}#{pr_number} ignored: \
                                 no session (mention @{bot} once to open one)",
                                bot = cfg.bot_name
                            );
                            // The comment *looks* like a command, so silence
                            // reads as a broken bot. One line teaching the
                            // one-mention rule, in the PR's language.
                            let bot = &cfg.bot_name;
                            let body = match lang {
                                crate::lang::Lang::En => format!(
                                    "💡 To run commands without mentioning me, run one \
command *with* a mention first — `@{bot} help` — here on this PR. After that, bare \
commands like yours work without the mention."
                                ),
                                crate::lang::Lang::Zh => format!(
                                    "💡 想不 @ 直接下命令,请先在本 PR 上带 @ 执行一次命令 \
(如 `@{bot} help`)。之后本 PR 上即可免 @ 使用指令。"
                                ),
                            };
                            let _ = gh.post_issue_comment(&repo, pr_number, &body).await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "session check for @{commenter} on {repo}#{pr_number}: {e}"
                            );
                        }
                    }
                }
            }

            let ctx = CommentContext {
                repo: repo.clone(),
                pr_number,
                commenter,
                pr_author,
                installation_id,
                is_pr,
                lang,
            };
            // Rendered here, not at routing time: `handle_comment` takes plain
            // strings so it needn't know the parser's types, and the wording
            // needs the language that only this side of the queue knows.
            let diagnostics: Vec<String> = diagnostics.iter().map(|d| d.message(lang)).collect();
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
            let lang = crate::lang::for_pr(&gh, &repo, pr_number, None).await;
            let status = crate::codeql::run_codeql_report(&gh, cfg, &repo, pr_number, lang).await;
            tracing::info!("codeql report {repo}#{pr_number}: {status}");
            Ok(())
        }
        Work::PrReview {
            repo,
            pr_number,
            state,
            reviewer,
            installation_id,
        } => {
            let gh = Client::installation_resolved(cfg, installation_id)
                .await
                .map_err(|e| format!("installation client: {e}"))?;
            let status =
                crate::bors::handle_review(&gh, cfg, &repo, pr_number, &state, &reviewer).await;
            tracing::info!("bors review {repo}#{pr_number} ({state} by {reviewer}): {status}");
            Ok(())
        }
        Work::BorsPrClosed {
            repo,
            pr_number,
            installation_id,
        } => {
            let gh = Client::installation(cfg, installation_id, "")
                .map_err(|e| format!("installation client: {e}"))?;
            let status = crate::bors::handle_pr_closed(&gh, cfg, &repo, pr_number).await;
            tracing::info!("bors closed {repo}#{pr_number}: {status}");
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

    /// The same, on an issue: GitHub omits `issue.pull_request` entirely.
    fn issue_payload(body: &str) -> serde_json::Value {
        json!({
            "action": "created",
            "installation": {"id": 42},
            "repository": {"full_name": "Xero-Team/xero-bot"},
            "issue": {"number": 1, "user": {"login": "alice"}},
            "comment": {"body": body, "user": {"login": "bob", "type": "User"}}
        })
    }

    fn ignored_reason(r: &Routing) -> Option<String> {
        match r {
            Routing::Respond(v) => v.get("ignored").and_then(|s| s.as_str()).map(String::from),
            Routing::Act(_) => None,
        }
    }

    /// A human's `pull_request_review` payload; `review` is merged in.
    fn review_payload(review: serde_json::Value) -> serde_json::Value {
        json!({
            "action": "submitted",
            "installation": {"id": 42},
            "repository": {"full_name": "Xero-Team/xero-bot"},
            "pull_request": {"number": 7},
            "review": review
        })
    }

    fn bors_cfg() -> Config {
        let mut c = cfg();
        c.bors_enabled = true;
        c
    }

    /// A human APPROVED is the queue's second trigger (the first is r+).
    #[test]
    fn human_approval_routes_to_bors_when_enabled() {
        let p = review_payload(json!({"state": "APPROVED", "user": {"login": "alice"}}));
        match route_event(&bors_cfg(), "pull_request_review", &p) {
            Routing::Act(Work::PrReview {
                state, reviewer, ..
            }) => {
                assert_eq!(state, "APPROVED");
                assert_eq!(reviewer, "alice");
            }
            other => panic!("expected Act(PrReview), got {other:?}"),
        }
    }

    /// CHANGES_REQUESTED dequeues a member; the state travels raw.
    #[test]
    fn changes_requested_routes_to_bors_when_enabled() {
        let p = review_payload(json!({"state": "CHANGES_REQUESTED", "user": {"login": "bob"}}));
        let r = route_event(&bors_cfg(), "pull_request_review", &p);
        assert!(matches!(r, Routing::Act(Work::PrReview { .. })), "{r:?}");
    }

    /// COMMENTED reviews don't gate a merge — nothing to act on.
    #[test]
    fn commented_review_is_ignored() {
        let p = review_payload(json!({"state": "COMMENTED", "user": {"login": "alice"}}));
        assert_eq!(
            ignored_reason(&route_event(&bors_cfg(), "pull_request_review", &p)).as_deref(),
            Some("state not a verdict")
        );
    }

    /// The r+ relay posts its APPROVE as the App; GitHub delivers that as this
    /// very event. Without the guard the route would enqueue on it, and r+
    /// would fire again — an infinite loop of reviews. The App-id check runs
    /// even when the reviewer login isn't ours (e.g. a misconfigured BOT_NAME).
    #[test]
    fn self_review_ignored_via_app_id() {
        let p = review_payload(json!({
            "state": "APPROVED",
            "user": {"login": "whatever[bot]", "type": "Bot"},
            "performed_via_github_app": {"id": 4768775}
        }));
        let r = route_event(&bors_cfg(), "pull_request_review", &p);
        assert_eq!(ignored_reason(&r).as_deref(), Some("self review"));
    }

    /// The login check covers the case the App id can't: a review posted
    /// through the API on behalf of the App's identity.
    #[test]
    fn self_review_ignored_via_bot_login() {
        let p = review_payload(json!({
            "state": "APPROVED",
            "user": {"login": "xero-team-bot[bot]", "type": "Bot"}
        }));
        let r = route_event(&bors_cfg(), "pull_request_review", &p);
        assert_eq!(ignored_reason(&r).as_deref(), Some("self review"));
    }

    /// A similarly-named human must not be swallowed by the guard.
    #[test]
    fn similar_reviewer_not_treated_as_self() {
        let p = review_payload(json!({
            "state": "APPROVED",
            "user": {"login": "xero-team-bot-helper", "type": "User"}
        }));
        let r = route_event(&bors_cfg(), "pull_request_review", &p);
        assert!(matches!(r, Routing::Act(_)), "{r:?}");
    }

    /// Without BORS_ENABLED the queue must be entirely inert: the old
    /// deployments' behavior is byte-for-byte preserved.
    #[test]
    fn review_events_ignored_without_bors() {
        for state in ["APPROVED", "CHANGES_REQUESTED"] {
            let p = review_payload(json!({"state": state, "user": {"login": "alice"}}));
            assert_eq!(
                ignored_reason(&route_event(&cfg(), "pull_request_review", &p)).as_deref(),
                Some("merge queue disabled")
            );
        }
    }

    /// `pull_request closed` feeds the queue's dequeue-on-close; without the
    /// feature it stays ignored exactly as before.
    #[test]
    fn pr_closed_routes_only_with_bors() {
        let payload = json!({
            "action": "closed",
            "installation": {"id": 42},
            "repository": {"full_name": "Xero-Team/xero-bot"},
            "pull_request": {"number": 7}
        });
        let r = route_event(&bors_cfg(), "pull_request", &payload);
        assert!(
            matches!(r, Routing::Act(Work::BorsPrClosed { .. })),
            "{r:?}"
        );
        let r = route_event(&cfg(), "pull_request", &payload);
        assert_eq!(
            ignored_reason(&r).as_deref(),
            Some("closed: merge queue disabled")
        );
    }

    /// The rebase path keeps its actions: `closed` must not have broadened
    /// what reaches it.
    #[test]
    fn synchronize_still_routes_to_rebase() {
        let payload = json!({
            "action": "synchronize",
            "installation": {"id": 42},
            "repository": {"full_name": "Xero-Team/xero-bot"},
            "pull_request": {"number": 7}
        });
        let r = route_event(&bors_cfg(), "pull_request", &payload);
        assert!(matches!(r, Routing::Act(Work::RebaseCheck { .. })), "{r:?}");
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
                // Carried unrendered, so the assertion is on the message the
                // PR would actually get once its language is known.
                let msg = diagnostics[0].message(crate::lang::Lang::En);
                assert!(msg.contains("review"), "{msg}");
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

    /// Reported by a user: `@bot claim` in an issue came back
    /// `{"ignored":"not a PR"}`. Labels, assignees and comments are the issues
    /// API — the same endpoints either way — so these belong on an issue.
    #[test]
    fn issue_commands_are_dispatched() {
        for body in [
            "@xero-team-bot claim",
            "@xero-team-bot unclaim",
            "@xero-team-bot assign @alice",
            "@xero-team-bot label +bug -wip",
            "@xero-team-bot cc @alice",
            "@xero-team-bot ready",
            "@xero-team-bot author",
            "@xero-team-bot blocked",
            "@xero-team-bot ping",
            "@xero-team-bot help",
            "r? @alice",
            "?r @alice",
        ] {
            let r = route_event(&cfg(), "issue_comment", &issue_payload(body));
            match r {
                Routing::Act(Work::Comment { is_pr, .. }) => {
                    assert!(!is_pr, "the handler must know it's an issue: {body}")
                }
                other => panic!("expected Act(Comment) for {body:?}, got {other:?}"),
            }
        }
    }

    /// A comment with nothing an issue can do is still turned away at the door,
    /// so no installation token is minted to say so.
    #[test]
    fn pr_only_commands_on_an_issue_are_still_refused() {
        for body in [
            "@xero-team-bot review",
            "@xero-team-bot codeql",
            "@xero-team-bot r+",
            "@xero-team-bot r-",
            "@xero-team-bot review; codeql",
        ] {
            let r = route_event(&cfg(), "issue_comment", &issue_payload(body));
            assert_eq!(
                ignored_reason(&r).as_deref(),
                Some("not a PR"),
                "for {body:?}"
            );
        }
    }

    /// One runnable command carries the delivery; the PR-only ones then get
    /// their own reply from the handler rather than taking the rest down.
    #[test]
    fn mixed_commands_on_an_issue_are_dispatched() {
        let r = route_event(
            &cfg(),
            "issue_comment",
            &issue_payload("@xero-team-bot claim; review"),
        );
        match r {
            Routing::Act(Work::Comment { commands, .. }) => assert_eq!(commands.len(), 2),
            other => panic!("expected Act(Comment), got {other:?}"),
        }
    }

    /// A typo in an issue deserves the same answer as a typo in a PR — the
    /// diagnostic needs no PR to be worth posting.
    #[test]
    fn diagnostics_alone_are_dispatched_on_an_issue() {
        let r = route_event(
            &cfg(),
            "issue_comment",
            &issue_payload("@xero-team-bot reviwe"),
        );
        match r {
            Routing::Act(Work::Comment { diagnostics, .. }) => {
                assert_eq!(diagnostics.len(), 1, "{diagnostics:?}")
            }
            other => panic!("expected Act(Comment), got {other:?}"),
        }
    }

    /// Prose is still dropped, PR or issue.
    #[test]
    fn prose_in_an_issue_is_ignored() {
        let r = route_event(
            &cfg(),
            "issue_comment",
            &issue_payload("@xero-team-bot 谢谢!"),
        );
        assert_eq!(ignored_reason(&r).as_deref(), Some("no command"));
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

    // ---- bare-command sessions -------------------------------------------

    use crate::commands::Command;

    /// A bare, mention-less comment that reads as a command is not answered
    /// at routing time — it is carried as a candidate for the session check.
    /// A mention-form comment runs directly and carries no candidate.
    #[test]
    fn a_bare_command_is_carried_as_a_candidate() {
        for (body, want) in [
            ("review", Some(Command::Review)),
            ("ready", Some(Command::Ready)),
            ("codeql", Some(Command::Codeql)),
            ("r+", Some(Command::Approve { on_behalf_of: None })),
            ("r-", Some(Command::Reject)),
            ("- review", Some(Command::Review)), // list bullet before the verb
        ] {
            let p = payload(json!({"body": body, "user": {"login": "alice", "type": "User"}}));
            match route_event(&cfg(), "issue_comment", &p) {
                Routing::Act(Work::Comment { commands, bare, .. }) => {
                    assert_eq!(bare, want, "candidate for {body:?}");
                    assert!(
                        commands.is_empty(),
                        "{body:?} must wait for the session check"
                    );
                }
                other => panic!("expected Act(Comment) for {body:?}, got {other:?}"),
            }
        }
        // The mention form is executed at routing time; no session needed.
        let p = payload(json!({
            "body": "@xero-team-bot review",
            "user": {"login": "alice", "type": "User"}
        }));
        match route_event(&cfg(), "issue_comment", &p) {
            Routing::Act(Work::Comment { commands, bare, .. }) => {
                assert_eq!(commands.len(), 1);
                assert!(bare.is_none());
            }
            other => panic!("expected Act(Comment), got {other:?}"),
        }
    }

    /// Prose that merely contains a whitelisted word is not a candidate.
    /// The line has to open with it.
    #[test]
    fn prose_is_not_a_bare_command() {
        for body in [
            "这个 review 不错",
            "I think we are ready to merge",
            "claim 是什么意思?",
            "please review this again",
            "```\nreview\n```",
            "> review",
        ] {
            let p = payload(json!({"body": body, "user": {"login": "alice", "type": "User"}}));
            match route_event(&cfg(), "issue_comment", &p) {
                Routing::Respond(v) => {
                    assert_eq!(
                        v.get("ignored").and_then(|s| s.as_str()),
                        Some("no command")
                    )
                }
                Routing::Act(Work::Comment { bare, .. }) => {
                    assert!(
                        bare.is_none(),
                        "{body:?} must not be a candidate, got {bare:?}"
                    )
                }
                other => panic!("expected a plain response for {body:?}, got {other:?}"),
            }
        }
    }

    /// `bare_command_candidate` reads the first line only and skips leading
    /// punctuation, so a comment whose first line is prose with the verb on
    /// a later line stays quiet — that's the mixed-comment interference the
    /// session feature was built to avoid.
    #[test]
    fn only_the_first_line_opens_a_bare_command() {
        for body in [
            "looks good\nreview", // prose first
            "r? @alice\nreview",  // already a command; not the bare path
        ] {
            assert!(
                crate::commands::bare_command_candidate(body).is_none() || body.starts_with("r?"),
                "{body:?}"
            );
        }
        // First line wins: everything after is ignored.
        assert_eq!(
            crate::commands::bare_command_candidate("review\nclaim 是什么意思"),
            Some(Command::Review)
        );
    }
}
