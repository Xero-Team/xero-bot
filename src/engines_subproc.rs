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
use crate::review::{
    build_inline_comments, parse_added_lines, parse_verdict, render_summary, truncate,
};

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
        .map(|p| format!("\n## 上一轮审查意见(核对是否已修复,避免重复):\n{p}\n"))
        .unwrap_or_default();
    let commits = new_commits
        .map(|c| format!("\n## 自上一轮审查以来的新提交:\n{c}\n"))
        .unwrap_or_default();
    format!(
        "请审查当前仓库中 PR #{pr_number} 的改动({repo})。\n\
PR 标题: {title}\nPR 描述: {body}{prev}{commits}\n\
改动内容: 本仓库工作区已检出该 PR 的最新提交,基准分支为 origin/{base}。请用 `git diff origin/main...HEAD` 或读取文件来查看改动。\n\n\
要求:\n\
1. 先快速了解项目结构(根目录、构建配置、相关模块),再审查改动。\n\
2. 只报告真实问题,按风险分级。\n\
3. 最终输出必须是严格的 JSON(无解释文字、无 markdown 围栏),schema:\n\
{{\"summary\": \"一句话总体评价(中文)\", \"findings\": [{{\"severity\": \"critical|high|medium|low|info\", \"title\": \"简短标题\", \"file\": \"文件路径\", \"line\": 行号, \"description\": \"描述(中文)\", \"suggestion\": \"修复建议(中文)\"}}]}}\n\
4. 若无问题, findings 为空数组。只输出 JSON。",
        base = meta
            .pointer("/base/ref")
            .and_then(|r| r.as_str())
            .unwrap_or("main")
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
) -> String {
    let Some(verdict) = parse_verdict(raw_output) else {
        let body = format!(
            "## 🤖 AI Code Review ({engine_tag})\n\n⚠️ 未能解析模型返回的 JSON,以下为原始输出(截断):\n\n```\n{}\n```",
            raw_output.chars().take(4000).collect::<String>()
        );
        let _ = gh.post_issue_comment(repo, pr_number, &body).await;
        return "parse-failed".into();
    };
    let summary = render_summary(&verdict, engine_tag);
    let diff = gh.get_pr_diff(repo, pr_number).await.unwrap_or_default();
    let added = parse_added_lines(&truncate(&diff, cfg.max_diff_chars).0);
    let inline = build_inline_comments(&verdict, &added);
    if let Err(e) = gh.post_review(repo, pr_number, &summary, inline).await {
        let _ = gh
            .post_issue_comment(
                repo,
                pr_number,
                &format!("## 🤖 AI Code Review\n\n❌ 发布失败: `{e}`"),
            )
            .await;
        return format!("error: {e}");
    }
    "ok".into()
}

// ---------------------------------------------------------------------------
// pi engine
// ---------------------------------------------------------------------------

pub async fn run_pi(gh: &Client, cfg: &Config, repo: &str, pr_number: i64, token: &str) -> String {
    let _ = gh
        .post_issue_comment(
            repo,
            pr_number,
            "🔄 正在审查(pi 引擎,项目记忆 + 增量对比),稍候…",
        )
        .await;

    if let Err(e) = run_pi_inner(gh, cfg, repo, pr_number, token).await {
        let _ = gh
            .post_issue_comment(
                repo,
                pr_number,
                &format!("## 🤖 AI Code Review (pi)\n\n❌ 审查出错: `{e}`"),
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
    let status = post_subproc_result(gh, cfg, repo, pr_number, &stdout, "pi").await;
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
) -> String {
    let _ = gh
        .post_issue_comment(repo, pr_number, "🔄 正在审查(codex 引擎),稍候…")
        .await;

    if let Err(e) = run_codex_inner(gh, cfg, repo, pr_number, token).await {
        let _ = gh
            .post_issue_comment(
                repo,
                pr_number,
                &format!("## 🤖 AI Code Review (codex)\n\n❌ 审查出错: `{e}`"),
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
    let status = post_subproc_result(gh, cfg, repo, pr_number, &last, "codex").await;
    if status != "ok" {
        return Err(status);
    }
    Ok(())
}
