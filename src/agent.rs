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
    build_inline_comments, parse_added_lines, parse_verdict, render_summary, truncate,
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

/// Tool definitions sent to the model (OpenAI chat tool-calling format).
fn tool_definitions() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List files and subdirectories at a path in the repository (empty path = root).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Directory path, \"\" for root"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read one file's text content from the repository.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_code",
                "description": "Search code in this repository (GitHub code search syntax).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"}
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "submit_review",
                "description": "Submit the final review verdict and finish.",
                "parameters": {
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
                }
            }
        }
    ])
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

    // message history
    let mut messages: Vec<Value> = vec![
        json!({"role": "system", "content": agent_system_prompt(lang)}),
        json!({"role": "user", "content": user_prompt}),
    ];

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/chat/completions", cfg.ai_base_url.trim_end_matches('/'));
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
            .bearer_auth(&cfg.ai_api_key)
            .json(&json!({
                "model": cfg.ai_model,
                "messages": messages,
                "tools": tool_definitions(),
                "tool_choice": "auto",
                "temperature": 0.2,
            }))
            .send()
            .await
            .map_err(|e| format!("agent step failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "agent step HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            ));
        }
        let out: Value =
            serde_json::from_str(&text).map_err(|e| format!("agent step bad JSON: {e}"))?;
        let choice = out.pointer("/choices/0").ok_or("agent step: no choice")?;
        let message = choice
            .get("message")
            .cloned()
            .ok_or("agent step: no message")?;

        // check for submit_review tool call
        if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
            // append the assistant message with tool calls
            messages.push(message.clone());
            let mut submitted: Option<Value> = None;
            for call in calls {
                let name = call
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let raw_args = call
                    .pointer("/function/arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let args: Value = serde_json::from_str(raw_args).unwrap_or(json!({}));
                let call_id = call
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();

                if name == "submit_review" {
                    let verdict = args.get("verdict").cloned().unwrap_or(Value::Null);
                    if verdict.is_object() {
                        submitted = Some(verdict);
                    }
                    // acknowledge the tool call so the history stays valid
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": "review submitted",
                    }));
                } else {
                    let result = execute_tool(gh, repo, &base_ref, name, &args).await;
                    // keep tool results bounded
                    let bounded: String = result.chars().take(8000).collect();
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": bounded,
                    }));
                }
            }
            if let Some(verdict) = submitted {
                return Ok(AgentOutcome {
                    verdict: Some(verdict),
                    turns_used,
                    timed_out: false,
                });
            }
            continue;
        }

        // no tool calls: model produced plain text — treat as final attempt
        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if let Some(verdict) = parse_verdict(content) {
            if verdict.get("findings").is_some() {
                return Ok(AgentOutcome {
                    verdict: Some(verdict),
                    turns_used,
                    timed_out: false,
                });
            }
        }
        // nudge the model to use submit_review
        messages.push(message);
        messages.push(json!({
            "role": "user",
            "content": lang.pick(
                "Call submit_review with your final result (the `verdict` matching the schema).",
                "请调用 submit_review 提交最终审查结果(verdict 按 schema)。",
            )
        }));
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
                if let Err(e) = gh.post_review(repo, pr_number, &summary, inline).await {
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
                    return format!("error: {e}");
                }
                "ok".into()
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

    #[test]
    fn test_tool_definitions_valid() {
        let tools = tool_definitions();
        let arr = tools.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        for t in arr {
            assert!(t.pointer("/function/name").is_some());
            assert!(t.pointer("/function/parameters").is_some());
        }
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
