//! Rebase detection: watch PR push/reopen events and a periodic sweep,
//! maintain the `needs-rebase` label, comment a reminder when conflicted.
//!
//! State lives entirely in the label — no database.

use serde_json::Value;

use crate::config::Config;
use crate::github::Client;

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

fn reminder_comment(repo: &str, base_branch: &str) -> String {
    format!(
        "⚠️ **此 PR 已与目标分支冲突,需要 rebase。**\n\n\
```bash\ngit fetch origin {base_branch}\ngit rebase origin/{base_branch}\n# 解决冲突后\ngit push --force-with-lease\n```\n\n\
冲突解决后会自动移除 `needs-rebase` 标签。\n\
_(_{repo} · 由 xero-bot 自动检测)_"
    )
}

fn resolved_comment() -> &'static str {
    "✅ 冲突已解决,移除 `needs-rebase` 标签。"
}

/// Check one PR and act. Returns a short status string.
pub async fn check_pr(gh: &Client, cfg: &Config, repo: &str, pr_number: i64) -> String {
    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(p) => p,
        Err(e) => return format!("fetch-pr-error: {e}"),
    };
    // skip closed PRs
    if pr.get("state").and_then(|s| s.as_str()) != Some("open") {
        return "closed".into();
    }
    let mergeable = pr.get("mergeable").and_then(|m| m.as_bool());
    let labels = gh.list_labels(repo, pr_number).await.unwrap_or_default();
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
                return format!("add-label-error: {e}");
            }
            let _ = gh
                .post_issue_comment(repo, pr_number, &reminder_comment(repo, &base_branch))
                .await;
            "flagged".into()
        }
        RebaseDecision::AlreadyFlagged => "already-flagged".into(),
        RebaseDecision::Clear => {
            if let Err(e) = gh
                .remove_label(repo, pr_number, &cfg.label_needs_rebase)
                .await
            {
                return format!("remove-label-error: {e}");
            }
            let _ = gh
                .post_issue_comment(repo, pr_number, resolved_comment())
                .await;
            "cleared".into()
        }
        RebaseDecision::Noop => "noop".into(),
        RebaseDecision::Unknown => "unknown".into(),
    }
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
        let c = reminder_comment("o/r", "main");
        assert!(c.contains("rebase"));
        assert!(c.contains("origin/main"));
        assert!(c.contains("needs-rebase"));
    }
}
