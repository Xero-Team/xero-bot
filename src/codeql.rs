//! CodeQL quality report: read existing code-scanning alerts for the repo,
//! intersect them with the PR's changed files, post a report comment.
//!
//! This is the serverless-friendly approach — no CodeQL CLI execution. The
//! repo itself must already run CodeQL (default setup or codeql.yml workflow).

use serde_json::Value;

use crate::config::Config;
use crate::github::{Client, GhError};
use crate::lang::Lang;
use crate::review::md_cell;
use crate::t;

pub async fn run_codeql_report(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    lang: Lang,
) -> String {
    // The placeholder is courtesy, not the deliverable: if it fails the report
    // itself may still land, so log and carry on rather than aborting.
    if let Err(e) = gh
        .post_issue_comment(
            repo,
            pr_number,
            lang.pick(
                "🔍 Building the CodeQL quality report, one moment…",
                "🔍 正在生成 CodeQL 质量报告,稍候…",
            ),
        )
        .await
    {
        tracing::warn!("codeql placeholder comment on {repo}#{pr_number}: {e}");
    }

    match run_inner(gh, cfg, repo, pr_number, lang).await {
        Ok(status) => status,
        Err(e) => {
            let _ = gh
                .post_issue_comment(
                    repo,
                    pr_number,
                    &t!(
                        lang,
                        "## 🔍 CodeQL quality report\n\n❌ Failed: `{e}`",
                        "## 🔍 CodeQL 质量报告\n\n❌ 出错: `{e}`"
                    ),
                )
                .await;
            format!("error: {e}")
        }
    }
}

async fn run_inner(
    gh: &Client,
    _cfg: &Config,
    repo: &str,
    pr_number: i64,
    lang: Lang,
) -> Result<String, String> {
    // 1. alerts
    let alerts = match gh.code_scanning_alerts(repo).await {
        Ok(a) => a,
        Err(GhError::Api { status: 403, .. }) | Err(GhError::Api { status: 404, .. }) => {
            // Propagated, not swallowed: the setup instructions *are* the whole
            // answer in this branch, and dropping them left the user watching
            // the "one moment…" placeholder forever.
            let body = not_enabled_message(repo, lang);
            gh.post_issue_comment(repo, pr_number, &body)
                .await
                .map_err(|e| e.to_string())?;
            return Ok("not-enabled".into());
        }
        Err(e) => return Err(e.to_string()),
    };

    // 2. changed files
    let files = gh
        .list_pr_files(repo, pr_number)
        .await
        .map_err(|e| e.to_string())?;
    let changed: std::collections::HashSet<&str> = files
        .iter()
        .filter_map(|f| f.get("filename").and_then(|n| n.as_str()))
        .collect();

    // 3. intersect: alert location file ∈ changed files
    let mut relevant: Vec<&Value> = Vec::new();
    for alert in &alerts {
        let Some(loc) = alert
            .pointer("/most_recent_instance/location")
            .cloned()
            .or_else(|| alert.get("location").cloned())
        else {
            continue;
        };
        let Some(path) = loc.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        if changed.contains(path) {
            relevant.push(alert);
        }
    }

    // 4. render + post — the report is the deliverable, so a failed post is a
    // failed command. Discarding it reported `ok` for a report nobody received.
    let report = render_report(&alerts, &relevant, &changed, lang);
    gh.post_issue_comment(repo, pr_number, &report)
        .await
        .map_err(|e| e.to_string())?;
    Ok("ok".into())
}

/// Sort key: 0 is the most severe.
///
/// Both this and [`severity_badge`] go through `canon_severity` so the CodeQL
/// report and the AI review agree on what a word means — they had separate
/// tables, and an alert whose `security_severity_level` was `moderate` sorted
/// last here while the AI review would have called it medium.
fn severity_rank(sev: &str) -> u8 {
    crate::review::SEVERITIES
        .iter()
        .position(|s| *s == crate::review::canon_severity(sev))
        .unwrap_or(crate::review::SEVERITIES.len()) as u8
}

/// The dot and its label, in the reader's language. Previously the label was
/// SARIF's own vocabulary and English-only, so a Chinese report said
/// "🟡 warning" in a table headed 级别.
fn severity_badge(sev: &str, lang: Lang) -> (&'static str, &'static str) {
    crate::review::sev_meta(crate::review::canon_severity(sev), lang)
}

fn render_report(
    all_alerts: &[Value],
    relevant: &[&Value],
    changed: &std::collections::HashSet<&str>,
    lang: Lang,
) -> String {
    let open = all_alerts.len();
    let files = changed.len();
    let hit_files = relevant
        .iter()
        .filter_map(|a| {
            a.pointer("/most_recent_instance/location/path")
                .or_else(|| a.pointer("/location/path"))
                .and_then(|p| p.as_str())
        })
        .collect::<std::collections::HashSet<_>>()
        .len();
    let mut lines = vec![
        lang.pick("## 🔍 CodeQL quality report", "## 🔍 CodeQL 质量报告")
            .to_string(),
        String::new(),
        t!(
            lang,
            "- Open alerts in the repo: **{open}**",
            "- 仓库存量 open 告警: **{open}** 条"
        ),
        t!(
            lang,
            "- Files changed by this PR: **{files}**, of which **{hit_files}** touch an existing alert",
            "- 本次 PR 变更文件: **{files}** 个,其中 **{hit_files}** 个文件触及存量告警"
        ),
        String::new(),
    ];

    if relevant.is_empty() {
        lines.push(
            lang.pick(
                "✅ This change touches none of the existing CodeQL alerts.",
                "✅ 本次变更未触及任何存量 CodeQL 告警。",
            )
            .to_string(),
        );
        return lines.join("\n");
    }

    // sort by severity
    let mut sorted: Vec<&&Value> = relevant.iter().collect();
    sorted.sort_by_key(|a| {
        a.get("rule")
            .and_then(|r| r.get("security_severity_level"))
            .and_then(|s| s.as_str())
            .or_else(|| {
                a.get("rule")
                    .and_then(|r| r.get("severity"))
                    .and_then(|s| s.as_str())
            })
            .map(severity_rank)
            .unwrap_or(4)
    });

    let n = sorted.len();
    lines.push(t!(
        lang,
        "### Alerts touched by this change ({n})",
        "### 本次变更触及的告警({n} 条)"
    ));
    lines.push(String::new());
    lines.push(
        lang.pick(
            "| Level | Rule | Location | Description |",
            "| 级别 | 规则 | 位置 | 说明 |",
        )
        .into(),
    );
    lines.push("|---|---|---|---|".into());
    for alert in sorted {
        let sev = alert
            .get("rule")
            .and_then(|r| r.get("security_severity_level"))
            .and_then(|s| s.as_str())
            .or_else(|| {
                alert
                    .get("rule")
                    .and_then(|r| r.get("severity"))
                    .and_then(|s| s.as_str())
            })
            .unwrap_or("");
        let (icon, label) = severity_badge(sev, lang);
        let rule_id = alert
            .get("rule")
            .and_then(|r| r.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or("?");
        let description = alert
            .get("rule")
            .and_then(|r| r.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let (path, start_line) = alert
            .pointer("/most_recent_instance/location")
            .or_else(|| alert.get("location"))
            .map(|loc| {
                (
                    loc.get("path").and_then(|p| p.as_str()).unwrap_or("?"),
                    loc.get("start_line").and_then(|l| l.as_i64()).unwrap_or(0),
                )
            })
            .unwrap_or(("?", 0));
        let html_url = alert.get("html_url").and_then(|u| u.as_str()).unwrap_or("");
        // Escaped before it goes in a cell: a rule description is prose from
        // the query author, and a single `|` in it shifted every column after
        // it — the location cell landed under "Description".
        let path = md_cell(path);
        let loc_cell = if html_url.is_empty() {
            format!("`{path}:{start_line}`")
        } else {
            format!("[`{path}:{start_line}`]({html_url})")
        };
        // Truncate first, escape second, so the cut can't land between the `\`
        // and the `|` it escapes.
        let description: String = description.chars().take(120).collect();
        lines.push(format!(
            "| {icon} {label} | `{}` | {loc_cell} | {} |",
            md_cell(rule_id),
            md_cell(&description)
        ));
    }

    lines.push(String::new());
    lines.push(
        lang.pick(
            "> Tip: fix the alerts listed above in this PR (or confirm a false \
positive and add a `// lgtm` suppression). Existing alerts this change doesn't \
touch are out of scope for this report.",
            "> 提示: 建议在本次 PR 中一并修复所列告警(或确认误报并加 `// lgtm` 忽略注释)。\
其余未触及的存量告警不在本报告范围内。",
        )
        .to_string(),
    );
    lines.join("\n")
}

fn not_enabled_message(repo: &str, lang: Lang) -> String {
    t!(
        lang,
        "## 🔍 CodeQL quality report\n\n\
⚠️ Code scanning isn't enabled on `{repo}` (or the App can't read it), so there's no report to build.\n\n\
**Enable it either way**:\n\
1. **CodeQL default setup** (recommended): repo Settings → Code security → Code scanning → Setup default configuration\n\
2. **Workflow**: add `.github/workflows/codeql.yml` (using `github/codeql-action/init` + `analyze`)\n\n\
Once it's on, comment `@bot codeql` again.\n\
_Note: code scanning on a private repo needs a GitHub Advanced Security licence._\n\
_Also check the GitHub App has been granted read access to \"Code scanning alerts\"._",
        "## 🔍 CodeQL 质量报告\n\n\
⚠️ 仓库 `{repo}` 未启用 code scanning(或 App 无读取权限),无法生成报告。\n\n\
**启用方式(任选其一)**:\n\
1. **CodeQL default setup**(推荐): 仓库 Settings → Code security → Code scanning → Setup default configuration\n\
2. **Workflow**: 添加 `.github/workflows/codeql.yml`(使用 `github/codeql-action/init` + `analyze`)\n\n\
启用后再次评论 `@bot codeql` 即可生成报告。\n\
_注: 私有仓库的 code scanning 需要 GitHub Advanced Security 许可。_\n\
_同时请确认 GitHub App 已被授予 \"Code scanning alerts\" 只读权限。_"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Lang;

    fn alert(path: &str, line: i64, rule: &str, sev: &str) -> Value {
        serde_json::json!({
            "rule": {"id": rule, "severity": sev, "description": format!("{rule} description")},
            "most_recent_instance": {"location": {"path": path, "start_line": line}},
            "html_url": "https://github.com/x/y/security/code-scanning/1"
        })
    }

    #[test]
    fn test_render_report_clean() {
        let alerts = vec![alert("other/file.rs", 1, "rs/sql-injection", "error")];
        let mut changed = std::collections::HashSet::new();
        changed.insert("src/main.rs");
        let out = render_report(&alerts, &[], &changed, Lang::Zh);
        assert!(out.contains("未触及任何存量"));
        assert!(out.contains("**1** 条"));
    }

    #[test]
    fn test_render_report_with_hits() {
        let alerts = vec![
            alert("src/main.rs", 10, "rs/sql-injection", "error"),
            alert("src/lib.rs", 3, "js/xss", "warning"),
        ];
        let mut changed = std::collections::HashSet::new();
        changed.insert("src/main.rs");
        // only the first alert is relevant
        let relevant: Vec<&Value> = alerts
            .iter()
            .filter(|a| {
                a.pointer("/most_recent_instance/location/path")
                    .and_then(|p| p.as_str())
                    == Some("src/main.rs")
            })
            .collect();
        let out = render_report(&alerts, &relevant, &changed, Lang::Zh);
        assert!(out.contains("触及的告警(1 条)"));
        assert!(out.contains("rs/sql-injection"));
        assert!(!out.contains("js/xss"));
    }

    /// The badge vocabulary is now shared with the AI review, and localized:
    /// a Chinese report used to say "🟡 warning" under a 级别 header.
    #[test]
    fn severity_badge_is_shared_and_localized() {
        assert_eq!(severity_badge("error", Lang::En), ("🟠", "high"));
        assert_eq!(severity_badge("error", Lang::Zh), ("🟠", "高"));
        assert_eq!(severity_badge("warning", Lang::Zh), ("🟡", "中"));
        // `security_severity_level` is CVSS, `rule.severity` is SARIF — both
        // reach this function, and both must rank the same way.
        assert_eq!(severity_rank("error"), severity_rank("high"));
        assert_eq!(severity_rank("warning"), severity_rank("medium"));
        assert!(severity_rank("critical") < severity_rank("note"));
        assert!(severity_rank("note") < severity_rank("nonsense"));
    }

    /// A `|` in a rule description shifted every column after it, so the
    /// location cell rendered under "Description".
    #[test]
    fn table_cells_are_escaped() {
        let mut a = alert("src/main.rs", 10, "rs/pipe", "error");
        a["rule"]["description"] = serde_json::json!("takes a|b and\nsplits it");
        let alerts = vec![a];
        let relevant: Vec<&Value> = alerts.iter().collect();
        let mut changed = std::collections::HashSet::new();
        changed.insert("src/main.rs");
        let out = render_report(&alerts, &relevant, &changed, Lang::En);

        let row = out
            .lines()
            .find(|l| l.contains("rs/pipe"))
            .expect("row must exist");
        // Header, separator and the row all have to agree on column count.
        assert_eq!(
            row.matches('|').count() - row.matches("\\|").count(),
            5,
            "column count broken: {row}"
        );
        assert!(row.contains("takes a\\|b and splits it"), "{row}");
    }

    #[test]
    fn test_not_enabled_message() {
        let m = not_enabled_message("o/r", Lang::Zh);
        assert!(m.contains("未启用"));
        assert!(m.contains("default setup"));
        assert!(m.contains("Advanced Security"));
    }
}
