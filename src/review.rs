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

/// The request body for one protocol.
///
/// Shared by [`call_ai`] and [`preflight`] so the two can't drift: a startup
/// check that asked for a different response format than the real call does
/// would report a health it hasn't actually tested.
fn ai_body(cfg: &Config, proto: Proto, system_prompt: &str, user_prompt: &str) -> Value {
    match proto {
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
    }
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
    let body = ai_body(cfg, proto, system_prompt, user_prompt);

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
// Startup self-check
// ---------------------------------------------------------------------------

/// The result of one throwaway request to the configured AI endpoint.
pub struct Probe {
    /// What was tested, in operator terms.
    pub what: String,
    /// The full URL. This is log-only output, so it isn't redacted — the log
    /// belongs to the operator, and the endpoint is the thing they need to see.
    pub url: String,
    /// `Ok(detail)` if a review would get through; `Err(detail)` otherwise.
    pub verdict: Result<String, String>,
}

/// Both prompts name JSON on purpose.
///
/// Every format's request asks for a `json_object`, and OpenAI-compatible
/// providers reject that with HTTP 400 unless the *input messages* ask for JSON
/// — a system prompt doesn't count under `API_FORMAT=responses`, where it
/// travels as `instructions` instead. A real review satisfies the rule through
/// [`build_user_prompt`]'s closing line; a probe that didn't would report 400 on
/// a perfectly healthy configuration, which is worse than not probing at all.
const PREFLIGHT_SYSTEM: &str = "Reply with the JSON object {\"ok\":true} and nothing else.";
const PREFLIGHT_USER: &str = "Health check. Reply with the JSON object {\"ok\":true}.";

/// Boot must not hang on an unresponsive provider, so this is far shorter than
/// the 300s a real review allows.
const PREFLIGHT_TIMEOUT_SECS: u64 = 20;

/// Send one throwaway request to the endpoint the AI engines will use.
///
/// Without this, a wrong key or a mismatched `API_FORMAT` stays invisible until
/// someone asks for a review — and then surfaces as a failed review on a real
/// PR, which is both slower to notice and public. The answer is discarded; what
/// is being tested is reachability, authentication, and that the reply has the
/// shape `API_FORMAT` claims. Tool-calling support isn't probed, so a provider
/// that serves plain completions but refuses `tools` still fails on first use —
/// the agent engine falls back to builtin there, which is why that is worth
/// less than a boot-time round trip costs.
///
/// `None` when no AI is configured: the r+/label/rebase commands don't need
/// one, so a bot running without AI is a valid deployment, not a failure.
pub async fn preflight(cfg: &Config) -> Option<Probe> {
    if !cfg.ai_ready() {
        return None;
    }
    let what = format!("API_FORMAT={}", cfg.api_format.trim());
    let proto = match Proto::parse(&cfg.api_format) {
        Ok(p) => p,
        Err(e) => {
            return Some(Probe {
                what,
                url: String::new(),
                verdict: Err(e),
            })
        }
    };
    let (url, verdict) = probe_endpoint(cfg, proto).await;
    Some(Probe { what, url, verdict })
}

async fn probe_endpoint(cfg: &Config, proto: Proto) -> (String, Result<String, String>) {
    let (url, headers) = match ai_endpoint(cfg, proto) {
        Ok(pair) => pair,
        Err(e) => return (String::new(), Err(e)),
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(PREFLIGHT_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (url, Err(format!("cannot build an HTTP client: {e}"))),
    };

    let body = ai_body(cfg, proto, PREFLIGHT_SYSTEM, PREFLIGHT_USER);
    let resp = match client.post(&url).headers(headers).json(&body).send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return (
                url,
                Err(format!("no response within {PREFLIGHT_TIMEOUT_SECS}s")),
            )
        }
        Err(e) => return (url, Err(format!("unreachable: {}", e.without_url()))),
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let snippet: String = text.chars().take(200).collect();

    if !status.is_success() {
        return (url, Err(diagnose(cfg, status, &snippet)));
    }
    // 200 with the wrong body shape means `API_FORMAT` names a protocol this
    // provider doesn't speak here — a review would reach the model and then
    // fail to read the answer, which looks nothing like a config mistake.
    match serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("reply was not JSON: {e}"))
        .and_then(|v| extract_ai_text(proto, &v))
    {
        Ok(_) => (url, Ok(format!("ok ({status})"))),
        Err(e) => (
            url,
            Err(format!(
                "authenticated, but the reply doesn't match the '{}' response shape \
                 — check API_FORMAT against what this provider serves ({e})",
                proto.as_str()
            )),
        ),
    }
}

/// Turn a failing status into the configuration mistake it usually means.
fn diagnose(cfg: &Config, status: reqwest::StatusCode, snippet: &str) -> String {
    let hint = match status.as_u16() {
        401 | 403 => {
            "the provider rejected AI_API_KEY — verify the key itself, that it is enabled \
             for AI_MODEL, and that it belongs to this AI_BASE_URL"
        }
        404 => {
            "no such endpoint — AI_BASE_URL and API_FORMAT disagree with this provider \
             (note that API_FORMAT=anthropic appends /v1/messages, so a base URL already \
             ending in /v1 posts to /v1/v1/messages)"
        }
        429 => "rate-limited or out of quota",
        400 | 422 => "the endpoint answered but rejected the request — AI_MODEL is the usual cause",
        500..=599 => "the provider is failing on its side",
        _ => "unexpected status",
    };
    format!(
        "HTTP {status}: {hint}. Model: {}. Body: {snippet}",
        cfg.ai_model
    )
}

// ---------------------------------------------------------------------------
// Verdict parsing (robust, three-tier fallback)
// ---------------------------------------------------------------------------

/// The checker's system line. Mentioning JSON on purpose: a provider asked for
/// `json_object` rejects the *request* unless the input asks for JSON, and the
/// checker travels through [`call_ai`], which always sets that response format.
pub fn verify_checker_system(lang: Lang) -> &'static str {
    lang.pick(
        "You are an independent second reviewer. Reply with JSON {\"answer\": \"CONFIRM or \
REFUTE\", \"reason\": \"one sentence\"}. REFUTE means the claim is wrong, unsupported by the \
diff, or describes intended behaviour rather than a defect.",
        "你是独立二审。回复 JSON {\"answer\": \"CONFIRM 或 REFUTE\", \"reason\": \"一句话\"}。\
REFUTE 表示该断言错误、diff 中无依据、或描述的是有意行为而非缺陷。",
    )
}

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
\"findings\": [{\"id\": assigned by the system, omit it, \
\"severity\": \"critical|high|medium|low|info\", \
\"type\": \"security|bug|resource|race|error-handling|performance|style|question\", \
\"title\": \"short title\", \"file\": \"path of a file in the diff\", \
\"line\": integer line number (one of the added lines; use 1 if not applicable), \
\"description\": \"the problem, its potential impact, and the code evidence that \
establishes it (quote the exact lines)\", \
\"suggestion\": \"a concrete fix\"}]}. \
Severity guide: critical=security hole (injection/RCE/auth bypass/data loss); \
high=logic bug/resource leak/race/core functionality broken; \
medium=edge case/missing error handling; low=style/maintainability; \
info=suggestion/question/nit. \
Scope: whether the code compiles, parses or imports is CI's question, not yours — you \
have no execution environment, and you may not know the project's language version. \
Never report \"invalid syntax\" / \"does not compile\" / \"cannot be imported\" as a \
finding; when a CI section is provided and it says green, such a claim is wrong by \
construction, and your first hypothesis should be that a construct you don't recognize \
is newer language grammar. \
Write `summary`, `description` and `suggestion` in English. \
Every description must cite the code it is about — a finding without evidence in \
the diff will be discarded by the second reviewer. \
If there is nothing to report, `findings` is an empty array.",
        "你是一名资深、严谨的安全与代码质量审查员。审查 pull request 的代码改动,\
按风险分级输出问题。只报告真实问题,不要为了凑数编造。\
输出必须是严格的 JSON(不要加任何解释性文字、不要 markdown 围栏)。\
JSON schema: \
{\"summary\": \"一句话总体评价\", \
\"findings\": [{\"severity\": \"critical|high|medium|low|info\", \
\"type\": \"security|bug|resource|race|error-handling|performance|style|question\", \
\"title\": \"简短标题\", \"file\": \"改动中的文件路径\", \
\"line\": 整数行号(改动新增行之一,若不适用填1), \
\"description\": \"问题描述、潜在影响,以及支撑它的代码证据(引用具体行)\", \
\"suggestion\": \"具体修复建议\"}]}。\
severity 标准: critical=安全漏洞(注入/RCE/鉴权绕过/数据丢失); \
high=逻辑bug/资源泄漏/竞态/核心功能损坏; \
medium=边界条件/错误处理缺失; low=风格/可维护性; info=建议/疑问/nit。\
职责边界: 代码能否编译/解析/导入是 CI 的问题,不是你的 —— 你没有执行环境,也未必了解\
项目的语言版本。绝不要把\"语法非法\"/\"无法编译\"/\"无法导入\"作为 finding 提出;若提供了\
CI 段且显示通过,这类断言必然错误,你的第一假设应当是你不认识的结构是新的语言语法。\
用中文输出 summary、description 和 suggestion。\
每条 description 必须引用它所针对的代码 —— 没有证据支撑的发现会被二审驳回。\
若无问题, findings 为空数组。",
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
    build_user_prompt_with_sections(
        diff,
        pr_meta,
        truncated,
        previous_review,
        new_commits,
        &[],
        lang,
    )
}

/// [`build_user_prompt`] with extra context sections.
///
/// `sections` are already-rendered blocks (CI status, author feedback) that
/// ride between the incremental context and the diff. Order matters: each
/// section's claims are about what precedes it — the CI section may reference
/// the previous review's findings, the feedback section definitely does — so
/// callers append in that order.
pub fn build_user_prompt_with_sections(
    diff: &str,
    pr_meta: &Value,
    truncated: bool,
    previous_review: Option<&str>,
    new_commits: Option<&str>,
    sections: &[String],
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
                "\n## Previous review (audit trail — for every finding in it, check the \
current diff and state in your summary whether it is now fixed, still present, or was \
rejected as a false positive. Don't repeat findings that were resolved; do repeat any that \
are still present, marking them carried over):\n{p}\n",
                "\n## 上一轮审查意见(对账依据 —— 对其中的每一条 finding,核对本轮 diff,并在总结中 \
逐条说明:已修复/仍存在/被判误报。已解决的不要重复;仍存在的要重复提出并标注为遗留):\n{p}\n"
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
    let extra = sections
        .iter()
        .map(|s| format!("\n{s}\n"))
        .collect::<String>();
    t!(
        lang,
        "PR title: {title}\nPR description: {body}\n{prev_section}{commits_section}{extra}\nBelow is the PR's unified diff (look only at the added code):\n{diff}{note}\n\nReview the change above and answer with the JSON schema given.",
        "PR 标题: {title}\nPR 描述: {body}\n{prev_section}{commits_section}{extra}\n以下是 PR 的 unified diff(只关注新增的代码):\n{diff}{note}\n\n请审查上述改动并按指定 JSON schema 输出。"
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
            let id = f.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let ftype = f.get("type").and_then(|x| x.as_str()).unwrap_or("");
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
            // The id tags the finding across rounds — this is what makes
            // "XRV-… still present in round 3" sayable. Omitted entirely when
            // the verdict carried none (older engines, tests).
            let tag = match (id.is_empty(), ftype.is_empty()) {
                (false, false) => format!("`{id}` · {ftype} · "),
                (false, true) => format!("`{id}` · "),
                _ => String::new(),
            };
            lines.push(format!("- {tag}**`{file}:{line}` — {title}**"));
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
    // What the author already rejected about the previous round. Learning is
    // per-PR context, not a model change: the same pushback that was written
    // into a thread (AstrBot #5's PEP 758 rebuttal) stops being re-reported.
    let feedback = author_feedback_section(gh, cfg, repo, pr_number, lang).await;
    // What CI already proved about this commit. This is the fence against the
    // recurring "SyntaxError on green CI" false positive (#5, #64): the model
    // is told what has actually executed, not asked to guess.
    let ci = ci_section(gh, cfg, repo, pr_number, lang).await;
    let sections: Vec<String> = [ci, feedback].into_iter().flatten().collect();

    let user_prompt = build_user_prompt_with_sections(
        &diff,
        &meta,
        truncated,
        previous_review.as_deref(),
        new_commits.as_deref(),
        &sections,
        lang,
    );

    let raw = call_ai(cfg, system_prompt(lang), &user_prompt).await?;
    let Some(mut verdict) = parse_verdict(&raw) else {
        let body = t!(
            lang,
            "## 🤖 AI Code Review\n\n⚠️ Couldn't parse the model's JSON; raw output below:\n\n```\n{raw}\n```",
            "## 🤖 AI Code Review\n\n⚠️ 未能解析模型返回的 JSON,以下为原始输出:\n\n```\n{raw}\n```"
        );
        let _ = gh.post_issue_comment(repo, pr_number, &body).await;
        return Ok("parse-failed".into());
    };

    // The adversarial second pass. Gated on config: it costs one AI call per
    // significant finding, so a deployment that prefers cheap reviews leaves
    // it off and publishes the first pass as-is. One shared entry so the other
    // engines cannot drift apart on when the pass runs.
    let verify_tag = crate::verify::verify_and_stamp(&mut verdict, cfg, &diff, lang, |prompt| {
        let cfg = cfg.clone();
        async move { call_ai(&cfg, verify_checker_system(lang), &prompt).await }
    })
    .await;
    let engine_tag = format!("builtin{verify_tag}");

    let summary = render_summary(&verdict, &engine_tag, lang);
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

// ---------------------------------------------------------------------------
// Author feedback — what the PR's author already rejected
// ---------------------------------------------------------------------------

/// One piece of author pushback against a previous finding.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthorPushback {
    /// Who replied (or reacted).
    pub author: String,
    /// Their reply text, verbatim. Empty when the signal is a reaction only.
    pub reply: String,
    /// 👎 count on the bot's own inline comment — a dissent the author didn't
    /// write out. 0 when there was none.
    pub downvotes: usize,
    /// Path and line the finding was anchored to, so the model can relate the
    /// pushback to a place in the current diff.
    pub path: String,
    pub line: i64,
}

/// Collect the author's disagreement with the previous round's findings.
///
/// The material is what GitHub already stores about the last review: this
/// bot's inline comments, replies threaded under them, and 👎 reactions on
/// them. The PR author is `pr_author`; the learner's contract is that the
/// feedback describes *that person's* position, so replies from other users
/// (teammates chiming in, drive-bys) are attributed separately — the prompt
/// shows who said what rather than laundering everyone's view into "the
/// author's".
///
/// `app_slug` is the normalized bot login: our inline comments are authored
/// as `slug[bot]`, and the review body they belong to may have degraded to a
/// plain comment, so identity goes by author rather than by review id.
pub async fn fetch_author_pushback(
    gh: &Client,
    repo: &str,
    pr_number: i64,
    app_slug: &str,
) -> Vec<AuthorPushback> {
    let comments = match gh.list_review_comments(repo, pr_number).await {
        Ok(c) => c,
        Err(e) => {
            // Learning is additive; a failed read must not fail the review.
            tracing::warn!("author feedback: review comments of {repo}#{pr_number}: {e}");
            return Vec::new();
        }
    };

    // Our own inline comments, in id order, so thread replies can be grouped.
    let slug = format!("{}[bot]", normalize_bot(app_slug));
    let ours: Vec<&Value> = comments
        .iter()
        .filter(|c| {
            c.pointer("/user/login")
                .and_then(|l| l.as_str())
                .map(|l| l.eq_ignore_ascii_case(&slug))
                .unwrap_or(false)
        })
        .collect();
    if ours.is_empty() {
        return Vec::new();
    }

    let mut pushback = Vec::new();
    for own in ours {
        let own_id = own.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
        let path = own
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        let line = own.get("line").and_then(|l| l.as_i64()).unwrap_or(0);
        let downvotes = own
            .pointer("/reactions/-1")
            .and_then(|n| n.as_i64())
            .unwrap_or(0)
            .max(0) as usize;

        // Replies threaded under this comment, oldest first, with author.
        let mut replies: Vec<(String, String)> = Vec::new();
        for c in &comments {
            let in_reply = c.get("in_reply_to_id").and_then(|i| i.as_i64());
            if in_reply != Some(own_id) {
                continue;
            }
            let who = c
                .pointer("/user/login")
                .and_then(|l| l.as_str())
                .unwrap_or("");
            let body = c.get("body").and_then(|b| b.as_str()).unwrap_or("");
            if who.is_empty() || body.trim().is_empty() {
                continue;
            }
            replies.push((who.to_string(), body.to_string()));
        }

        if replies.is_empty() && downvotes == 0 {
            continue;
        }
        // The reply text is the disagreement itself; the 👎 is its shape when
        // nobody wrote anything. Only the last written reply per (thread,
        // author) pair is kept — the argument tends to be a back-and-forth,
        // and the latest word is the author's current position.
        let mut by_author: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (who, body) in replies {
            by_author.insert(who, body);
        }
        if !by_author.is_empty() || downvotes > 0 {
            // Keep one entry per commenting user; the 👎 rides on the first.
            let mut first = true;
            for (who, reply) in by_author.iter() {
                pushback.push(AuthorPushback {
                    author: who.clone(),
                    reply: reply.clone(),
                    downvotes: if first { downvotes } else { 0 },
                    path: path.clone(),
                    line,
                });
                first = false;
            }
            if by_author.is_empty() {
                pushback.push(AuthorPushback {
                    author: "(reaction only)".to_string(),
                    reply: String::new(),
                    downvotes,
                    path,
                    line,
                });
            }
        }
    }
    pushback
}

/// Strip the `[bot]` suffix the way [`crate::github::normalize_login`] does.
///
/// A local helper rather than a re-export because the learner reads it as
/// "login to compare against `slug[bot]`", and keeping the suffix-append
/// explicit here is what makes that contract visible.
fn normalize_bot(slug: &str) -> String {
    let lower = slug.trim().to_lowercase();
    lower.strip_suffix("[bot]").unwrap_or(&lower).to_string()
}

/// Render the collected pushback as the prompt section the model audits.
///
/// Empty input is `None`, so a first review carries no dead section. The
/// instruction is the contract that makes the section matter: a rejected
/// finding is not repeated unless there is verifiable new evidence, and any
/// rebuttal the model disagrees with must be argued, not ignored.
pub fn render_author_feedback(items: &[AuthorPushback], lang: Lang) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut lines = vec![match lang {
        Lang::En => "## Author feedback on the previous review (be bound by it)".to_string(),
        Lang::Zh => "## 作者对上一轮审查的反馈(必须遵守)".to_string(),
    }];
    lines.push(String::new());
    lines.push(match lang {
        Lang::En => "The PR's author and others replied to findings of the previous round \
under the inline comments. Their disagreement is input you must weigh: do NOT repeat a \
finding of the same claim at the same place unless the current diff contains verifiable \
new evidence (a spec citation, an actual error, a runtime failure) that answers the \
rebuttal. If you believe the author is wrong, say so in `summary` with the argument — \
never by silently re-reporting the same finding."
            .to_string(),
        Lang::Zh => "作者及其他人曾在内联评论下对上一轮的发现作出回应。这些不同意见是你必须 \
权衡的输入:除非当前 diff 中存在可验证的新证据(规范引用、实际报错、运行失败)足以回应 \
反驳,否则**不要**在同一位置重复同一断言的 finding。若你认为作者是错的,请在 `summary` \
中给出论证 —— 而不是默默重报同一条发现。"
            .to_string(),
    });
    lines.push(String::new());
    for item in items {
        let mut entry = format!("- `{}`:{} — ", item.path, item.line);
        let downvotes = item.downvotes;
        if downvotes > 0 {
            entry.push_str(&format!("👎 ×{downvotes}"));
            if !item.reply.is_empty() {
                entry.push_str("; ");
            }
        }
        if !item.reply.is_empty() {
            entry.push_str(&format!("@{}: \"{}\"", item.author, item.reply));
        } else if downvotes == 0 {
            continue;
        }
        lines.push(entry);
    }
    Some(lines.join("\n"))
}

/// The feedback section for a prompt: fetched, budgeted, `None` when empty.
///
/// Split from [`fetch_author_pushback`] so callers that already have the
/// items (tests, future engines) can share the wording.
pub async fn author_feedback_section(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    lang: Lang,
) -> Option<String> {
    let items = fetch_author_pushback(gh, repo, pr_number, &gh.app_slug).await;
    let section = render_author_feedback(&items, lang)?;
    // Budget it like every other context section: a thread that turned into a
    // shouting match must not eat the diff's prompt share. Half of what the
    // previous review gets, which has proven enough for disagreement prose.
    Some(truncate(&section, cfg.max_diff_chars / 4).0)
}

// ---------------------------------------------------------------------------
// CI state — the ground truth about "does it build"
// ---------------------------------------------------------------------------

/// What CI says about the commit under review, as far as we can see.
#[derive(Debug, Clone, PartialEq)]
pub enum CiState {
    /// Every check that ran succeeded. The names travel because the prompt
    /// cites them: "CI passed (build, tests)" is an argument, "CI passed" is
    /// a vibe.
    Green(Vec<String>),
    /// At least one check failed.
    Failed(Vec<String>),
    /// Checks exist but none has reached a verdict yet.
    Pending(Vec<String>),
    /// No checks are configured, or the App cannot read them. Deliberately
    /// distinct from `Green`: silence is not success, and a review that
    /// treats it so would repeat exactly the mistake this section exists to
    /// prevent.
    Unknown,
}

impl CiState {
    /// Fold the Checks API and the Status API into one state.
    ///
    /// A failed check wins over anything else; green requires *every* signal
    /// we can see to be green, with at least one present. Both endpoints
    /// partition the CI world between them, so only the union is the truth.
    pub fn combine(checks: &[Value], statuses: &[Value]) -> CiState {
        let check_states = |conclusion: &str| {
            checks
                .iter()
                .any(|c| c.get("conclusion").and_then(|x| x.as_str()) == Some(conclusion))
        };
        let status_states = |state: &str| {
            statuses
                .iter()
                .any(|s| s.get("state").and_then(|x| x.as_str()) == Some(state))
        };

        if check_states("failure")
            || check_states("timed_out")
            || check_states("action_required")
            || status_states("failure")
            || status_states("error")
        {
            let mut names: Vec<String> = checks
                .iter()
                .filter(|c| {
                    matches!(
                        c.get("conclusion").and_then(|x| x.as_str()),
                        Some("failure") | Some("timed_out") | Some("action_required")
                    )
                })
                .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                .chain(
                    statuses
                        .iter()
                        .filter(|s| {
                            matches!(
                                s.get("state").and_then(|x| x.as_str()),
                                Some("failure") | Some("error")
                            )
                        })
                        .filter_map(|s| s.get("context").and_then(|n| n.as_str())),
                )
                .map(String::from)
                .collect();
            names.sort();
            names.dedup();
            return CiState::Failed(names);
        }

        let green_names = || -> Vec<String> {
            let mut names: Vec<String> = checks
                .iter()
                .filter(|c| c.get("conclusion").and_then(|x| x.as_str()) == Some("success"))
                .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                .chain(
                    statuses
                        .iter()
                        .filter(|s| s.get("state").and_then(|x| x.as_str()) == Some("success"))
                        .filter_map(|s| s.get("context").and_then(|n| n.as_str())),
                )
                .map(String::from)
                .collect();
            names.sort();
            names.dedup();
            names
        };

        // Anything still running (queued/in_progress, pending status) is a
        // reason not to claim green even if some checks passed.
        let pending = check_states("queued")
            || check_states("in_progress")
            || check_states("pending")
            || status_states("pending");
        if pending {
            return CiState::Pending(green_names());
        }

        let green = green_names();
        if green.is_empty() {
            CiState::Unknown
        } else {
            CiState::Green(green)
        }
    }
}

/// Fetch the CI state of one commit, or [`CiState::Unknown`] when it cannot
/// be read.
///
/// Permission errors (no `Checks: read`), network errors and empty configs
/// all land in `Unknown` rather than propagating: the review must never fail
/// because a context section couldn't load, and — critically — must never
/// read a load failure as success.
pub async fn ci_state_for(gh: &Client, repo: &str, sha: &str) -> CiState {
    let checks = match gh.check_runs(repo, sha).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("ci state: check-runs of {repo}@{sha}: {e}");
            return CiState::Unknown;
        }
    };
    let statuses = match gh.commit_statuses(repo, sha).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("ci state: statuses of {repo}@{sha}: {e}");
            // The Checks half can still be meaningful on its own; only an
            // empty union is Unknown.
            return CiState::combine(&checks, &[]);
        }
    };
    CiState::combine(&checks, &statuses)
}

/// The prompt section that states what CI already verified.
///
/// The binding rule is the fix for the recurring false positive (AstrBot #5
/// and #64, both critical "SyntaxError" claims on code whose CI had passed):
/// whether code compiles is CI's question, not the reviewer's. A reviewer
/// with no execution environment who sees green CI and believes a syntax
/// error should first doubt its own knowledge of the language version — new
/// grammar is legal somewhere — and say so in `summary` instead of filing a
/// critical finding.
pub fn render_ci_section(state: &CiState, lang: Lang) -> Option<String> {
    match state {
        CiState::Green(names) => {
            let list = names.join(", ");
            Some(match lang {
                Lang::En => format!(
                    "## CI status (ground truth about whether the code builds)\n\n\
CI has **passed** on this exact commit — compilation, imports and the test suite were \
executed and succeeded ({list}). Do NOT report findings of the form \"this does not \
compile\", \"invalid syntax\", or \"the module cannot be imported\": those are CI's \
questions, and CI has already answered them. If you believe a construct is a syntax \
error while CI is green, assume your knowledge of the language version is stale — newer \
grammar (e.g. Python 3.14's paren-less multi-exception except) is legal — and at most \
mention the doubt in `summary`, never as a finding."
                ),
                Lang::Zh => format!(
                    "## CI 状态(关于代码能否编译的既定事实)\n\n\
CI 已在本提交上**通过** —— 编译、导入与测试套件均已实际执行并成功({list})。\
不要报告\"无法编译\"、\"语法非法\"、\"模块无法导入\"之类的 finding:这些是 CI 的问题,CI 已经作答。\
若你认为某处是语法错误而 CI 全绿,应首先怀疑自己对语言版本的了解已过时 —— 新语法\
(如 Python 3.14 允许无括号的多异常 except)是合法的 —— 至多在 `summary` 中提出疑问,\
绝不能作为 finding 提出。"
                ),
            })
        }
        CiState::Failed(names) => {
            let list = names.join(", ");
            Some(match lang {
                Lang::En => format!(
                    "## CI status (ground truth about whether the code builds)\n\n\
These checks **failed** on this commit: {list}. Do not re-report what a failing CI run \
already proves; if you have something to say about those failures, add information the \
CI log does not contain, and keep the severity honest — a CI failure that the author's \
own checklist already explains is not a critical finding."
                ),
                Lang::Zh => format!(
                    "## CI 状态(关于代码能否编译的既定事实)\n\n\
以下检查在本提交上**失败**: {list}。不要重复报告失败的 CI 已经证明的问题;若对这些失败 \
有补充,请提供 CI 日志中没有的信息,并保持严重度诚实 —— 作者说明里已解释的 CI 失败不是 \
critical。"
                ),
            })
        }
        CiState::Pending(names) => {
            let list = names.join(", ");
            Some(match lang {
                Lang::En => format!(
                    "## CI status\n\n\
CI has not reached a verdict yet (passed so far: {list}). Report code problems as code \
problems; do not claim the code does or does not compile, and treat \"this will break \
the build\" as a hypothesis to phrase in `summary`, not a finding."
                ),
                Lang::Zh => format!(
                    "## CI 状态\n\n\
CI 尚未给出结论(目前已通过: {list})。请以代码问题本身作答;不要断言代码能否编译,\
\"这会破坏构建\"只能作为 `summary` 中的假设表述,不能作为 finding。"
                ),
            })
        }
        CiState::Unknown => None,
    }
}

/// The CI section for a prompt: fetched from the PR's head, `None` when the
/// state is unknown (no checks configured, or no permission to read them).
pub async fn ci_section(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    lang: Lang,
) -> Option<String> {
    let sha = match gh.get_pr(repo, pr_number).await {
        Ok(meta) => meta
            .pointer("/head/sha")
            .and_then(|s| s.as_str())
            .map(String::from)?,
        Err(e) => {
            tracing::warn!("ci state: head sha of {repo}#{pr_number}: {e}");
            return None;
        }
    };
    let state = ci_state_for(gh, repo, &sha).await;
    let section = render_ci_section(&state, lang)?;
    Some(truncate(&section, cfg.max_diff_chars / 4).0)
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

    // ---- author feedback --------------------------------------------------

    /// The AstrBot #5 shape: the author rebuts a critical finding in the
    /// thread ("It's the python 3.14 syntax") and thumbs-downs the comment.
    /// The section must carry the rebuttal and bind the model to it.
    #[test]
    fn author_feedback_is_rendered_with_the_rebuttal_and_the_downvote() {
        let items = vec![
            AuthorPushback {
                author: "BegoniaHe".into(),
                reply: "It's the python 3.14 syntax. Exceptions can be used without \
parentheses."
                    .into(),
                downvotes: 1,
                path: "astrbot/core/tools/web_search_tools.py".into(),
                line: 1432,
            },
            AuthorPushback {
                author: "(reaction only)".into(),
                reply: String::new(),
                downvotes: 2,
                path: "src/other.rs".into(),
                line: 10,
            },
        ];
        let en = render_author_feedback(&items, Lang::En).expect("some");
        assert!(en.contains("Author feedback"), "{en}");
        assert!(en.contains("BegoniaHe"), "{en}");
        assert!(en.contains("python 3.14"), "{en}");
        assert!(
            en.contains("`astrbot/core/tools/web_search_tools.py`:1432"),
            "{en}"
        );
        assert!(en.contains("👎 ×1"), "{en}");
        assert!(en.contains("👎 ×2"), "{en}");
        assert!(en.contains("do NOT repeat"), "{en}");

        // The Chinese one is Chinese; the English one carries no CJK.
        let zh = render_author_feedback(&items, Lang::Zh).expect("some");
        assert!(zh.contains("作者对上一轮审查的反馈"), "{zh}");
        assert!(
            !en.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "{en}"
        );
    }

    #[test]
    fn no_feedback_means_no_section() {
        assert_eq!(render_author_feedback(&[], Lang::En), None);
    }

    #[test]
    fn feedback_section_lands_in_the_prompt_after_the_previous_review() {
        let meta = serde_json::json!({"title": "T", "body": "B"});
        let p = build_user_prompt_with_sections(
            "d",
            &meta,
            false,
            Some("prev review body"),
            None,
            &["## Author feedback\n\n- `f.rs`:1 — @bob: \"not a bug\"".to_string()],
            Lang::En,
        );
        assert!(p.contains("Author feedback"), "{p}");
        assert!(p.contains("not a bug"), "{p}");
        // The feedback comes after the review it comments on.
        let review_at = p.find("prev review body").unwrap();
        let feedback_at = p.find("Author feedback").unwrap();
        assert!(review_at < feedback_at, "{p}");
    }

    /// Identity for finding our own inline comments. The bot authors them as
    /// `slug[bot]`, so the fetcher must build that suffix itself — a bare
    /// slug never matched, and an unfound thread is an unlearned rebuttal.
    #[test]
    fn normalize_bot_strips_the_suffix() {
        assert_eq!(normalize_bot("xero-team-bot"), "xero-team-bot");
        assert_eq!(normalize_bot("xero-team-bot[bot]"), "xero-team-bot");
        assert_eq!(normalize_bot(" Xero-Team-Bot "), "xero-team-bot");
    }

    // ---- CI state ---------------------------------------------------------

    fn check(name: &str, conclusion: &str) -> Value {
        json!({"name": name, "conclusion": conclusion})
    }

    fn status(context: &str, state: &str) -> Value {
        json!({"context": context, "state": state})
    }

    /// The false positive this whole section exists to prevent is a
    /// "SyntaxError" critical on a commit whose CI ran green. The combine
    /// rules must put every observable signal into that decision.
    #[test]
    fn ci_state_combines_checks_and_statuses() {
        // All green across both APIs.
        let s = CiState::combine(
            &[check("build", "success"), check("tests", "success")],
            &[status("coverage", "success")],
        );
        assert_eq!(
            s,
            CiState::Green(vec!["build".into(), "coverage".into(), "tests".into()]),
            "{s:?}"
        );

        // A failure anywhere wins, and its name is reported.
        let s = CiState::combine(&[check("build", "success"), check("tests", "failure")], &[]);
        assert_eq!(s, CiState::Failed(vec!["tests".into()]), "{s:?}");
        // Status-API failures count too.
        let s = CiState::combine(&[check("build", "success")], &[status("lint", "error")]);
        assert_eq!(s, CiState::Failed(vec!["lint".into()]), "{s:?}");

        // Pending beats green: some checks passed but one is still running.
        let s = CiState::combine(&[check("build", "success"), check("deploy", "queued")], &[]);
        assert!(matches!(s, CiState::Pending(_)), "{s:?}");

        // Nothing at all is Unknown — silence is not success.
        assert_eq!(CiState::combine(&[], &[]), CiState::Unknown);
        // A 403'd checks call with no statuses is also Unknown, not Green.
        assert_eq!(CiState::combine(&[], &[]), CiState::Unknown);
    }

    /// The rendered section must bind the model: no compile findings on green
    /// CI, and the newer-grammar hypothesis named (the AstrBot #5/#64 lesson).
    #[test]
    fn green_ci_section_forbids_compile_findings() {
        let state = CiState::Green(vec!["build".into(), "pytest".into()]);
        let en = render_ci_section(&state, Lang::En).expect("some");
        assert!(en.contains("CI has **passed**"), "{en}");
        assert!(en.contains("build, pytest"), "{en}");
        assert!(en.contains("does not compile"), "{en}");
        assert!(en.contains("Python 3.14"), "{en}");

        let zh = render_ci_section(&state, Lang::Zh).expect("some");
        assert!(zh.contains("CI 已在本提交上**通过**"), "{zh}");
        assert!(zh.contains("不要报告"), "{zh}");
    }

    #[test]
    fn failed_ci_section_names_the_failures() {
        let state = CiState::Failed(vec!["tests".into()]);
        let en = render_ci_section(&state, Lang::En).expect("some");
        assert!(en.contains("tests"), "{en}");
        assert!(en.contains("failed"), "{en}");
    }

    /// Unknown CI renders nothing: a review without signal stays quiet about
    /// CI rather than inventing one.
    #[test]
    fn unknown_ci_is_no_section() {
        assert_eq!(render_ci_section(&CiState::Unknown, Lang::En), None);
        assert_eq!(render_ci_section(&CiState::Unknown, Lang::Zh), None);
    }

    /// The scope rule now lives in both language briefs, so neither language
    /// produces the compile-findings false positive.
    #[test]
    fn system_prompt_draws_the_ci_scope_boundary() {
        for p in [system_prompt(Lang::En), system_prompt(Lang::Zh)] {
            assert!(p.contains("CI"), "{p}");
        }
        assert!(
            system_prompt(Lang::En).contains("does not compile"),
            "{}",
            system_prompt(Lang::En)
        );
        assert!(
            system_prompt(Lang::Zh).contains("无法编译"),
            "{}",
            system_prompt(Lang::Zh)
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
