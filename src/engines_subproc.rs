//! Subprocess review engines: `pi` and `codex` CLIs (self-hosted / Docker only).
//!
//! Both need a local checkout, so we maintain a per-repo cache under
//! `XERO_DATA_DIR/repos/{repo}` (shallow clone + fetch of the PR ref).
//! pi keeps per-repo sessions under `XERO_DATA_DIR/sessions/{repo}` — that is
//! the incremental "project understanding" memory across reviews.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;

use crate::config::Config;
use crate::github::Client;
use crate::lang::Lang;
use crate::review::{
    build_inline_comments, parse_added_lines, parse_verdict, render_summary, truncate,
};
use crate::t;

/// Where the repo checkout lives for a given repository full name.
fn repo_dir(cfg: &Config, repo: &str) -> PathBuf {
    Path::new(&cfg.data_dir)
        .join("repos")
        .join(repo.replace('/', "__"))
}

fn sessions_dir(cfg: &Config, repo: &str) -> PathBuf {
    Path::new(&cfg.data_dir)
        .join("sessions")
        .join(repo.replace('/', "__"))
}

/// Does a subprocess engine look available? (binary on PATH)
pub async fn engine_available(binary: &str) -> bool {
    tokio::process::Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Clone-or-fetch the repo at `ref` into the cache dir. `token` is an
/// installation token used for the clone URL.
pub async fn ensure_checkout(
    cfg: &Config,
    repo: &str,
    pr_ref: &str,
    token: &str,
) -> Result<PathBuf, String> {
    let dir = repo_dir(cfg, repo);
    let parent = dir
        .parent()
        .ok_or_else(|| "bad repo dir".to_string())?
        .to_path_buf();
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|e| e.to_string())?;

    let authed_url = format!("https://x-access-token:{token}@github.com/{repo}.git");

    if !dir.join(".git").exists() {
        // fresh shallow clone of the base repo
        let out = tokio::process::Command::new("git")
            .args(["clone", "--depth=50", "--no-single-branch", &authed_url])
            .arg(&dir)
            .output()
            .await
            .map_err(|e| format!("git clone spawn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    } else {
        // existing cache: fetch the PR ref
        let out = tokio::process::Command::new("git")
            .args(["fetch", "--depth=50", "origin", pr_ref])
            .current_dir(&dir)
            .output()
            .await
            .map_err(|e| format!("git fetch spawn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git fetch failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }

    // check out the PR ref
    let out = tokio::process::Command::new("git")
        .args(["checkout", "--force", "FETCH_HEAD"])
        .current_dir(&dir)
        .output()
        .await
        .map_err(|e| format!("git checkout spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(dir)
}

fn build_review_prompt(
    repo: &str,
    pr_number: i64,
    meta: &Value,
    previous_review: Option<&str>,
    new_commits: Option<&str>,
    lang: Lang,
) -> String {
    let title = meta.get("title").and_then(|t| t.as_str()).unwrap_or("");
    let body: String = meta
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .chars()
        .take(2000)
        .collect();
    let prev = previous_review
        .map(|p| {
            t!(
                lang,
                "\n## Previous review (check whether these were fixed; don't repeat them):\n{p}\n",
                "\n## 上一轮审查意见(核对是否已修复,避免重复):\n{p}\n"
            )
        })
        .unwrap_or_default();
    let commits = new_commits
        .map(|c| {
            t!(
                lang,
                "\n## Commits pushed since the previous review:\n{c}\n",
                "\n## 自上一轮审查以来的新提交:\n{c}\n"
            )
        })
        .unwrap_or_default();
    let base = meta
        .pointer("/base/ref")
        .and_then(|r| r.as_str())
        .unwrap_or("main");
    t!(
        lang,
        "Review the changes in PR #{pr_number} of the repository checked out here ({repo}).\n\
PR title: {title}\nPR description: {body}{prev}{commits}\n\
The working tree is at this PR's head commit; the base branch is origin/{base}. Use \
`git diff origin/{base}...HEAD` or read the files to see the change.\n\n\
Requirements:\n\
1. Get a quick sense of the project first (root layout, build config, the modules involved), then review the change.\n\
2. Report only real problems, graded by risk.\n\
3. Your final output must be strict JSON (no explanatory prose, no markdown fence), schema:\n\
{{\"summary\": \"one-sentence overall assessment (in English)\", \"findings\": [{{\"severity\": \"critical|high|medium|low|info\", \"title\": \"short title\", \"file\": \"file path\", \"line\": line number, \"description\": \"description (in English)\", \"suggestion\": \"fix (in English)\"}}]}}\n\
4. If there is nothing to report, `findings` is an empty array. Output the JSON only.",
        "请审查当前仓库中 PR #{pr_number} 的改动({repo})。\n\
PR 标题: {title}\nPR 描述: {body}{prev}{commits}\n\
改动内容: 本仓库工作区已检出该 PR 的最新提交,基准分支为 origin/{base}。请用 `git diff origin/{base}...HEAD` 或读取文件来查看改动。\n\n\
要求:\n\
1. 先快速了解项目结构(根目录、构建配置、相关模块),再审查改动。\n\
2. 只报告真实问题,按风险分级。\n\
3. 最终输出必须是严格的 JSON(无解释文字、无 markdown 围栏),schema:\n\
{{\"summary\": \"一句话总体评价(中文)\", \"findings\": [{{\"severity\": \"critical|high|medium|low|info\", \"title\": \"简短标题\", \"file\": \"文件路径\", \"line\": 行号, \"description\": \"描述(中文)\", \"suggestion\": \"修复建议(中文)\"}}]}}\n\
4. 若无问题, findings 为空数组。只输出 JSON。"
    )
}

/// Shared tail for subprocess engines: parse stdout as verdict, post review.
async fn post_subproc_result(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    raw_output: &str,
    engine_tag: &str,
    lang: Lang,
) -> String {
    let Some(verdict) = parse_verdict(raw_output) else {
        let raw = raw_output.chars().take(4000).collect::<String>();
        let body = t!(
            lang,
            "## 🤖 AI Code Review ({engine_tag})\n\n⚠️ Couldn't parse the model's JSON; raw output below (truncated):\n\n```\n{raw}\n```",
            "## 🤖 AI Code Review ({engine_tag})\n\n⚠️ 未能解析模型返回的 JSON,以下为原始输出(截断):\n\n```\n{raw}\n```"
        );
        let _ = gh.post_issue_comment(repo, pr_number, &body).await;
        return "parse-failed".into();
    };
    let summary = render_summary(&verdict, engine_tag, lang);
    let diff = gh.get_pr_diff(repo, pr_number).await.unwrap_or_default();
    let added = parse_added_lines(&truncate(&diff, cfg.max_diff_chars).0);
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
}

// ---------------------------------------------------------------------------
// pi engine
// ---------------------------------------------------------------------------

pub async fn run_pi(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    token: &str,
    lang: Lang,
) -> String {
    let _ = gh
        .post_issue_comment(
            repo,
            pr_number,
            lang.pick(
                "🔄 Reviewing (pi engine, project memory + incremental diff), one moment…",
                "🔄 正在审查(pi 引擎,项目记忆 + 增量对比),稍候…",
            ),
        )
        .await;

    if let Err(e) = run_pi_inner(gh, cfg, repo, pr_number, token, lang).await {
        let _ = gh
            .post_issue_comment(
                repo,
                pr_number,
                &t!(
                    lang,
                    "## 🤖 AI Code Review (pi)\n\n❌ Review failed: `{e}`",
                    "## 🤖 AI Code Review (pi)\n\n❌ 审查出错: `{e}`"
                ),
            )
            .await;
        return format!("error: {e}");
    }
    "ok".into()
}

async fn run_pi_inner(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    token: &str,
    lang: Lang,
) -> Result<(), String> {
    let meta = gh
        .get_pr(repo, pr_number)
        .await
        .map_err(|e| e.to_string())?;
    let head_sha = meta
        .pointer("/head/sha")
        .and_then(|s| s.as_str())
        .ok_or("no head sha")?;
    let pr_ref = format!("pull/{pr_number}/head:{head_sha}");

    let dir = ensure_checkout(cfg, repo, &pr_ref, token).await?;

    let (previous_review, new_commits) =
        crate::review::fetch_incremental_context(gh, repo, pr_number, cfg.max_diff_chars).await;
    let prompt = build_review_prompt(
        repo,
        pr_number,
        &meta,
        previous_review.as_deref(),
        new_commits.as_deref(),
        lang,
    );

    let sessions = sessions_dir(cfg, repo);
    tokio::fs::create_dir_all(&sessions)
        .await
        .map_err(|e| e.to_string())?;

    let mut cmd = tokio::process::Command::new(&cfg.pi_path);
    cmd.arg("-p") // print mode: non-interactive
        .arg("--session-dir")
        .arg(&sessions)
        .arg("--tools")
        .arg("read,grep,find,ls,bash") // read + git diff via bash
        .arg("--no-extensions")
        .arg("--no-skills")
        .arg("-nc") // no context files (AGENTS.md etc.)
        .arg(&prompt)
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !cfg.pi_args.is_empty() {
        for a in cfg.pi_args.split_whitespace() {
            cmd.arg(a);
        }
    }

    let out = cmd.output().await.map_err(|e| format!("pi spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pi exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let status = post_subproc_result(gh, cfg, repo, pr_number, &stdout, "pi", lang).await;
    if status != "ok" {
        return Err(status);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// codex engine
// ---------------------------------------------------------------------------

pub async fn run_codex(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    token: &str,
    lang: Lang,
) -> String {
    let _ = gh
        .post_issue_comment(
            repo,
            pr_number,
            lang.pick(
                "🔄 Reviewing (codex engine), one moment…",
                "🔄 正在审查(codex 引擎),稍候…",
            ),
        )
        .await;

    if let Err(e) = run_codex_inner(gh, cfg, repo, pr_number, token, lang).await {
        let _ = gh
            .post_issue_comment(
                repo,
                pr_number,
                &t!(
                    lang,
                    "## 🤖 AI Code Review (codex)\n\n❌ Review failed: `{e}`",
                    "## 🤖 AI Code Review (codex)\n\n❌ 审查出错: `{e}`"
                ),
            )
            .await;
        return format!("error: {e}");
    }
    "ok".into()
}

async fn run_codex_inner(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    token: &str,
    lang: Lang,
) -> Result<(), String> {
    let meta = gh
        .get_pr(repo, pr_number)
        .await
        .map_err(|e| e.to_string())?;
    let head_sha = meta
        .pointer("/head/sha")
        .and_then(|s| s.as_str())
        .ok_or("no head sha")?;
    let pr_ref = format!("pull/{pr_number}/head:{head_sha}");

    let dir = ensure_checkout(cfg, repo, &pr_ref, token).await?;

    let (previous_review, new_commits) =
        crate::review::fetch_incremental_context(gh, repo, pr_number, cfg.max_diff_chars).await;
    let prompt = build_review_prompt(
        repo,
        pr_number,
        &meta,
        previous_review.as_deref(),
        new_commits.as_deref(),
        lang,
    );

    let out_file = Path::new(&cfg.data_dir).join("codex-last-message.md");
    if let Some(parent) = out_file.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    let _ = tokio::fs::remove_file(&out_file).await;

    let mut cmd = tokio::process::Command::new(&cfg.codex_path);
    cmd.arg("exec")
        .arg("--sandbox")
        .arg("read-only")
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&dir)
        .arg("-o")
        .arg(&out_file)
        .arg(&prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !cfg.codex_args.is_empty() {
        for a in cfg.codex_args.split_whitespace() {
            cmd.arg(a);
        }
    }

    let out = cmd
        .output()
        .await
        .map_err(|e| format!("codex spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "codex exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    let last = tokio::fs::read_to_string(&out_file)
        .await
        .map_err(|e| format!("codex output file: {e}"))?;
    let status = post_subproc_result(gh, cfg, repo, pr_number, &last, "codex", lang).await;
    if status != "ok" {
        return Err(status);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Engine dispatch
// ---------------------------------------------------------------------------

/// Pick and run an engine. `token` is needed only for subprocess engines.
pub async fn run_review(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    token: &str,
    lang: Lang,
) -> String {
    let choice = cfg.review_engine.to_lowercase();
    match choice.as_str() {
        "builtin" => crate::review::run_builtin(gh, cfg, repo, pr_number, lang).await,
        "agent" => crate::agent::run_agent_review(gh, cfg, repo, pr_number, lang).await,
        "pi" => run_pi(gh, cfg, repo, pr_number, token, lang).await,
        "codex" => run_codex(gh, cfg, repo, pr_number, token, lang).await,
        _ => {
            // auto: pi → codex → agent → builtin
            if engine_available(&cfg.pi_path).await {
                run_pi(gh, cfg, repo, pr_number, token, lang).await
            } else if engine_available(&cfg.codex_path).await {
                run_codex(gh, cfg, repo, pr_number, token, lang).await
            } else if cfg.ai_ready() {
                crate::agent::run_agent_review(gh, cfg, repo, pr_number, lang).await
            } else {
                let _ = gh
                    .post_issue_comment(
                        repo,
                        pr_number,
                        lang.pick(
                            "⚠️ No AI engine is configured (AI_BASE_URL/AI_API_KEY/AI_MODEL are \
missing), and neither pi nor codex is available.",
                            "⚠️ 未配置 AI 引擎(缺 AI_BASE_URL/AI_API_KEY/AI_MODEL),也无法使用 pi/codex。",
                        ),
                    )
                    .await;
                "error: no engine".into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Lang;

    #[test]
    fn test_prompt_contains_schema() {
        let meta = serde_json::json!({
            "title": "Add feature",
            "body": "does things",
            "base": {"ref": "main"},
            "head": {"sha": "abc123"}
        });
        let p = build_review_prompt("o/r", 7, &meta, Some("上一轮: X"), Some("fix: y"), Lang::Zh);
        assert!(p.contains("PR #7"));
        assert!(p.contains("Add feature"));
        assert!(p.contains("上一轮: X"));
        assert!(p.contains("fix: y"));
        assert!(p.contains("findings"));

        let en = build_review_prompt("o/r", 7, &meta, Some("prev: X"), Some("fix: y"), Lang::En);
        assert!(en.contains("PR #7"), "{en}");
        assert!(en.contains("findings"), "{en}");
        assert!(en.contains("in English"), "{en}");
        assert!(
            !en.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "{en}"
        );
    }

    /// The base branch is read from the PR, not assumed to be `main` — a PR
    /// onto `develop` was being told to diff against a branch that isn't there.
    #[test]
    fn test_prompt_uses_the_real_base_branch() {
        let meta = serde_json::json!({
            "title": "t", "body": "b", "base": {"ref": "develop"}
        });
        for lang in [Lang::En, Lang::Zh] {
            let p = build_review_prompt("o/r", 1, &meta, None, None, lang);
            assert!(p.contains("origin/develop...HEAD"), "{p}");
            assert!(!p.contains("origin/main"), "{p}");
        }
    }

    #[test]
    fn test_repo_dir_paths() {
        let cfg = crate::config::Config {
            data_dir: "/data".into(),
            ..test_cfg()
        };
        assert_eq!(
            repo_dir(&cfg, "octocat/hello"),
            Path::new("/data/repos/octocat__hello")
        );
        assert_eq!(
            sessions_dir(&cfg, "octocat/hello"),
            Path::new("/data/sessions/octocat__hello")
        );
    }

    fn test_cfg() -> crate::config::Config {
        crate::config::Config::from_env()
    }
}
