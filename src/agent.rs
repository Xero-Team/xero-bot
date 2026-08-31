//! Native review agent: a tool-calling loop where the model explores the
//! project through GitHub API tools before reviewing the diff.
//!
//! This is the Vercel-friendly engine — no subprocesses, no local clone.
//! "Incremental" comes from injecting the bot's previous review on this PR
//! plus the commits since, fetched from GitHub itself (no other state).

use serde_json::{json, Value};

use crate::config::Config;
use crate::github::{Client, GhError};
use crate::lang::Lang;
use crate::review::{
    ai_endpoint, build_inline_comments, parse_added_lines, parse_verdict, render_summary, truncate,
    Proto,
};
use crate::t;

/// The reviewer's brief, in the language the review will be written in.
fn agent_system_prompt(lang: Lang) -> &'static str {
    lang.pick(AGENT_SYSTEM_PROMPT_EN, AGENT_SYSTEM_PROMPT_ZH)
}

const AGENT_SYSTEM_PROMPT_EN: &str = "\
You are a senior, rigorous code reviewer working inside a GitHub PR review workflow.

You have tools available (via function calling):
- list_files(path): list the files/subdirectories at a path in the repository, to learn the \
project's structure. Pass \"\" on the first call (repository root).
- read_file(path): read one file's text content. Prefer the build config (package.json, \
Cargo.toml, go.mod), the README, and the source files this change touches.
- search_code(query): search code in the repository. Query syntax: \"term in:file\", or just \
keywords.

Workflow (follow it):
1. Explore first: call list_files(\"\") for the root; use list_files on the directories the \
changed files live in; read 1-4 key files (build config, the modules involved) to build \
context.
2. Then review: read the diff with that context in mind, and report only real problems — \
don't pad the list.
3. When you're done, call submit_review with the final result, then stop.

submit_review's `verdict` argument must match this schema:
{\"summary\": \"one-sentence overall assessment (in English)\",
 \"findings\": [{\"severity\": \"critical|high|medium|low|info\",
   \"title\": \"short title\", \"file\": \"path of a file in the diff\",
   \"line\": integer line number (one of the added lines; use 1 if not applicable),
   \"description\": \"the problem and its potential impact (in English)\", \
\"suggestion\": \"a concrete fix (in English)\"}]}

Severity guide: critical=security hole (injection/RCE/auth bypass/data loss); high=logic \
bug/resource leak/race/core functionality broken; medium=edge case/missing error handling; \
low=style/maintainability; info=suggestion/question/nit. If there is nothing to report, \
`findings` is an empty array. If a \"Previous review\" section is provided, check which of \
those were fixed and don't repeat findings that are already resolved.";

const AGENT_SYSTEM_PROMPT_ZH: &str = "\
你是一名资深、严谨的代码审查员,在 GitHub PR 审查工作流中工作。

你有两类工具可用(通过函数调用):
- list_files(path): 列出仓库中某目录下的文件/子目录,用于了解项目结构。首次调用建议传 \"\"(仓库根目录)。
- read_file(path): 读取仓库中一个文件的内容(文本)。优先读构建配置(如 package.json、Cargo.toml、go.mod)、README、与本次改动相关的源文件。
- search_code(query): 在仓库内搜索代码。query 语法: \"搜索词 in:file\" 或直接关键词。

工作流程(务必遵守):
1. 先探索: 调用 list_files(\"\") 看根目录;根据改动文件所在目录,用 list_files 了解其结构;读 1-4 个关键文件(构建配置/改动涉及的模块)建立项目背景。
2. 再审查: 结合项目背景审查 diff,只报告真实问题,不凑数。
3. 结束审查时,调用 submit_review 提交最终结果,然后停止。

submit_review 的 verdict 参数必须符合 schema:
{\"summary\": \"一句话总体评价(中文)\",
 \"findings\": [{\"severity\": \"critical|high|medium|low|info\",
   \"title\": \"简短标题\", \"file\": \"改动中的文件路径\",
   \"line\": 整数行号(改动新增行之一,若不适用填1),
   \"description\": \"问题描述与潜在影响(中文)\", \"suggestion\": \"具体修复建议(中文)\"}]}

severity 标准: critical=安全漏洞(注入/RCE/鉴权绕过/数据丢失); high=逻辑bug/资源泄漏/竞态/核心功能损坏; medium=边界条件/错误处理缺失; low=风格/可维护性; info=建议/疑问/nit。若无问题, findings 为空数组。若提供\"上一轮审查意见\",核对哪些已修复、避免重复已解决的发现。";

/// The tools the model gets, as `(name, description, JSON-Schema parameters)`.
///
/// Declared once and reshaped per protocol by [`tool_definitions`]: the three
/// protocols disagree about how to wrap a tool, but all three carry exactly
/// these three fields.
fn tool_specs() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "list_files",
            "List files and subdirectories at a path in the repository (empty path = root).",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory path, \"\" for root"}
                },
                "required": ["path"]
            }),
        ),
        (
            "read_file",
            "Read one file's text content from the repository.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "File path"}},
                "required": ["path"]
            }),
        ),
        (
            "search_code",
            "Search code in this repository (GitHub code search syntax).",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Search query"}},
                "required": ["query"]
            }),
        ),
        (
            "submit_review",
            "Submit the final review verdict and finish.",
            json!({
                "type": "object",
                "properties": {
                    "verdict": {
                        "type": "object",
                        "properties": {
                            "summary": {"type": "string"},
                            "findings": {"type": "array", "items": {"type": "object"}}
                        },
                        "required": ["summary", "findings"]
                    }
                },
                "required": ["verdict"]
            }),
        ),
    ]
}

/// The same tools in the shape `proto` expects.
fn tool_definitions(proto: Proto) -> Value {
    let defs: Vec<Value> = tool_specs()
        .into_iter()
        .map(|(name, description, parameters)| match proto {
            Proto::Chat => json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                }
            }),
            // The Responses API flattens the same three fields to the top
            // level. `strict` has to be spelled out: it defaults to true
            // there, and strict mode requires `additionalProperties: false`
            // on every object in the schema — which these schemas
            // deliberately don't set, since a finding is a free-form object.
            Proto::Responses => json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
                "strict": false,
            }),
            Proto::Anthropic => json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            }),
        })
        .collect();
    Value::Array(defs)
}

/// One tool call the model asked for, in protocol-independent form.
struct ToolCall {
    /// The id to quote when returning the result — `tool_call_id`,
    /// `call_id` or `tool_use_id` depending on the protocol.
    id: String,
    name: String,
    args: Value,
}

/// What one model turn contained.
struct Turn {
    calls: Vec<ToolCall>,
    /// The assistant's prose, if it wrote any.
    text: String,
}

/// The running conversation, in whichever protocol we're speaking.
///
/// Each protocol has its own idea of what a message history is: chat keeps
/// system/assistant/tool messages in one array, the Responses API keeps the
/// system text out in `instructions` and appends flat items, and Anthropic
/// wants all of a turn's tool results inside a single user message. Confining
/// those differences here keeps the loop below protocol-blind.
struct Conversation {
    proto: Proto,
    system: String,
    items: Vec<Value>,
}

impl Conversation {
    fn new(proto: Proto, system: &str, user: &str) -> Conversation {
        let items = match proto {
            Proto::Chat => vec![
                json!({"role": "system", "content": system}),
                json!({"role": "user", "content": user}),
            ],
            // Here the system text rides in a top-level field instead.
            Proto::Responses | Proto::Anthropic => vec![json!({"role": "user", "content": user})],
        };
        Conversation {
            proto,
            system: system.to_string(),
            items,
        }
    }

    fn body(&self, cfg: &Config) -> Value {
        match self.proto {
            Proto::Chat => json!({
                "model": cfg.ai_model,
                "messages": self.items,
                "tools": tool_definitions(self.proto),
                "tool_choice": "auto",
                "temperature": 0.2,
            }),
            Proto::Responses => json!({
                "model": cfg.ai_model,
                "instructions": self.system,
                "input": self.items,
                "tools": tool_definitions(self.proto),
                "tool_choice": "auto",
            }),
            Proto::Anthropic => json!({
                "model": cfg.ai_model,
                "max_tokens": 4096,
                "system": self.system,
                "messages": self.items,
                "tools": tool_definitions(self.proto),
            }),
        }
    }

    /// Read one response, append it to the history, and report what the model
    /// asked for. Appending is part of reading: every protocol rejects a tool
    /// result whose call isn't already in the history.
    fn read_turn(&mut self, out: &Value) -> Result<Turn, String> {
        let mut calls = Vec::new();
        let mut text = String::new();
        match self.proto {
            Proto::Chat => {
                let message = out
                    .pointer("/choices/0/message")
                    .cloned()
                    .ok_or("chat API: no message in the response")?;
                if let Some(raw) = message.get("tool_calls").and_then(|c| c.as_array()) {
                    for call in raw {
                        calls.push(ToolCall {
                            id: str_at(call, "id").to_string(),
                            name: pointer_str(call, "/function/name").to_string(),
                            args: parse_args(pointer_str(call, "/function/arguments")),
                        });
                    }
                }
                text.push_str(str_at(&message, "content"));
                self.items.push(message);
            }
            Proto::Responses => {
                let output = out
                    .get("output")
                    .and_then(|o| o.as_array())
                    .cloned()
                    .ok_or("responses API: no output in the response")?;
                for item in &output {
                    match str_at(item, "type") {
                        "function_call" => calls.push(ToolCall {
                            id: str_at(item, "call_id").to_string(),
                            name: str_at(item, "name").to_string(),
                            args: parse_args(str_at(item, "arguments")),
                        }),
                        "message" => collect_text(&mut text, item, "output_text"),
                        _ => {}
                    }
                }
                // Echo back the whole output array rather than just the calls:
                // a `function_call_output` whose `call_id` has no matching
                // `function_call` in the input is rejected, and reasoning items
                // have to stay alongside the call they belong to.
                self.items.extend(output);
            }
            Proto::Anthropic => {
                let content = out
                    .get("content")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .ok_or("anthropic API: no content in the response")?;
                for block in &content {
                    match str_at(block, "type") {
                        "tool_use" => calls.push(ToolCall {
                            id: str_at(block, "id").to_string(),
                            name: str_at(block, "name").to_string(),
                            // Already an object here, not a JSON string.
                            args: block.get("input").cloned().unwrap_or_else(|| json!({})),
                        }),
                        "text" => text.push_str(str_at(block, "text")),
                        _ => {}
                    }
                }
                self.items
                    .push(json!({"role": "assistant", "content": content}));
            }
        }
        Ok(Turn { calls, text })
    }

    /// Return every result from one turn.
    fn push_tool_results(&mut self, results: &[(ToolCall, String)]) {
        match self.proto {
            Proto::Chat => {
                for (call, result) in results {
                    self.items.push(json!({
                        "role": "tool",
                        "tool_call_id": call.id,
                        "content": result,
                    }));
                }
            }
            Proto::Responses => {
                for (call, result) in results {
                    self.items.push(json!({
                        "type": "function_call_output",
                        "call_id": call.id,
                        "output": result,
                    }));
                }
            }
            // Anthropic requires every `tool_result` for a turn to sit in one
            // user message, and that message to come immediately after the
            // assistant turn — one message per result is a 400.
            Proto::Anthropic => {
                if results.is_empty() {
                    return;
                }
                let blocks: Vec<Value> = results
                    .iter()
                    .map(|(call, result)| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": call.id,
                            "content": result,
                        })
                    })
                    .collect();
                self.items.push(json!({"role": "user", "content": blocks}));
            }
        }
    }

    /// All three protocols take a plain user message in the same shape.
    fn push_user(&mut self, text: &str) {
        self.items.push(json!({"role": "user", "content": text}));
    }
}

/// `v[key]` as a string, or `""`.
fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

fn pointer_str<'a>(v: &'a Value, path: &str) -> &'a str {
    v.pointer(path).and_then(|x| x.as_str()).unwrap_or("")
}

/// Tool arguments arrive as a JSON *string* in both OpenAI protocols. A model
/// that emits malformed JSON shouldn't kill the review — the tool will just
/// report a missing argument and the model can try again.
fn parse_args(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
}

/// Append the text blocks of `item.content` whose type is `kind`.
fn collect_text(out: &mut String, item: &Value, kind: &str) {
    let Some(content) = item.get("content").and_then(|c| c.as_array()) else {
        return;
    };
    for c in content {
        if str_at(c, "type") == kind {
            out.push_str(str_at(c, "text"));
        }
    }
}

pub struct AgentOutcome {
    pub verdict: Option<Value>,
    pub turns_used: usize,
    pub timed_out: bool,
}

/// Run the agent loop. Returns the outcome; posting is done by the caller.
pub async fn run_agent(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    lang: Lang,
) -> Result<AgentOutcome, String> {
    // gather PR context
    let meta = gh
        .get_pr(repo, pr_number)
        .await
        .map_err(|e| e.to_string())?;
    let base_ref = meta
        .get("base")
        .and_then(|b| b.get("ref"))
        .and_then(|r| r.as_str())
        .unwrap_or("main")
        .to_string();
    let full_diff = gh
        .get_pr_diff(repo, pr_number)
        .await
        .map_err(|e| e.to_string())?;
    let (diff, _truncated) = truncate(&full_diff, cfg.max_diff_chars);

    let (previous_review, new_commits) =
        crate::review::fetch_incremental_context(gh, repo, pr_number, cfg.max_diff_chars).await;

    let title = meta.get("title").and_then(|t| t.as_str()).unwrap_or("");
    let body: String = meta
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .chars()
        .take(1500)
        .collect();
    let prev_section = previous_review
        .map(|p| {
            t!(
                lang,
                "\n## Previous review (check whether these were fixed; don't repeat them):\n{p}\n",
                "\n## 上一轮审查意见(核对是否已修复,避免重复):\n{p}\n"
            )
        })
        .unwrap_or_default();
    let commits_section = new_commits
        .map(|c| {
            t!(
                lang,
                "\n## Commits pushed since the previous review (focus on the increment):\n{c}\n",
                "\n## 自上一轮审查以来的新提交(重点增量):\n{c}\n"
            )
        })
        .unwrap_or_default();
    let trunc_note = if _truncated {
        lang.pick("(diff truncated)", "(diff 已截断)")
    } else {
        ""
    };

    let user_prompt = t!(
        lang,
        "Repository: {repo}\nBase branch: {base_ref}\nPR title: {title}\nPR description: {body}{prev_section}{commits_section}\n\n\
Use the tools to learn the project's structure first, then review the diff below{trunc_note}. Call submit_review when you're done:\n\n{diff}",
        "仓库: {repo}\n基准分支: {base_ref}\nPR 标题: {title}\nPR 描述: {body}{prev_section}{commits_section}\n\n\
先用工具了解项目结构,再审查以下 diff{trunc_note}。完成后调用 submit_review 提交:\n\n{diff}"
    );

    // Whatever protocol the operator configured — the loop below doesn't care
    // which, but it used to: it posted to `/chat/completions` unconditionally.
    let proto = Proto::parse(&cfg.api_format)?;
    let (url, headers) = ai_endpoint(cfg, proto)?;
    let mut convo = Conversation::new(proto, agent_system_prompt(lang), &user_prompt);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(cfg.agent_timeout_secs);

    let mut turns_used = 0usize;
    let mut timed_out = false;

    while turns_used < cfg.agent_max_turns {
        if tokio::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        turns_used += 1;

        let resp = client
            .post(&url)
            .headers(headers.clone())
            .json(&convo.body(cfg))
            .send()
            .await
            .map_err(|e| format!("agent step failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "agent step HTTP {status} at the {} endpoint: {}",
                proto.as_str(),
                text.chars().take(300).collect::<String>()
            ));
        }
        let out: Value =
            serde_json::from_str(&text).map_err(|e| format!("agent step bad JSON: {e}"))?;
        let turn = convo.read_turn(&out)?;

        if turn.calls.is_empty() {
            // Plain prose. Accept it if it happens to be a verdict anyway,
            // otherwise point the model back at submit_review.
            if let Some(verdict) = parse_verdict(&turn.text) {
                if verdict.get("findings").is_some() {
                    return Ok(AgentOutcome {
                        verdict: Some(verdict),
                        turns_used,
                        timed_out: false,
                    });
                }
            }
            convo.push_user(lang.pick(
                "Call submit_review with your final result (the `verdict` matching the schema).",
                "请调用 submit_review 提交最终审查结果(verdict 按 schema)。",
            ));
            continue;
        }

        let mut submitted: Option<Value> = None;
        let mut results: Vec<(ToolCall, String)> = Vec::new();
        for call in turn.calls {
            let result = if call.name == "submit_review" {
                let verdict = call.args.get("verdict").cloned().unwrap_or(Value::Null);
                if verdict.is_object() {
                    submitted = Some(verdict);
                }
                // Answer it regardless, so the history stays valid for the
                // next turn if the verdict was unusable.
                "review submitted".to_string()
            } else {
                let raw = execute_tool(gh, repo, &base_ref, &call.name, &call.args).await;
                // keep tool results bounded
                raw.chars().take(8000).collect()
            };
            results.push((call, result));
        }
        convo.push_tool_results(&results);

        if let Some(verdict) = submitted {
            return Ok(AgentOutcome {
                verdict: Some(verdict),
                turns_used,
                timed_out: false,
            });
        }
    }

    Ok(AgentOutcome {
        verdict: None,
        turns_used,
        timed_out,
    })
}

/// Execute one agent tool against the GitHub API. Returns a text result.
async fn execute_tool(gh: &Client, repo: &str, base_ref: &str, name: &str, args: &Value) -> String {
    match name {
        "list_files" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
            match gh.list_dir(repo, path, base_ref).await {
                Ok(entries) => {
                    let lines: Vec<String> = entries
                        .iter()
                        .filter_map(|e| {
                            let ename = e.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                            let etype = e.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                            Some(format!("{etype}\t{ename}"))
                        })
                        .collect();
                    if lines.is_empty() {
                        "(empty directory or not found)".to_string()
                    } else {
                        lines.join("\n")
                    }
                }
                Err(e) => format!("error: {e}"),
            }
        }
        "read_file" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
            match gh.get_file_content(repo, path, base_ref).await {
                Ok(Some(content)) => content,
                Ok(None) => "(path is a directory; use list_files)".to_string(),
                Err(e) => format!("error: {e}"),
            }
        }
        "search_code" => {
            let query = args.get("query").and_then(|q| q.as_str()).unwrap_or("");
            match gh
                .get(&format!(
                    "/search/code?q={}+repo:{repo}&per_page=20",
                    urlencode(query)
                ))
                .await
            {
                Ok(v) => {
                    let items = v
                        .get("items")
                        .and_then(|i| i.as_array())
                        .cloned()
                        .unwrap_or_default();
                    if items.is_empty() {
                        "(no results)".to_string()
                    } else {
                        items
                            .iter()
                            .filter_map(|i| {
                                let p = i.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                                Some(p.to_string())
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                }
                Err(e) => format!("error: {e}"),
            }
        }
        _ => format!("unknown tool: {name}"),
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            // keep characters meaningful in GitHub search syntax
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'+'
            | b':'
            | b'"' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Full agent review run: agent loop → fallback to builtin verdict on
/// failure → post. Mirrors run_builtin's error-reporting contract.
pub async fn run_agent_review(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    lang: Lang,
) -> String {
    let _ = gh
        .post_issue_comment(
            repo,
            pr_number,
            lang.pick(
                "🔄 Reviewing (exploring the project + incremental diff), one moment…",
                "🔄 正在审查(探索项目 + 增量对比),稍候…",
            ),
        )
        .await;

    let outcome = run_agent(gh, cfg, repo, pr_number, lang).await;

    match outcome {
        Ok(o) => {
            if let Some(verdict) = o.verdict {
                let engine = format!("agent ({} turns)", o.turns_used);
                let summary = render_summary(&verdict, &engine, lang);
                let full_diff = gh.get_pr_diff(repo, pr_number).await.unwrap_or_default();
                let added = parse_added_lines(&truncate(&full_diff, cfg.max_diff_chars).0);
                let inline = build_inline_comments(&verdict, &added);
                // The mode is the status: "ok" hides that the inline comments
                // were dropped or that this went out as a plain comment.
                match gh.post_review(repo, pr_number, &summary, inline).await {
                    Ok(mode) => mode.to_string(),
                    Err(e) => {
                        let _ = gh
                            .post_issue_comment(
                                repo,
                                pr_number,
                                &t!(
                                    lang,
                                    "## 🤖 AI Code Review\n\n❌ Failed to publish: `{e}`",
                                    "## 🤖 AI Code Review\n\n❌ 发布失败: `{e}`"
                                ),
                            )
                            .await;
                        format!("error: {e}")
                    }
                }
            } else if o.timed_out {
                // timed out: fall back to builtin
                let note = lang.pick(
                    "⚠️ The agent's exploration timed out; falling back to the basic review.",
                    "⚠️ agent 探索超时,回退到基础审查。",
                );
                let _ = gh.post_issue_comment(repo, pr_number, note).await;
                crate::review::run_builtin(gh, cfg, repo, pr_number, lang).await
            } else {
                let note = lang.pick(
                    "⚠️ The agent submitted no review; falling back to the basic review.",
                    "⚠️ agent 未提交审查结果,回退到基础审查。",
                );
                let _ = gh.post_issue_comment(repo, pr_number, note).await;
                crate::review::run_builtin(gh, cfg, repo, pr_number, lang).await
            }
        }
        Err(e) => {
            let note = t!(
                lang,
                "⚠️ The agent failed (`{e}`); falling back to the basic review.",
                "⚠️ agent 出错(`{e}`),回退到基础审查。"
            );
            let _ = gh.post_issue_comment(repo, pr_number, &note).await;
            crate::review::run_builtin(gh, cfg, repo, pr_number, lang).await
        }
    }
}

// silence unused import warning for GhError in some cfg combinations
#[allow(unused)]
fn _gh_err_bound(e: &GhError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Lang;

    /// Where each protocol expects a tool's name and schema to be. A tool the
    /// provider can't parse is dropped silently — the model just never calls
    /// it — so the shape is worth asserting per protocol.
    const SHAPES: [(Proto, &str, &str); 3] = [
        (Proto::Chat, "/function/name", "/function/parameters"),
        (Proto::Responses, "/name", "/parameters"),
        (Proto::Anthropic, "/name", "/input_schema"),
    ];

    #[test]
    fn every_protocol_gets_the_same_tools() {
        for (proto, name_at, schema_at) in SHAPES {
            let tools = tool_definitions(proto);
            let arr = tools.as_array().unwrap();
            assert_eq!(arr.len(), tool_specs().len(), "{proto:?}");
            let names: Vec<&str> = arr
                .iter()
                .map(|t| {
                    assert!(t.pointer(schema_at).is_some(), "{proto:?}: {t}");
                    t.pointer(name_at).and_then(|n| n.as_str()).unwrap_or("")
                })
                .collect();
            // The system prompt names these; a tool the model can't see is a
            // loop that runs out of turns without ever submitting.
            for tool in ["list_files", "read_file", "search_code", "submit_review"] {
                assert!(names.contains(&tool), "{proto:?} missing {tool}");
            }
        }
    }

    /// Strict mode demands `additionalProperties: false` throughout, which
    /// these schemas don't set — leaving `strict` unset there is a 400 on
    /// every request.
    #[test]
    fn the_responses_api_gets_strict_turned_off() {
        for t in tool_definitions(Proto::Responses).as_array().unwrap() {
            assert_eq!(t.get("strict"), Some(&json!(false)), "{t}");
        }
    }

    #[test]
    fn the_system_prompt_goes_where_each_protocol_looks_for_it() {
        let mut cfg = Config::from_env();
        cfg.ai_model = "m".into();

        let chat = Conversation::new(Proto::Chat, "BRIEF", "diff").body(&cfg);
        assert_eq!(chat.pointer("/messages/0/role").unwrap(), "system");
        assert_eq!(chat.pointer("/messages/0/content").unwrap(), "BRIEF");

        let responses = Conversation::new(Proto::Responses, "BRIEF", "diff").body(&cfg);
        assert_eq!(responses.get("instructions").unwrap(), "BRIEF");
        // ...and not as a message, where it would be ignored
        assert_eq!(responses.pointer("/input/0/role").unwrap(), "user");

        let anthropic = Conversation::new(Proto::Anthropic, "BRIEF", "diff").body(&cfg);
        assert_eq!(anthropic.get("system").unwrap(), "BRIEF");
        assert_eq!(anthropic.pointer("/messages/0/role").unwrap(), "user");
        // Anthropic rejects a request with no token ceiling.
        assert!(anthropic.get("max_tokens").is_some());
    }

    #[test]
    fn a_tool_call_is_read_out_of_every_protocol() {
        let mut chat = Conversation::new(Proto::Chat, "s", "u");
        let turn = chat
            .read_turn(&json!({"choices": [{"message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "c1", "type": "function", "function": {
                    "name": "read_file", "arguments": "{\"path\": \"a.rs\"}"}}]
            }}]}))
            .unwrap();
        assert_eq!(turn.calls.len(), 1);
        assert_eq!(turn.calls[0].id, "c1");
        assert_eq!(turn.calls[0].name, "read_file");
        assert_eq!(turn.calls[0].args["path"], "a.rs");

        let mut responses = Conversation::new(Proto::Responses, "s", "u");
        let turn = responses
            .read_turn(&json!({"output": [
                {"type": "reasoning", "id": "r1", "summary": []},
                {"type": "function_call", "call_id": "c2", "name": "list_files",
                 "arguments": "{\"path\": \"\"}"},
            ]}))
            .unwrap();
        assert_eq!(turn.calls.len(), 1);
        assert_eq!(turn.calls[0].id, "c2", "the call_id, not the item id");
        assert_eq!(turn.calls[0].name, "list_files");
        // The reasoning item has to be echoed back too, next to its call.
        assert_eq!(responses.items.len(), 3);

        let mut anthropic = Conversation::new(Proto::Anthropic, "s", "u");
        let turn = anthropic
            .read_turn(&json!({"content": [
                {"type": "text", "text": "looking"},
                {"type": "tool_use", "id": "c3", "name": "search_code",
                 "input": {"query": "fn main"}},
            ]}))
            .unwrap();
        assert_eq!(turn.calls.len(), 1);
        assert_eq!(turn.calls[0].id, "c3");
        // An object here, not a JSON string like the other two.
        assert_eq!(turn.calls[0].args["query"], "fn main");
        assert_eq!(turn.text, "looking");
    }

    #[test]
    fn prose_with_no_tool_calls_reads_as_text_everywhere() {
        let verdict = "{\"summary\": \"fine\", \"findings\": []}";
        let cases = [
            (
                Proto::Chat,
                json!({"choices": [{"message": {"role": "assistant", "content": verdict}}]}),
            ),
            (
                Proto::Responses,
                json!({"output": [{"type": "message", "role": "assistant",
                                   "content": [{"type": "output_text", "text": verdict}]}]}),
            ),
            (
                Proto::Anthropic,
                json!({"content": [{"type": "text", "text": verdict}]}),
            ),
        ];
        for (proto, out) in cases {
            let turn = Conversation::new(proto, "s", "u").read_turn(&out).unwrap();
            assert!(turn.calls.is_empty(), "{proto:?}");
            assert_eq!(turn.text, verdict, "{proto:?}");
            // the loop's salvage path: prose that parses as a verdict is used
            assert!(parse_verdict(&turn.text).is_some(), "{proto:?}");
        }
    }

    fn two_results() -> Vec<(ToolCall, String)> {
        vec![
            (
                ToolCall {
                    id: "a".into(),
                    name: "read_file".into(),
                    args: json!({}),
                },
                "one".to_string(),
            ),
            (
                ToolCall {
                    id: "b".into(),
                    name: "list_files".into(),
                    args: json!({}),
                },
                "two".to_string(),
            ),
        ]
    }

    /// Anthropic wants every `tool_result` of a turn in a single user message.
    /// Two messages — the natural loop-shaped thing to write — is a 400.
    #[test]
    fn anthropic_batches_tool_results_into_one_message() {
        let mut convo = Conversation::new(Proto::Anthropic, "s", "u");
        convo.push_tool_results(&two_results());
        assert_eq!(convo.items.len(), 2, "{:?}", convo.items);
        let blocks = convo.items[1]
            .pointer("/content")
            .and_then(|c| c.as_array())
            .unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
    }

    #[test]
    fn the_openai_protocols_answer_each_call_separately() {
        let mut chat = Conversation::new(Proto::Chat, "s", "u");
        chat.push_tool_results(&two_results());
        assert_eq!(chat.items.len(), 4);
        assert_eq!(chat.items[2]["role"], "tool");
        assert_eq!(chat.items[2]["tool_call_id"], "a");
        assert_eq!(chat.items[2]["content"], "one");

        let mut responses = Conversation::new(Proto::Responses, "s", "u");
        responses.push_tool_results(&two_results());
        assert_eq!(responses.items.len(), 3);
        assert_eq!(responses.items[1]["type"], "function_call_output");
        assert_eq!(responses.items[1]["call_id"], "a");
        assert_eq!(responses.items[1]["output"], "one");
    }

    #[test]
    fn malformed_tool_arguments_do_not_end_the_review() {
        assert_eq!(parse_args("{\"path\": \"a.rs\"}")["path"], "a.rs");
        assert_eq!(parse_args("not json"), json!({}));
        assert_eq!(parse_args(""), json!({}));
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(urlencode("foo in:file"), "foo%20in:file");
    }

    #[test]
    fn test_agent_system_prompt_has_submit() {
        for lang in [Lang::En, Lang::Zh] {
            let p = agent_system_prompt(lang);
            assert!(p.contains("submit_review"), "{lang:?}");
            assert!(p.contains("severity"), "{lang:?}");
            // the tool names the model is told it has must be the ones it has
            for tool in ["list_files", "read_file", "search_code"] {
                assert!(p.contains(tool), "{lang:?} prompt missing {tool}");
            }
        }
        assert!(
            !agent_system_prompt(Lang::En)
                .chars()
                .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "{}",
            agent_system_prompt(Lang::En)
        );
    }
}
