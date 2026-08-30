//! Rebase detection: watch PR push/reopen events and a periodic sweep,
//! maintain the `needs-rebase` label, comment a reminder when conflicted.
//!
//! State lives entirely in the label — no database.

use serde_json::Value;

use crate::config::Config;
use crate::github::Client;
use crate::lang::Lang;
use crate::t;

/// What to do about the needs-rebase label, given (mergeable, has_label).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RebaseDecision {
    /// conflict, not yet flagged → add label + comment
    Flag,
    /// conflict, already flagged → nothing (avoid spam)
    AlreadyFlagged,
    /// clean, flagged before → remove label + note resolved
    Clear,
    /// clean, not flagged → nothing
    Noop,
    /// mergeable state unknown (GitHub still computing) → check later via sweep
    Unknown,
}

pub fn decide(mergeable: Option<bool>, has_label: bool) -> RebaseDecision {
    match mergeable {
        Some(false) => {
            if has_label {
                RebaseDecision::AlreadyFlagged
            } else {
                RebaseDecision::Flag
            }
        }
        Some(true) => {
            if has_label {
                RebaseDecision::Clear
            } else {
                RebaseDecision::Noop
            }
        }
        None => RebaseDecision::Unknown,
    }
}

fn reminder_comment(repo: &str, base_branch: &str, lang: Lang) -> String {
    t!(
        lang,
        "⚠️ **This PR conflicts with its base branch and needs a rebase.**\n\n\
```bash\ngit fetch origin {base_branch}\ngit rebase origin/{base_branch}\n# after resolving the conflicts\ngit push --force-with-lease\n```\n\n\
The `needs-rebase` label is removed automatically once the conflicts are gone.\n\
_({repo} · detected by xero-bot)_",
        "⚠️ **此 PR 已与目标分支冲突,需要 rebase。**\n\n\
```bash\ngit fetch origin {base_branch}\ngit rebase origin/{base_branch}\n# 解决冲突后\ngit push --force-with-lease\n```\n\n\
冲突解决后会自动移除 `needs-rebase` 标签。\n\
_({repo} · 由 xero-bot 自动检测)_"
    )
}

fn resolved_comment(lang: Lang) -> &'static str {
    lang.pick(
        "✅ Conflicts resolved; removing the `needs-rebase` label.",
        "✅ 冲突已解决,移除 `needs-rebase` 标签。",
    )
}

/// What actually happened to one PR.
///
/// A string was fine for the log line and useless to the sweep, which had to
/// compare against `"flagged"` and had no way to notice a failure at all — so it
/// reported "0 flagged, 0 errors" for a round in which every single check had
/// failed. `Display` reproduces the old strings exactly, so the log line and
/// existing assertions are unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Label added and the reminder posted.
    Flagged,
    /// Conflicted and already labelled — stay quiet.
    AlreadyFlagged,
    /// Label removed; the conflict is gone.
    Cleared,
    /// Clean and unlabelled.
    Noop,
    /// GitHub is still computing mergeability; the next sweep decides.
    Unknown,
    /// The PR isn't open.
    Closed,
    /// Something failed. Carries the already-prefixed message.
    Error(String),
}

impl std::fmt::Display for CheckOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckOutcome::Flagged => f.write_str("flagged"),
            CheckOutcome::AlreadyFlagged => f.write_str("already-flagged"),
            CheckOutcome::Cleared => f.write_str("cleared"),
            CheckOutcome::Noop => f.write_str("noop"),
            CheckOutcome::Unknown => f.write_str("unknown"),
            CheckOutcome::Closed => f.write_str("closed"),
            CheckOutcome::Error(msg) => f.write_str(msg),
        }
    }
}

/// Check one PR and act.
pub async fn check_pr(gh: &Client, cfg: &Config, repo: &str, pr_number: i64) -> CheckOutcome {
    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(p) => p,
        Err(e) => return CheckOutcome::Error(format!("fetch-pr-error: {e}")),
    };
    // skip closed PRs
    if pr.get("state").and_then(|s| s.as_str()) != Some("open") {
        return CheckOutcome::Closed;
    }
    let mergeable = pr.get("mergeable").and_then(|m| m.as_bool());
    // An API failure here used to become `has_label = false`, i.e. a guess that
    // we had never flagged this PR — which is why a conflicted PR got the same
    // reminder comment again on every sweep for as long as the endpoint misbehaved.
    let labels = match gh.list_labels(repo, pr_number).await {
        Ok(l) => l,
        Err(e) => return CheckOutcome::Error(format!("list-labels-error: {e}")),
    };
    let has_label = labels.iter().any(|l| l == &cfg.label_needs_rebase);
    let base_branch = pr
        .pointer("/base/ref")
        .and_then(|r| r.as_str())
        .unwrap_or("main")
        .to_string();

    match decide(mergeable, has_label) {
        RebaseDecision::Flag => {
            if let Err(e) = gh
                .add_labels(repo, pr_number, &[cfg.label_needs_rebase.clone()])
                .await
            {
                return CheckOutcome::Error(format!("add-label-error: {e}"));
            }
            // Resolved here rather than at the top of the function: the sweep
            // walks every open PR of every repo, and only two of the five
            // outcomes say anything, so the commits are worth an extra request
            // only once we know we're about to speak.
            let lang = crate::lang::for_pr(gh, repo, pr_number, None).await;
            // The label is the state, and it is already set — so this is
            // flagged either way. Logged rather than propagated for that reason.
            if let Err(e) = gh
                .post_issue_comment(repo, pr_number, &reminder_comment(repo, &base_branch, lang))
                .await
            {
                tracing::warn!("rebase reminder on {repo}#{pr_number}: {e}");
            }
            CheckOutcome::Flagged
        }
        RebaseDecision::AlreadyFlagged => CheckOutcome::AlreadyFlagged,
        RebaseDecision::Clear => {
            if let Err(e) = gh
                .remove_label(repo, pr_number, &cfg.label_needs_rebase)
                .await
            {
                return CheckOutcome::Error(format!("remove-label-error: {e}"));
            }
            let lang = crate::lang::for_pr(gh, repo, pr_number, None).await;
            if let Err(e) = gh
                .post_issue_comment(repo, pr_number, resolved_comment(lang))
                .await
            {
                tracing::warn!("rebase resolved note on {repo}#{pr_number}: {e}");
            }
            CheckOutcome::Cleared
        }
        RebaseDecision::Noop => CheckOutcome::Noop,
        RebaseDecision::Unknown => CheckOutcome::Unknown,
    }
}

/// Handle a pull_request synchronize/reopened/opened webhook: wait for GitHub
/// to compute mergeability, then check.
pub async fn handle_push_event(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    action: &str,
) {
    if action == "opened" {
        // on open, mergeable is almost always still being computed; the daily
        // sweep will catch genuine conflicts. Skip to reduce noise.
        return;
    }
    tokio::time::sleep(std::time::Duration::from_secs(cfg.rebase_check_delay_secs)).await;
    let status = check_pr(gh, cfg, repo, pr_number).await;
    tracing::info!("rebase check {repo}#{pr_number} ({action}): {status}");
}

/// Sweep every installation's repositories for conflicted PRs.
/// App-level client lists installations; each repo gets an installation client.
pub async fn sweep(cfg: &Config) -> String {
    if !cfg.rebase_sweep_enabled {
        return "sweep disabled".into();
    }
    let app = match crate::github::Client::app_client(cfg) {
        Ok(c) => c,
        Err(e) => return format!("app-client-error: {e}"),
    };

    // List installations. Paginated and classified via the shared helper: this
    // call used to bypass `classify_octo_error`, so a 401 from a bad key was
    // reported as an opaque transport error, and an App on more than 100
    // installations swept only the first page.
    let installations: Vec<Value> =
        match crate::github::paginate(&app, "/app/installations?per_page=100").await {
            Ok(v) => v,
            Err(e) => return format!("installations-error: {e}"),
        };

    let mut checked = 0usize;
    let mut flagged = 0usize;
    let mut cleared = 0usize;
    let mut errors = 0usize;

    for inst in &installations {
        let Some(inst_id) = inst.get("id").and_then(|i| i.as_i64()) else {
            continue;
        };
        // `/app/installations` does carry app_slug, but resolve it centrally so
        // every code path agrees on our identity.
        let Ok(gh) = crate::github::Client::installation_resolved(cfg, inst_id).await else {
            errors += 1;
            continue;
        };

        let repos = match crate::github::Client::installation_repositories_via(&gh).await {
            Ok(r) => r,
            Err(_) => {
                errors += 1;
                continue;
            }
        };

        for repo in repos {
            let Ok(prs) = gh.open_prs(&repo).await else {
                errors += 1;
                continue;
            };
            for pr in prs {
                let Some(n) = pr.get("number").and_then(|n| n.as_i64()) else {
                    continue;
                };
                checked += 1;
                match check_pr(&gh, cfg, &repo, n).await {
                    CheckOutcome::Flagged => flagged += 1,
                    CheckOutcome::Cleared => cleared += 1,
                    // Counted at last: a sweep where every check failed used to
                    // read exactly like a sweep where nothing needed doing.
                    CheckOutcome::Error(msg) => {
                        tracing::warn!("rebase check {repo}#{n}: {msg}");
                        errors += 1;
                    }
                    _ => {}
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }

    let summary = format!(
        "sweep done: {checked} PRs checked, {flagged} flagged, {cleared} cleared, {errors} errors"
    );
    tracing::info!("{summary}");
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decision_matrix() {
        // conflict
        assert_eq!(decide(Some(false), false), RebaseDecision::Flag);
        assert_eq!(decide(Some(false), true), RebaseDecision::AlreadyFlagged);
        // clean
        assert_eq!(decide(Some(true), false), RebaseDecision::Noop);
        assert_eq!(decide(Some(true), true), RebaseDecision::Clear);
        // unknown
        assert_eq!(decide(None, false), RebaseDecision::Unknown);
        assert_eq!(decide(None, true), RebaseDecision::Unknown);
    }

    #[test]
    fn test_reminder_comment() {
        for lang in [Lang::En, Lang::Zh] {
            let c = reminder_comment("o/r", "main", lang);
            assert!(c.contains("rebase"), "{c}");
            assert!(c.contains("origin/main"), "{c}");
            assert!(c.contains("needs-rebase"), "{c}");
            assert!(c.contains("o/r"), "{c}");
        }
        // and no Chinese left in the English one
        let en = reminder_comment("o/r", "main", Lang::En);
        assert!(
            !en.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "{en}"
        );
    }
}
