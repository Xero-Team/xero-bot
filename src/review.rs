//! AI code review — builtin engine (faithful port of review.py) plus the
//! shared publication pipeline used by every engine.
//!
//! Flow: fetch diff → parse added lines → build prompt → call AI → parse
//! verdict → post review (with inline comments, degrading gracefully).

use serde_json::{json, Value};

use crate::config::Config;
use crate::github::{Client, GhError};
use crate::lang::Lang;
use crate::t;

pub const SEVERITIES: [&str; 5] = ["critical", "high", "medium", "low", "info"];

/// Fold any severity word we might be handed into one of [`SEVERITIES`].
///
/// Three vocabularies reach the renderers: our own five buckets (from the model
/// prompt), SARIF's `none`/`note`/`warning`/`error` (CodeQL `rule.severity`),
/// and CVSS's `low`/`medium`/`high`/`critical` (CodeQL
/// `security_severity_level`). Every place that read a severity used to do its
/// own matching, and they disagreed — a finding marked `warning` was counted
/// under *info* in the summary table, listed in *no* section at all (the filter
/// compared the raw string against the five buckets), and still emitted as an
/// inline comment with the info dot. One report, three different answers about
/// the same finding.
///
/// Anything unrecognized becomes `info` rather than being dropped: an
/// unexpected word from a model is not a reason to lose the finding.
pub fn canon_severity(raw: &str) -> &'static str {
    match raw.trim().to_lowercase().as_str() {
        "critical" | "blocker" => "critical",
        "high" | "error" => "high",
        "medium" | "moderate" | "warning" => "medium",
        "low" | "note" | "minor" => "low",
        _ => "info",
    }
}

/// Escape a value going into a Markdown table cell.
///
/// A `|` ends the cell and a newline ends the row, so an unescaped one from a
/// rule description silently shifts every following column — the location and
/// the link end up under the wrong headers. GitHub renders `\|` as a literal
/// pipe; there is no way to put a real line break inside a cell, so newlines
/// fold to a space.
pub fn md_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '|' => out.push_str("\\|"),
            '\r' => {}
            '\n' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// The dot alone. Split out from [`sev_meta`] so callers that only draw the
/// badge — inline comments, whose body text comes from the model — needn't
/// carry a language just to throw the label away.
pub fn sev_icon(sev: &str) -> &'static str {
    match sev {
        "critical" => "🔴",
        "high" => "🟠",
        "medium" => "🟡",
        "low" => "🔵",
        _ => "⚪",
    }
}

pub fn sev_meta(sev: &str, lang: Lang) -> (&'static str, &'static str) {
    let label = match sev {
        "critical" => lang.pick("critical", "严重"),
        "high" => lang.pick("high", "高"),
        "medium" => lang.pick("medium", "中"),
        "low" => lang.pick("low", "低"),
        _ => lang.pick("info", "信息"),
    };
    (sev_icon(sev), label)
}

// ---------------------------------------------------------------------------
// Diff parsing — per-file added line numbers (RIGHT side)
// ---------------------------------------------------------------------------

/// For each file, collect line numbers on the new side that were added.
/// These are the only lines GitHub lets inline review comments attach to.
///
/// `+++` is ambiguous in a unified diff: it introduces the new-side filename in
/// a header, and it is also what an added line whose content starts with `++ `
/// looks like. Reading it positionally — a header only where a header can
/// appear — is the only way to tell them apart. Treating every `+++ ` as a
/// header meant such a line cleared `current_file`, so **every remaining added
/// line in that file was dropped** and none of its findings could be posted
/// inline. Diffs of Markdown and of diffs themselves hit this routinely.
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
    // A `+++` line is a header only here: after `diff --git`, or after a `---`
    // seen outside a hunk (a bare unified diff with no `diff --git` at all).
    let mut expect_file_header = false;
    // Inside a hunk every `---`/`+++` is content — a removed or added line
    // whose own text begins with `--`/`++`.
    let mut in_hunk = false;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            expect_file_header = true;
            in_hunk = false;
            current_file = None;
            continue;
        }
        if !in_hunk && line.starts_with("--- ") {
            expect_file_header = true;
            continue;
        }
        if expect_file_header && line.starts_with("+++ ") {
            expect_file_header = false;
            // `+++ /dev/null` (a deletion) and anything else not under `b/`
            // leaves no file to attach comments to.
            current_file = file_re.captures(line).map(|m| {
                let name = m.get(1).unwrap().as_str().to_string();
                added.entry(name.clone()).or_default();
                name
            });
            continue;
        }
        if line.starts_with("@@") {
            expect_file_header = false;
            in_hunk = true;
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

/// Cut `text` to at most `max_bytes`, reporting whether anything was dropped.
///
/// The budget is bytes, because that is what the guard has always measured
/// (`str::len`) and what a request body is limited by. The cut used to be in
/// *chars*, so the two halves disagreed: a diff over the byte limit was
/// truncated to `max_bytes` **characters**, up to 3× the intended budget on CJK
/// text — exactly the input most likely to be near the limit in the first
/// place. Slicing needs a char boundary, so the cut walks back to the nearest
/// one; that loses at most three bytes.
pub fn truncate(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

// ---------------------------------------------------------------------------
// AI call — three formats
// ---------------------------------------------------------------------------

/// Which wire protocol `API_FORMAT` names.
///
/// A parsed value rather than a lowercased string, because two engines have to
/// agree about it. The agent loop used to skip this decision entirely and
/// hardcode `/chat/completions`, so an operator whose relay only speaks
/// `/responses` got a working builtin review and an agent engine that failed
/// every request — with `API_FORMAT=responses` sitting right there in the
/// config, apparently honored. Now every branch on the protocol branches on
/// this enum, and a fourth protocol is a compile error at each of them until
/// it's handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Chat,
    Responses,
    Anthropic,
}

impl Proto {
    /// Parse `API_FORMAT` — case- and whitespace-insensitive, since it comes
    /// from a hand-edited `.env`.
    pub fn parse(raw: &str) -> Result<Proto, String> {
        match raw.trim().to_lowercase().as_str() {
            "chat" => Ok(Proto::Chat),
            "responses" => Ok(Proto::Responses),
            "anthropic" => Ok(Proto::Anthropic),
            other => Err(format!(
                "unknown API_FORMAT {other:?} (expected chat, responses or anthropic)"
            )),
        }
    }

    /// The name it goes by in `.env` and in messages to the operator.
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Chat => "chat",
            Proto::Responses => "responses",
            Proto::Anthropic => "anthropic",
        }
    }

    /// The path appended to `AI_BASE_URL`.
    pub fn path(self) -> &'static str {
        match self {
            Proto::Chat => "/chat/completions",
            Proto::Responses => "/responses",
            // Anthropic's base URL carries no version segment, so the version
            // lives in the path here.
            Proto::Anthropic => "/v1/messages",
        }
    }
}

/// Build a header value out of operator-supplied text.
///
/// `.parse().unwrap()` looks harmless until you ask where the key comes from: a
/// `.env` line someone pasted. A stray newline or non-ASCII byte in
/// `AI_API_KEY` panicked the task that built the request — and a panic inside
/// `tokio::spawn` only kills that task, so the review simply never happened and
/// nothing said why.
fn header_value(what: &str, raw: &str) -> Result<reqwest::header::HeaderValue, String> {
    raw.parse().map_err(|_| {
        format!(
            "{what} cannot go in an HTTP header — check it for line breaks or non-ASCII characters"
        )
    })
}

/// The URL and headers for one protocol, shared by both AI engines so they
/// can't disagree about where the AI lives or how to authenticate to it.
pub(crate) fn ai_endpoint(
    cfg: &Config,
    proto: Proto,
) -> Result<(String, reqwest::header::HeaderMap), String> {
    let url = format!("{}{}", cfg.ai_base_url.trim_end_matches('/'), proto.path());
    let mut headers = reqwest::header::HeaderMap::new();
    match proto {
        Proto::Chat | Proto::Responses => {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                header_value("AI_API_KEY", &format!("Bearer {}", cfg.ai_api_key))?,
            );
        }
        Proto::Anthropic => {
            headers.insert(
                reqwest::header::HeaderName::from_static("x-api-key"),
                header_value("AI_API_KEY", &cfg.ai_api_key)?,
            );
            headers.insert(
                reqwest::header::HeaderName::from_static("anthropic-version"),
                reqwest::header::HeaderValue::from_static("2023-06-01"),
            );
        }
    }
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    Ok((url, headers))
}

pub async fn call_ai(
    cfg: &Config,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String, String> {
    let proto = Proto::parse(&cfg.api_format)?;
    let (url, headers) = ai_endpoint(cfg, proto)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    let body = match proto {
        Proto::Chat => json!({
            "model": cfg.ai_model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            "temperature": 0.2,
            "response_format": {"type": "json_object"},
        }),
        Proto::Responses => json!({
            "model": cfg.ai_model,
            "input": user_prompt,
            "instructions": system_prompt,
            "text": {"format": {"type": "json_object"}},
        }),
        Proto::Anthropic => json!({
            "model": cfg.ai_model,
            "max_tokens": 4096,
            "system": system_prompt,
            "messages": [{"role": "user", "content": user_prompt}],
        }),
    };

    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            // The URL goes to the log, which is ours; the returned string can
            // end up in a PR comment, so it names the protocol instead.
            // `without_url` is why: reqwest's Display includes the full URL.
            tracing::error!("AI request to {url} failed: {e}");
            format!("AI request failed: {}", e.without_url())
        })?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("AI read failed: {}", e.without_url()))?;
    if !status.is_success() {
        let snippet = text.chars().take(400).collect::<String>();
        tracing::error!("AI request to {url} failed ({status}): {snippet}");
        return Err(format!(
            "AI request failed ({status}) at the {} endpoint: {snippet}",
            proto.as_str()
        ));
    }
    let out: Value =
        serde_json::from_str(&text).map_err(|e| format!("AI response not JSON: {e}"))?;

    extract_ai_text(proto, &out)
}

fn extract_ai_text(proto: Proto, out: &Value) -> Result<String, String> {
    match proto {
        Proto::Chat => out
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .map(String::from)
            .ok_or_else(|| format!("chat API: no content in {out}")),
        Proto::Responses => {
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
        Proto::Anthropic => {
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

/// The reviewer's brief, in the language the review will be written in.
///
/// Translated in full rather than bolting [`Lang::output_rule`] onto a Chinese
/// prompt: a model asked in English writes better English, and the prose fields
/// of the verdict are published verbatim.
pub fn system_prompt(lang: Lang) -> &'static str {
    lang.pick(
        "You are a senior, rigorous security and code-quality reviewer. Review the \
code changes in a pull request and report problems graded by risk. Report only \
real problems; do not invent findings to fill the list. \
Your output must be strict JSON (no explanatory prose, no markdown fence). \
JSON schema: \
{\"summary\": \"one-sentence overall assessment\", \
\"findings\": [{\"severity\": \"critical|high|medium|low|info\", \
\"title\": \"short title\", \"file\": \"path of a file in the diff\", \
\"line\": integer line number (one of the added lines; use 1 if not applicable), \
\"description\": \"the problem and its potential impact\", \
\"suggestion\": \"a concrete fix\"}]}. \
Severity guide: critical=security hole (injection/RCE/auth bypass/data loss); \
high=logic bug/resource leak/race/core functionality broken; \
medium=edge case/missing error handling; low=style/maintainability; \
info=suggestion/question/nit. \
Write `summary`, `description` and `suggestion` in English. \
If there is nothing to report, `findings` is an empty array.",
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
用中文输出 summary、description 和 suggestion。若无问题, findings 为空数组。",
    )
}

pub fn build_user_prompt(
    diff: &str,
    pr_meta: &Value,
    truncated: bool,
    previous_review: Option<&str>,
    new_commits: Option<&str>,
    lang: Lang,
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
        lang.pick(
            "\n\n[Note: the diff is truncated; only the first part of the change is shown]\n",
            "\n\n[注意: diff 已截断,仅展示前部分改动]\n",
        )
    } else {
        ""
    };
    let prev_section = previous_review
        .map(|p| {
            t!(
                lang,
                "\n## Previous review (check whether these were fixed; don't repeat findings \
that were resolved or rejected, and confirm the fixes in your summary):\n{p}\n",
                "\n## 上一轮审查意见(检查这些是否已修复;避免重复已被解决/驳回的发现,如已修复请在总结中确认):\n{p}\n"
            )
        })
        .unwrap_or_default();
    let commits_section = new_commits
        .map(|c| {
            t!(
                lang,
                "\n## Commits pushed since the previous review (focus on the increment):\n{c}\n",
                "\n## 自上一轮审查以来的新提交(重点审查增量):\n{c}\n"
            )
        })
        .unwrap_or_default();
    t!(
        lang,
        "PR title: {title}\nPR description: {body}\n{prev_section}{commits_section}\nBelow is the PR's unified diff (look only at the added code):\n{diff}{note}\n\nReview the change above and answer with the JSON schema given.",
        "PR 标题: {title}\nPR 描述: {body}\n{prev_section}{commits_section}\n以下是 PR 的 unified diff(只关注新增的代码):\n{diff}{note}\n\n请审查上述改动并按指定 JSON schema 输出。"
    )
}

// ---------------------------------------------------------------------------
// Rendering & posting
// ---------------------------------------------------------------------------

/// One finding's bucket. The single reader of the `severity` field, so the
/// three renderers below cannot drift apart again.
fn finding_severity(f: &Value) -> &'static str {
    canon_severity(f.get("severity").and_then(|s| s.as_str()).unwrap_or("info"))
}

pub fn render_summary(verdict: &Value, engine_tag: &str, lang: Lang) -> String {
    let findings = verdict
        .get("findings")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    let summary = verdict
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or(lang.pick("(no summary)", "(无总结)"));

    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for s in SEVERITIES {
        counts.insert(s, 0);
    }
    for f in &findings {
        // Via `canon_severity`, so this count, the sections below and the
        // inline comments all put the finding in the same bucket.
        *counts.get_mut(finding_severity(f)).unwrap() += 1;
    }

    let mut table = String::from(lang.pick(
        "| Level | Count |\n|---|---|\n",
        "| 等级 | 数量 |\n|---|---|\n",
    ));
    for s in SEVERITIES {
        let (icon, label) = sev_meta(s, lang);
        table.push_str(&format!("| {icon} {label} | {} |\n", counts[s]));
    }

    let mut lines = vec![
        "## 🤖 AI Code Review".to_string(),
        String::new(),
        format!("**{summary}**"),
        String::new(),
        lang.pick("### Risk breakdown", "### 风险分级").to_string(),
        String::new(),
        table,
    ];
    if !engine_tag.is_empty() {
        lines.push(format!("_engine: {engine_tag}_\n"));
    }

    if findings.is_empty() {
        lines.push(lang.pick("Nothing found 🎉", "未发现问题 🎉").to_string());
        return lines.join("\n");
    }

    for s in SEVERITIES {
        let items: Vec<&Value> = findings
            .iter()
            .filter(|f| finding_severity(f) == s)
            .collect();
        if items.is_empty() {
            continue;
        }
        let (icon, label) = sev_meta(s, lang);
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
                .unwrap_or(lang.pick("(no title)", "(无标题)"));
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
        let icon = sev_icon(finding_severity(f));
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
pub async fn run_builtin(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    lang: Lang,
) -> String {
    match run_builtin_inner(gh, cfg, repo, pr_number, lang).await {
        Ok(status) => status,
        Err(e) => {
            let body = t!(
                lang,
                "## 🤖 AI Code Review\n\n❌ Review failed: `{e}`",
                "## 🤖 AI Code Review\n\n❌ 审查出错: `{e}`"
            );
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
    lang: Lang,
) -> Result<String, String> {
    // processing indicator (best-effort)
    let _ = gh
        .post_issue_comment(
            repo,
            pr_number,
            lang.pick("🔄 Reviewing, one moment…", "🔄 正在审查,稍候…"),
        )
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
        lang,
    );

    let raw = call_ai(cfg, system_prompt(lang), &user_prompt).await?;
    let Some(verdict) = parse_verdict(&raw) else {
        let body = t!(
            lang,
            "## 🤖 AI Code Review\n\n⚠️ Couldn't parse the model's JSON; raw output below:\n\n```\n{raw}\n```",
            "## 🤖 AI Code Review\n\n⚠️ 未能解析模型返回的 JSON,以下为原始输出:\n\n```\n{raw}\n```"
        );
        let _ = gh.post_issue_comment(repo, pr_number, &body).await;
        return Ok("parse-failed".into());
    };

    let summary = render_summary(&verdict, "builtin", lang);
    let inline = build_inline_comments(&verdict, &added);
    let mode = gh
        .post_review(repo, pr_number, &summary, inline)
        .await
        .map_err(|e: GhError| e.to_string())?;
    Ok(mode.to_string())
}

/// Fetch (previous own review body, new commits since that review).
pub async fn fetch_incremental_context(
    gh: &Client,
    repo: &str,
    pr_number: i64,
    max_bytes: usize,
) -> (Option<String>, Option<String>) {
    let prev = gh
        .own_previous_reviews(repo, pr_number)
        .await
        .unwrap_or_default();
    let last_body = prev
        .iter()
        .rev()
        .find_map(|r| r.get("body").and_then(|b| b.as_str()).map(String::from))
        // Through `truncate` for the same reason: taking `max/2` *chars* of a
        // Chinese review body spends up to 1.5× the whole prompt budget on the
        // half of it meant for context.
        .map(|b| truncate(&b, max_bytes / 2).0);

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
    use crate::lang::Lang;

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

    /// A content line that happens to start with `++ ` is not a file header.
    /// It used to clear `current_file`, so every added line after it in the same
    /// file was lost and no finding there could be posted inline.
    #[test]
    fn added_lines_survive_a_plus_plus_content_line() {
        let diff = "\
diff --git a/CHANGELOG.md b/CHANGELOG.md
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -1,1 +1,4 @@
 # Changelog
+++ nested diff marker
+-- and the old-side one too
+real content
";
        let added = parse_added_lines(diff);
        let set = added.get("CHANGELOG.md").expect("file must be tracked");
        assert_eq!(
            set,
            &[2i64, 3, 4].into_iter().collect(),
            "content lines after a `++ ` line were dropped: {set:?}"
        );
    }

    /// `+++ /dev/null` names no file on the new side, so there is nothing to
    /// attach to — and it must not leave the *previous* file selected.
    #[test]
    fn deleted_file_selects_nothing() {
        let diff = "\
diff --git a/keep.rs b/keep.rs
--- a/keep.rs
+++ b/keep.rs
@@ -1,1 +1,2 @@
 a
+b
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-x
";
        let added = parse_added_lines(diff);
        assert_eq!(added.get("keep.rs"), Some(&[2i64].into_iter().collect()));
        assert!(!added.contains_key("gone.rs"));
        assert_eq!(added.len(), 1, "{added:?}");
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

    /// The guard measured bytes and the cut counted chars, so a CJK diff over
    /// the limit was truncated to `max` *characters* — 3× the budget.
    #[test]
    fn truncate_budget_is_bytes_and_cuts_on_a_boundary() {
        // 10 chars, 30 bytes.
        let cjk = "一二三四五六七八九十";
        assert_eq!(cjk.len(), 30);

        // Under budget in bytes: untouched.
        let (d, t) = truncate(cjk, 30);
        assert_eq!(d, cjk);
        assert!(!t);

        // Over budget: the result must respect the byte budget, not blow past
        // it — the old code returned all 30 bytes here.
        let (d, t) = truncate(cjk, 10);
        assert!(t);
        assert!(d.len() <= 10, "{} bytes for a 10-byte budget", d.len());
        // 10 is mid-character (bytes 9..12 are 四), so it backs off to 9.
        assert_eq!(d, "一二三");

        // A budget smaller than the first character yields nothing rather than
        // panicking on a mid-character slice.
        let (d, t) = truncate(cjk, 2);
        assert_eq!(d, "");
        assert!(t);
    }

    #[test]
    fn canon_severity_folds_every_vocabulary() {
        // ours
        for s in SEVERITIES {
            assert_eq!(canon_severity(s), s);
        }
        // SARIF (CodeQL rule.severity)
        assert_eq!(canon_severity("error"), "high");
        assert_eq!(canon_severity("warning"), "medium");
        assert_eq!(canon_severity("note"), "low");
        assert_eq!(canon_severity("none"), "info");
        // case and stray whitespace
        assert_eq!(canon_severity(" WARNING "), "medium");
        // unknown words land in info rather than being dropped
        assert_eq!(canon_severity("spicy"), "info");
        assert_eq!(canon_severity(""), "info");
    }

    /// The regression that motivated `canon_severity`: one finding, three
    /// renderers, and they disagreed about which bucket it was in.
    #[test]
    fn a_sarif_severity_is_counted_listed_and_marked_alike() {
        let verdict = serde_json::json!({
            "summary": "s",
            "findings": [
                {"severity": "warning", "title": "t", "file": "src/main.rs", "line": 3,
                 "description": "d", "suggestion": ""}
            ]
        });
        let out = render_summary(&verdict, "", Lang::En);
        // Counted as medium in the table...
        assert!(out.contains("| 🟡 medium | 1 |"), "{out}");
        assert!(out.contains("| ⚪ info | 0 |"), "{out}");
        // ...and listed under the matching section, which it appeared in at
        // all before only if the raw word was one of our five.
        assert!(out.contains("### 🟡 medium (1)"), "{out}");
        // ...and the inline comment carries the same dot.
        let added = parse_added_lines(SAMPLE_DIFF);
        let inline = build_inline_comments(&verdict, &added);
        assert_eq!(inline.len(), 1);
        assert!(
            inline[0]["body"].as_str().unwrap().starts_with("🟡"),
            "{:?}",
            inline[0]["body"]
        );
    }

    #[test]
    fn md_cell_escapes_pipes_and_newlines() {
        assert_eq!(md_cell("a|b"), "a\\|b");
        assert_eq!(md_cell("a\r\nb"), "a b");
        assert_eq!(md_cell("plain"), "plain");
        // A row must stay one row no matter what the cell contains.
        let cell = md_cell("x | y\nz");
        assert!(!cell.contains('\n'));
        assert_eq!(cell.matches('|').count(), 1);
        assert!(cell.contains("\\|"));
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
        let out = render_summary(&verdict, "builtin", Lang::Zh);
        assert!(out.contains("clean"));
        assert!(out.contains("未发现问题"));
        let en = render_summary(&verdict, "builtin", Lang::En);
        assert!(en.contains("Nothing found"), "{en}");
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
        let out = render_summary(&verdict, "", Lang::Zh);
        assert!(out.contains("some issues"));
        assert!(out.contains("🟠 高 (1)"));
        assert!(out.contains("`a.rs:1`"));
        assert!(out.contains("💡 fix it"));
    }

    /// An English review must not carry Chinese severity labels or headers.
    /// The model's own prose is the only text in the body that isn't ours.
    #[test]
    fn test_render_summary_english_has_no_chinese() {
        let verdict = serde_json::json!({
            "summary": "some issues",
            "findings": [
                {"severity": "high", "title": "bug", "file": "a.rs", "line": 1,
                 "description": "desc", "suggestion": "fix it"},
                {"severity": "info", "file": "b.rs", "line": 2}
            ]
        });
        let out = render_summary(&verdict, "builtin", Lang::En);
        assert!(out.contains("🟠 high (1)"), "{out}");
        assert!(out.contains("Risk breakdown"), "{out}");
        assert!(out.contains("(no title)"), "{out}");
        assert!(
            !out.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "Chinese left in an English review: {out}"
        );
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
            Lang::Zh,
        );
        assert!(p.contains("上一轮审查意见"));
        assert!(p.contains("上一轮: 修了 X"));
        assert!(p.contains("新提交"));
        assert!(p.contains("fix: a"));

        let en = build_user_prompt(
            "d",
            &meta,
            true,
            Some("previously: fixed X"),
            Some("fix: a"),
            Lang::En,
        );
        assert!(en.contains("Previous review"), "{en}");
        assert!(en.contains("Commits pushed since"), "{en}");
        assert!(en.contains("truncated"), "{en}");
        assert!(
            !en.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "{en}"
        );
    }

    /// Both briefs must describe the same schema, or one language silently
    /// gets a differently-shaped verdict that `render_summary` can't read.
    #[test]
    fn test_system_prompt_agrees_across_languages() {
        for lang in [Lang::En, Lang::Zh] {
            let p = system_prompt(lang);
            for needle in [
                "summary",
                "findings",
                "severity",
                "title",
                "file",
                "line",
                "description",
                "suggestion",
                "critical|high|medium|low|info",
            ] {
                assert!(p.contains(needle), "{lang:?} prompt missing {needle}");
            }
        }
        assert!(
            !system_prompt(Lang::En)
                .chars()
                .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "{}",
            system_prompt(Lang::En)
        );
    }

    #[test]
    fn api_format_names_a_protocol_or_says_so() {
        assert_eq!(Proto::parse("chat"), Ok(Proto::Chat));
        assert_eq!(Proto::parse("responses"), Ok(Proto::Responses));
        assert_eq!(Proto::parse("anthropic"), Ok(Proto::Anthropic));
        // it comes from a hand-edited .env
        assert_eq!(Proto::parse("  Chat \n"), Ok(Proto::Chat));
        let err = Proto::parse("gpt").unwrap_err();
        assert!(err.contains("gpt") && err.contains("responses"), "{err}");
    }

    fn cfg_for(base: &str, key: &str) -> Config {
        let mut c = Config::from_env();
        c.ai_base_url = base.into();
        c.ai_api_key = key.into();
        c
    }

    #[test]
    fn each_protocol_has_its_own_endpoint_and_auth_header() {
        let cfg = cfg_for("https://relay.example/v1/", "k-secret-value");

        let (url, headers) = ai_endpoint(&cfg, Proto::Chat).unwrap();
        assert_eq!(url, "https://relay.example/v1/chat/completions");
        assert_eq!(headers["authorization"], "Bearer k-secret-value");

        let (url, headers) = ai_endpoint(&cfg, Proto::Responses).unwrap();
        assert_eq!(url, "https://relay.example/v1/responses");
        assert_eq!(headers["authorization"], "Bearer k-secret-value");

        // Anthropic authenticates with its own header, not a bearer token.
        let (url, headers) = ai_endpoint(&cfg, Proto::Anthropic).unwrap();
        assert_eq!(url, "https://relay.example/v1/v1/messages");
        assert_eq!(headers["x-api-key"], "k-secret-value");
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert!(!headers.contains_key("authorization"));
    }

    /// A pasted key with a newline in it used to panic the task building the
    /// request, and inside `tokio::spawn` that means the review vanishes
    /// without a word in the log.
    #[test]
    fn an_unusable_key_is_an_error_not_a_panic() {
        let cfg = cfg_for("https://relay.example/v1", "sk-abc\ndef");
        for proto in [Proto::Chat, Proto::Responses, Proto::Anthropic] {
            let err = ai_endpoint(&cfg, proto).unwrap_err();
            assert!(err.contains("AI_API_KEY"), "{proto:?}: {err}");
        }
    }
}
