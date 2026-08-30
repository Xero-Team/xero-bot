//! AI code review — builtin engine (faithful port of review.py) plus the
//! shared publication pipeline used by every engine.
//!
//! Flow: fetch diff → parse added lines → build prompt → call AI → parse
//! verdict → post review (with inline comments, degrading gracefully).

use serde_json::{json, Value};

use crate::config::Config;
use crate::github::{Client, GhError};

pub const SEVERITIES: [&str; 5] = ["critical", "high", "medium", "low", "info"];

pub fn sev_meta(sev: &str) -> (&'static str, &'static str) {
    match sev {
        "critical" => ("🔴", "严重"),
        "high" => ("🟠", "高"),
        "medium" => ("🟡", "中"),
        "low" => ("🔵", "低"),
        _ => ("⚪", "信息"),
    }
}

// ---------------------------------------------------------------------------
// Diff parsing — per-file added line numbers (RIGHT side)
// ---------------------------------------------------------------------------

/// For each file, collect line numbers on the new side that were added.
/// These are the only lines GitHub lets inline review comments attach to.
pub fn parse_added_lines(
    diff: &str,
) -> std::collections::HashMap<String, std::collections::HashSet<i64>> {
    use regex::Regex;
    use std::collections::{HashMap, HashSet};

    let mut added: HashMap<String, HashSet<i64>> = HashMap::new();
    let file_re = Regex::new(r"^\+\+\+ b/(.+)$").unwrap();
    let hunk_re = Regex::new(r"\+(\d+)(?:,(\d+))?").unwrap();

    let mut current_file: Option<String> = None;
    let mut new_line: i64 = 0;

    for line in diff.lines() {
        if let Some(m) = file_re.captures(line) {
            let name = m.get(1).unwrap().as_str().to_string();
            added.entry(name.clone()).or_default();
            current_file = Some(name);
            continue;
        }
        if line.starts_with("+++ ") {
            current_file = None;
            continue;
        }
        if line.starts_with("@@") {
            if let Some(mm) = hunk_re.captures(line) {
                new_line = mm.get(1).and_then(|d| d.as_str().parse().ok()).unwrap_or(0) - 1;
            }
            continue;
        }
        let Some(file) = &current_file else {
            continue;
        };
        if line.starts_with('+') {
            new_line += 1;
            added.get_mut(file).unwrap().insert(new_line);
        } else if line.starts_with('-') {
            // removed line: new-side numbering unchanged
        } else {
            new_line += 1;
        }
    }
    added
}

pub fn truncate(diff: &str, max_chars: usize) -> (String, bool) {
    if diff.len() <= max_chars {
        return (diff.to_string(), false);
    }
    (diff.chars().take(max_chars).collect(), true)
}

// ---------------------------------------------------------------------------
// AI call — three formats
// ---------------------------------------------------------------------------

pub async fn call_ai(
    cfg: &Config,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String, String> {
    let base = cfg.ai_base_url.trim_end_matches('/');
    let fmt = cfg.api_format.to_lowercase();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    let url;
    let mut headers = reqwest::header::HeaderMap::new();
    let body: Value;

    if fmt == "chat" {
        url = format!("{base}/chat/completions");
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", cfg.ai_api_key).parse().unwrap(),
        );
        body = json!({
            "model": cfg.ai_model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            "temperature": 0.2,
            "response_format": {"type": "json_object"},
        });
    } else if fmt == "responses" {
        url = format!("{base}/responses");
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", cfg.ai_api_key).parse().unwrap(),
        );
        body = json!({
            "model": cfg.ai_model,
            "input": user_prompt,
            "instructions": system_prompt,
            "text": {"format": {"type": "json_object"}},
        });
    } else if fmt == "anthropic" {
        url = format!("{base}/v1/messages");
        headers.insert(
            reqwest::header::HeaderName::from_static("x-api-key"),
            cfg.ai_api_key.parse().unwrap(),
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("anthropic-version"),
            "2023-06-01".parse().unwrap(),
        );
        body = json!({
            "model": cfg.ai_model,
            "max_tokens": 4096,
            "system": system_prompt,
            "messages": [{"role": "user", "content": user_prompt}],
        });
    } else {
        return Err(format!("unknown api_format: {}", cfg.api_format));
    }

    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );

    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("AI request failed to {url}: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("AI read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "AI request failed ({}) to {url}: {}",
            status,
            &text.chars().take(400).collect::<String>()
        ));
    }
    let out: Value =
        serde_json::from_str(&text).map_err(|e| format!("AI response not JSON: {e}"))?;

    extract_ai_text(&fmt, &out)
}

fn extract_ai_text(fmt: &str, out: &Value) -> Result<String, String> {
    match fmt {
        "chat" => out
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .map(String::from)
            .ok_or_else(|| format!("chat API: no content in {out}")),
        "responses" => {
            if let Some(t) = out.get("output_text").and_then(|t| t.as_str()) {
                return Ok(t.to_string());
            }
            // fallback: walk output array backwards
            if let Some(arr) = out.get("output").and_then(|o| o.as_array()) {
                for item in arr.iter().rev() {
                    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                        for c in content {
                            if c.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                                if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                                    if !t.is_empty() {
                                        return Ok(t.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(format!("responses API: no text in {out}"))
        }
        "anthropic" => {
            if let Some(content) = out.get("content").and_then(|c| c.as_array()) {
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                return Ok(t.to_string());
                            }
                        }
                    }
                }
            }
            Err(format!("anthropic API: no text in {out}"))
        }
        _ => Err("unknown format".into()),
    }
}

// ---------------------------------------------------------------------------
// Verdict parsing (robust, three-tier fallback)
// ---------------------------------------------------------------------------

pub fn parse_verdict(text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    // ```json fenced
    let re = regex::Regex::new(r"```(?:json)?\s*(\{.*?\})\s*```").unwrap();
    if let Some(m) = re.captures(text) {
        if let Ok(v) = serde_json::from_str::<Value>(m.get(1).unwrap().as_str()) {
            return Some(v);
        }
    }
    // greediest {...}
    if let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) {
        if a < b {
            if let Ok(v) = serde_json::from_str::<Value>(&text[a..=b]) {
                return Some(v);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

pub const SYSTEM_PROMPT: &str =
    "你是一名资深、严谨的安全与代码质量审查员。审查 pull request 的代码改动,\
按风险分级输出问题。只报告真实问题,不要为了凑数编造。\
输出必须是严格的 JSON(不要加任何解释性文字、不要 markdown 围栏)。\
JSON schema: \
{\"summary\": \"一句话总体评价\", \
\"findings\": [{\"severity\": \"critical|high|medium|low|info\", \
\"title\": \"简短标题\", \"file\": \"改动中的文件路径\", \
\"line\": 整数行号(改动新增行之一,若不适用填1), \
\"description\": \"问题描述与潜在影响\", \
\"suggestion\": \"具体修复建议\"}]}。\
severity 标准: critical=安全漏洞(注入/RCE/鉴权绕过/数据丢失); \
high=逻辑bug/资源泄漏/竞态/核心功能损坏; \
medium=边界条件/错误处理缺失; low=风格/可维护性; info=建议/疑问/nit。\
用中文输出 description 和 suggestion。若无问题, findings 为空数组。";

pub fn build_user_prompt(
    diff: &str,
    pr_meta: &Value,
    truncated: bool,
    previous_review: Option<&str>,
    new_commits: Option<&str>,
) -> String {
    let title = pr_meta.get("title").and_then(|t| t.as_str()).unwrap_or("");
    let body: String = pr_meta
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .chars()
        .take(2000)
        .collect();
    let note = if truncated {
        "\n\n[注意: diff 已截断,仅展示前部分改动]\n"
    } else {
        ""
    };
    let prev_section = previous_review
        .map(|p| format!("\n## 上一轮审查意见(检查这些是否已修复;避免重复已被解决/驳回的发现,如已修复请在总结中确认):\n{p}\n"))
        .unwrap_or_default();
    let commits_section = new_commits
        .map(|c| format!("\n## 自上一轮审查以来的新提交(重点审查增量):\n{c}\n"))
        .unwrap_or_default();
    format!(
        "PR 标题: {title}\nPR 描述: {body}\n{prev_section}{commits_section}\n以下是 PR 的 unified diff(只关注新增的代码):\n{diff}{note}\n\n请审查上述改动并按指定 JSON schema 输出。"
    )
}

// ---------------------------------------------------------------------------
// Rendering & posting
// ---------------------------------------------------------------------------

pub fn render_summary(verdict: &Value, engine_tag: &str) -> String {
    let findings = verdict
        .get("findings")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    let summary = verdict
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("(无总结)");

    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for s in SEVERITIES {
        counts.insert(s, 0);
    }
    for f in &findings {
        let sev = f
            .get("severity")
            .and_then(|s| s.as_str())
            .unwrap_or("info")
            .to_lowercase();
        match counts.get_mut(sev.as_str()) {
            Some(c) => *c += 1,
            None => *counts.get_mut("info").unwrap() += 1,
        }
    }

    let mut table = String::from("| 等级 | 数量 |\n|---|---|\n");
    for s in SEVERITIES {
        let (icon, label) = sev_meta(s);
        table.push_str(&format!("| {icon} {label} | {} |\n", counts[s]));
    }

    let mut lines = vec![
        "## 🤖 AI Code Review".to_string(),
        String::new(),
        format!("**{summary}**"),
        String::new(),
        "### 风险分级".to_string(),
        String::new(),
        table,
    ];
    if !engine_tag.is_empty() {
        lines.push(format!("_engine: {engine_tag}_\n"));
    }

    if findings.is_empty() {
        lines.push("未发现问题 🎉".to_string());
        return lines.join("\n");
    }

    for s in SEVERITIES {
        let items: Vec<&Value> = findings
            .iter()
            .filter(|f| {
                f.get("severity")
                    .and_then(|x| x.as_str())
                    .unwrap_or("info")
                    .to_lowercase()
                    == s
            })
            .collect();
        if items.is_empty() {
            continue;
        }
        let (icon, label) = sev_meta(s);
        lines.push(String::new());
        lines.push(format!("### {icon} {label} ({})", items.len()));
        lines.push(String::new());
        for f in items {
            let file = f.get("file").and_then(|x| x.as_str()).unwrap_or("?");
            let line = f
                .get("line")
                .and_then(|x| x.as_i64())
                .map(|l| l.to_string())
                .unwrap_or_else(|| "?".into());
            let title = f
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("(无标题)");
            let desc = f.get("description").and_then(|x| x.as_str()).unwrap_or("");
            let sug = f.get("suggestion").and_then(|x| x.as_str()).unwrap_or("");
            lines.push(format!("- **`{file}:{line}` — {title}**"));
            lines.push(format!("  {desc}"));
            if !sug.is_empty() {
                lines.push(format!("  💡 {sug}"));
            }
        }
    }
    lines.join("\n")
}

pub fn build_inline_comments(
    verdict: &Value,
    added_lines: &std::collections::HashMap<String, std::collections::HashSet<i64>>,
) -> Vec<Value> {
    let mut inline = Vec::new();
    let Some(findings) = verdict.get("findings").and_then(|f| f.as_array()) else {
        return inline;
    };
    for f in findings {
        let Some(file) = f.get("file").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(line) = f.get("line").and_then(|x| x.as_i64()) else {
            continue;
        };
        if !added_lines
            .get(file)
            .map(|set| set.contains(&line))
            .unwrap_or(false)
        {
            continue;
        }
        let sev = f
            .get("severity")
            .and_then(|x| x.as_str())
            .unwrap_or("info")
            .to_lowercase();
        let (icon, _) = sev_meta(&sev);
        let title = f.get("title").and_then(|x| x.as_str()).unwrap_or("");
        let desc = f.get("description").and_then(|x| x.as_str()).unwrap_or("");
        let sug = f.get("suggestion").and_then(|x| x.as_str()).unwrap_or("");
        inline.push(json!({
            "path": file,
            "line": line,
            "side": "RIGHT",
            "body": format!("{icon} **{title}**\n\n{desc}\n\n💡 {sug}").trim(),
        }));
    }
    inline
}

// ---------------------------------------------------------------------------
// Orchestration — run_builtin
// ---------------------------------------------------------------------------

/// The builtin review run: returns a status string, never panics.
/// Errors are reported to the PR as comments (Python bot behavior).
pub async fn run_builtin(gh: &Client, cfg: &Config, repo: &str, pr_number: i64) -> String {
    match run_builtin_inner(gh, cfg, repo, pr_number).await {
        Ok(status) => status,
        Err(e) => {
            let body = format!("## 🤖 AI Code Review\n\n❌ 审查出错: `{e}`");
            let _ = gh.post_issue_comment(repo, pr_number, &body).await;
            format!("error: {e}")
        }
    }
}

async fn run_builtin_inner(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
) -> Result<String, String> {
    // processing indicator (best-effort)
    let _ = gh
        .post_issue_comment(repo, pr_number, "🔄 正在审查,稍候…")
        .await;

    // fetch diff + meta
    let diff = gh
        .get_pr_diff(repo, pr_number)
        .await
        .map_err(|e| e.to_string())?;
    let meta = gh
        .get_pr(repo, pr_number)
        .await
        .map_err(|e| e.to_string())?;

    let (diff, truncated) = truncate(&diff, cfg.max_diff_chars);
    let added = parse_added_lines(&diff);

    // incremental context
    let (previous_review, new_commits) =
        fetch_incremental_context(gh, repo, pr_number, cfg.max_diff_chars).await;

    let user_prompt = build_user_prompt(
        &diff,
        &meta,
        truncated,
        previous_review.as_deref(),
        new_commits.as_deref(),
    );

    let raw = call_ai(cfg, SYSTEM_PROMPT, &user_prompt).await?;
    let Some(verdict) = parse_verdict(&raw) else {
        let body = format!(
            "## 🤖 AI Code Review\n\n⚠️ 未能解析模型返回的 JSON,以下为原始输出:\n\n```\n{raw}\n```"
        );
        let _ = gh.post_issue_comment(repo, pr_number, &body).await;
        return Ok("parse-failed".into());
    };

    let summary = render_summary(&verdict, "builtin");
    let inline = build_inline_comments(&verdict, &added);
    gh.post_review(repo, pr_number, &summary, inline)
        .await
        .map_err(|e: GhError| e.to_string())?;
    Ok("ok".into())
}

/// Fetch (previous own review body, new commits since that review).
pub async fn fetch_incremental_context(
    gh: &Client,
    repo: &str,
    pr_number: i64,
    max_chars: usize,
) -> (Option<String>, Option<String>) {
    let prev = gh
        .own_previous_reviews(repo, pr_number)
        .await
        .unwrap_or_default();
    let last_body = prev
        .iter()
        .rev()
        .find_map(|r| r.get("body").and_then(|b| b.as_str()).map(String::from))
        .map(|b| b.chars().take(max_chars / 2).collect::<String>());

    let new_commits = if prev.is_empty() {
        None
    } else {
        // commits submitted after the last own review
        let commits = gh
            .list_pr_commits(repo, pr_number)
            .await
            .unwrap_or_default();
        let last_time = prev
            .iter()
            .filter_map(|r| r.get("submitted_at").and_then(|t| t.as_str()))
            .max()
            .map(String::from);
        match last_time {
            Some(cutoff) => {
                let recent: Vec<String> = commits
                    .iter()
                    .filter(|c| {
                        c.get("commit")
                            .and_then(|cm| cm.get("committer"))
                            .and_then(|cm| cm.get("date"))
                            .and_then(|d| d.as_str())
                            .map(|d| d > cutoff.as_str())
                            .unwrap_or(false)
                    })
                    .filter_map(|c| {
                        c.get("commit")
                            .and_then(|cm| cm.get("message"))
                            .and_then(|m| m.as_str())
                            .map(|m| m.lines().next().unwrap_or("").to_string())
                    })
                    .collect();
                if recent.is_empty() {
                    None
                } else {
                    Some(recent.join("\n"))
                }
            }
            None => None,
        }
    };
    (last_body, new_commits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,6 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    let x = 1;
+    let y = 2;
 }
@@ -10,3 +12,4 @@
 fn other() {
+    helper();
 }
";

    #[test]
    fn test_parse_added_lines() {
        let added = parse_added_lines(SAMPLE_DIFF);
        let set = added.get("src/main.rs").unwrap();
        // lines 2,3,4 added in first hunk (start=1, +3 lines → 2,3,4);
        // hunk 2 starts at 12: line 13 added
        assert!(set.contains(&2) && set.contains(&3) && set.contains(&4));
        assert!(set.contains(&13));
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn test_truncate() {
        let (d, t) = truncate("hello", 10);
        assert_eq!(d, "hello");
        assert!(!t);
        let (d, t) = truncate("hello world", 5);
        assert_eq!(d, "hello");
        assert!(t);
    }

    #[test]
    fn test_parse_verdict_direct() {
        let v = parse_verdict(r#"{"summary": "ok", "findings": []}"#);
        assert!(v.is_some());
    }

    #[test]
    fn test_parse_verdict_fenced() {
        let v = parse_verdict("here you go:\n```json\n{\"summary\": \"x\", \"findings\": []}\n```");
        assert!(v.is_some());
        assert_eq!(v.unwrap()["summary"], "x");
    }

    #[test]
    fn test_parse_verdict_greedy() {
        let v = parse_verdict("junk before {\"summary\": \"y\", \"findings\": []} junk after");
        assert!(v.is_some());
    }

    #[test]
    fn test_parse_verdict_none() {
        assert!(parse_verdict("").is_none());
        assert!(parse_verdict("no json here").is_none());
    }

    #[test]
    fn test_render_summary_empty() {
        let verdict = serde_json::json!({"summary": "clean", "findings": []});
        let out = render_summary(&verdict, "builtin");
        assert!(out.contains("clean"));
        assert!(out.contains("未发现问题"));
    }

    #[test]
    fn test_render_summary_findings() {
        let verdict = serde_json::json!({
            "summary": "some issues",
            "findings": [
                {"severity": "high", "title": "bug", "file": "a.rs", "line": 1,
                 "description": "desc", "suggestion": "fix it"},
                {"severity": "info", "title": "nit", "file": "b.rs", "line": 2,
                 "description": "d2", "suggestion": ""}
            ]
        });
        let out = render_summary(&verdict, "");
        assert!(out.contains("some issues"));
        assert!(out.contains("🟠 高 (1)"));
        assert!(out.contains("`a.rs:1`"));
        assert!(out.contains("💡 fix it"));
    }

    #[test]
    fn test_build_inline_comments_filters_lines() {
        let verdict = serde_json::json!({
            "findings": [
                {"severity": "high", "title": "on added line", "file": "src/main.rs", "line": 3,
                 "description": "d", "suggestion": "s"},
                {"severity": "high", "title": "not added line", "file": "src/main.rs", "line": 99,
                 "description": "d", "suggestion": "s"},
                {"severity": "high", "title": "other file", "file": "nope.rs", "line": 1,
                 "description": "d", "suggestion": "s"}
            ]
        });
        let added = parse_added_lines(SAMPLE_DIFF);
        let inline = build_inline_comments(&verdict, &added);
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0]["path"], "src/main.rs");
        assert_eq!(inline[0]["line"], 3);
    }

    #[test]
    fn test_build_user_prompt_incremental_sections() {
        let meta = serde_json::json!({"title": "T", "body": "B"});
        let p = build_user_prompt(
            "d",
            &meta,
            false,
            Some("上一轮: 修了 X"),
            Some("fix: a\nfix: b"),
        );
        assert!(p.contains("上一轮审查意见"));
        assert!(p.contains("上一轮: 修了 X"));
        assert!(p.contains("新提交"));
        assert!(p.contains("fix: a"));
    }
}
