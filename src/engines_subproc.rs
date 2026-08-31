//! Subprocess review engines: `pi` and `codex` CLIs (self-hosted / Docker only).
//!
//! Both need a local checkout, so we keep one **per pull request** under
//! `XERO_DATA_DIR/repos/{owner}__{repo}/pr-{n}`. Per PR and not per repository:
//! the tree is checked out at a PR's head, so two reviews of two PRs of the
//! same repository sharing one directory meant each could be reading the
//! other's code — a review of #10 written from #11's tree, with nothing in the
//! output to suggest it.
//!
//! pi keeps per-repository sessions under `XERO_DATA_DIR/sessions/{repo}` —
//! that is the incremental "project understanding" memory across reviews, and
//! it is deliberately shared between the PRs of one repository, which is why pi
//! (unlike codex) still needs a lock.
//!
//! The other theme here is the clone token. It is an installation token with
//! write access to the repository, it travels in the clone URL, and git echoes
//! the remote URL in most of its failure messages — which were being posted to
//! the PR verbatim as `❌ Review failed: {e}`. Three layers now: it never
//! reaches `.git/config`, every `git` call goes through one helper that returns
//! only redacted errors, and anything on its way into a comment gets a second,
//! token-independent scrub.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::github::Client;
use crate::lang::Lang;
use crate::review::{
    build_inline_comments, parse_added_lines, parse_verdict, render_summary, truncate,
};
use crate::t;

/// Where the checkout for one pull request lives.
fn repo_dir(cfg: &Config, repo: &str, pr_number: i64) -> PathBuf {
    Path::new(&cfg.data_dir)
        .join("repos")
        .join(repo.replace('/', "__"))
        .join(format!("pr-{pr_number}"))
}

fn sessions_dir(cfg: &Config, repo: &str) -> PathBuf {
    Path::new(&cfg.data_dir)
        .join("sessions")
        .join(repo.replace('/', "__"))
}

/// Where codex is told to write its final message.
///
/// Keyed by PR *and* head sha, because the previous single global
/// `codex-last-message.md` was read back by whichever review finished first: a
/// concurrent run would publish the other PR's verdict, and a re-review of the
/// same PR could publish the previous commit's.
fn codex_out_file(cfg: &Config, repo: &str, pr_number: i64, head_sha: &str) -> PathBuf {
    let sha: String = head_sha.chars().take(12).collect();
    Path::new(&cfg.data_dir).join("codex").join(format!(
        "{}-pr{pr_number}-{sha}.md",
        repo.replace('/', "__")
    ))
}

/// The ref we fetch a PR head into. Under `refs/xero/` so it cannot collide
/// with a branch or tag of the repository being reviewed.
fn pr_ref(pr_number: i64) -> String {
    format!("refs/xero/pr/{pr_number}")
}

// ---------------------------------------------------------------------------
// Token redaction
// ---------------------------------------------------------------------------

/// Replace an installation token wherever it appears in `s`.
///
/// The length guard is the whole reason this is a function and not a `.replace`
/// at each call site: `str::replace` with an empty needle inserts the
/// replacement *between every character*, so an unset token — the exact case
/// where something has already gone wrong and the error matters most — would
/// turn the message into a wall of `***`. Real installation tokens are around
/// forty characters, so the guard never skips a live one.
pub fn redact(s: &str, token: &str) -> String {
    if token.len() < 8 {
        return s.to_string();
    }
    s.replace(token, "***")
}

/// Scrub credentials by shape, for text on its way into a comment or a log.
///
/// Complements [`redact`] rather than repeating it: this one needs no secret,
/// so it still works at a boundary that no longer holds the token, and it
/// catches a credential that arrived by a route we did not anticipate — a
/// submodule URL, a credential helper's own diagnostics, a model quoting
/// `git remote -v` back at us.
pub fn redact_any(s: &str) -> String {
    static URL_CREDS: OnceLock<Regex> = OnceLock::new();
    static GH_TOKEN: OnceLock<Regex> = OnceLock::new();
    // `https://x-access-token:ghs_…@github.com/o/r.git` -> `https://***@github.com/o/r.git`
    let url = URL_CREDS.get_or_init(|| Regex::new(r"://[^/@\s]*:[^@\s]*@").unwrap());
    // A bare token, with no URL around it.
    let tok = GH_TOKEN.get_or_init(|| Regex::new(r"gh[pousr]_[A-Za-z0-9]{16,}").unwrap());
    let s = url.replace_all(s, "://***@");
    tok.replace_all(&s, "***").into_owned()
}

/// Keep an error short enough to read. Cuts on a character boundary.
fn clamp(s: &str) -> String {
    truncate(s, 600).0
}

// ---------------------------------------------------------------------------
// Serialization between concurrent reviews
// ---------------------------------------------------------------------------

/// Named mutexes, created on demand.
///
/// Only pi uses one, and per *repository*: its `--session-dir` is per
/// repository by design (that is the cross-review project memory), so two pi
/// processes reviewing two PRs of one repository would be writing the same
/// session files. codex needs no lock — its working tree and its output file
/// are both per PR, and a second review of the *same* PR is refused by
/// [`InFlight`] before it gets this far.
static REVIEW_LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();

fn lock_for(key: &str) -> Arc<AsyncMutex<()>> {
    let locks = REVIEW_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    // A poisoned lock here would mean a panic while inserting into a HashMap;
    // the map is still usable and taking the process down over it would be a
    // worse outcome than the panic we already logged.
    let mut map = locks.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Marks one PR as being reviewed, for as long as this value is alive.
///
/// A duplicate `@bot review` is not work to be queued, it is the same work
/// again: waiting on a lock would run the model a second time and bill for it.
/// So the second request is turned away with an explanation instead.
struct InFlight(String);

impl InFlight {
    /// `None` if a review of this PR is already running in this process.
    fn claim(key: String) -> Option<InFlight> {
        let set = IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
        let mut guard = set.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(key.clone()).then(|| InFlight(key))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(set) = IN_FLIGHT.get() {
            set.lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.0);
        }
    }
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

/// The installation token these engines clone with.
///
/// Fetched here rather than by the caller because only the subprocess engines
/// need one: `builtin` and `agent` were paying for a token they never used. And
/// the failure has to be an error, not `unwrap_or_default()` — an empty token
/// goes into the clone URL and comes back as an authentication failure with no
/// indication that the token was what went missing.
async fn installation_token(
    gh: &Client,
    cfg: &Config,
    installation_id: i64,
) -> Result<String, String> {
    gh.installation_token(cfg, installation_id)
        .await
        .map_err(|e| format!("could not get an installation token: {e}"))
}

/// Run one `git` command. **The only way this module runs git.**
///
/// Not for convenience — the point is that a caller *cannot* reach the raw
/// stderr, so a future call site cannot reintroduce the leak by forgetting to
/// scrub it. Fixing the five places that formatted `out.stderr` directly would
/// have left the sixth to be written later; this makes forgetting impossible.
///
/// `args` may contain the authenticated URL. The error text quotes only the
/// subcommand, never the arguments, and puts git's own output through
/// [`redact`], because git names the remote it was talking to in most of its
/// failure messages.
async fn git(dir: Option<&Path>, args: &[&str], token: &str) -> Result<String, String> {
    let what = args.first().copied().unwrap_or("git");
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    // No credential prompt. Without this a rejected token makes git block on a
    // terminal that does not exist in a container, and the review hangs to its
    // timeout instead of failing with the authentication error it already has.
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = cmd
        .output()
        .await
        .map_err(|e| redact(&format!("git {what} could not start: {e}"), token))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(clamp(&redact(
            &format!("git {what} failed: {}", stderr.trim()),
            token,
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Where to fetch from, in two forms that must not be confused.
///
/// Separate fields rather than one URL and a rule about when to add
/// credentials: the distinction between "written to `.git/config`" and "passed
/// as an argument to one command" is the whole of layer one, and a single
/// string would leave it to each call site to remember.
struct Remote {
    /// Recorded as `origin`. Never contains credentials.
    clean: String,
    /// Passed to each `git fetch`. Never written anywhere.
    authed: String,
}

impl Remote {
    fn github(repo: &str, token: &str) -> Remote {
        Remote {
            clean: format!("https://github.com/{repo}.git"),
            authed: format!("https://x-access-token:{token}@github.com/{repo}.git"),
        }
    }

    /// A local repository, for testing the sequence without a network. `file://`
    /// rather than a bare path because a shallow fetch needs the smart
    /// transport, which the local one does not implement.
    #[cfg(test)]
    fn local(path: &Path) -> Remote {
        let url = format!(
            "file:///{}",
            path.to_string_lossy()
                .trim_start_matches('/')
                .replace('\\', "/")
        );
        Remote {
            clean: url.clone(),
            authed: url,
        }
    }
}

/// Put the PR's head, and its base branch, in a checkout of its own.
///
/// One unconditional sequence. The old code had two — a first-time branch that
/// cloned the repository's default branch and **never fetched the PR ref at
/// all**, and a warm-cache branch that did fetch it — so the first review of
/// any PR was a review of the default branch, and only a second run looked at
/// the change. That asymmetry is what "the first review fails at random" was.
///
/// `git init` on an existing repository is a no-op, so the cold and warm paths
/// are now literally the same commands.
pub async fn ensure_checkout(
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    base_ref: &str,
    token: &str,
) -> Result<PathBuf, String> {
    checkout(
        cfg,
        &Remote::github(repo, token),
        repo,
        pr_number,
        base_ref,
        token,
    )
    .await
}

async fn checkout(
    cfg: &Config,
    remote: &Remote,
    repo: &str,
    pr_number: i64,
    base_ref: &str,
    token: &str,
) -> Result<PathBuf, String> {
    let dir = repo_dir(cfg, repo, pr_number);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    git(Some(&dir), &["init", "--quiet"], token).await?;

    // The remote is recorded **without** credentials. `.git/config` is on disk
    // and readable by the review subprocess, an authenticated URL there would
    // outlive the token's hour, and it would defeat every attempt to keep the
    // token out of git's error messages. Authentication is passed per fetch
    // instead, as an argument that is never written anywhere.
    //
    // Not `-c http.extraHeader=…` either: the value of a `-c` flag is visible
    // in the process list to every other process on the host.
    if git(
        Some(&dir),
        &["remote", "set-url", "origin", &remote.clean],
        token,
    )
    .await
    .is_err()
    {
        git(
            Some(&dir),
            &["remote", "add", "origin", &remote.clean],
            token,
        )
        .await?;
    }

    let head_spec = format!("+refs/pull/{pr_number}/head:{}", pr_ref(pr_number));
    // The base branch has to be here too, and under `origin/` specifically: the
    // prompt tells the model to run `git diff origin/{base}...HEAD`, which
    // silently has no left side otherwise.
    let base_spec = format!("+refs/heads/{base_ref}:refs/remotes/origin/{base_ref}");
    let depth = format!("--depth={}", cfg.checkout_depth);
    let fetch = [
        "fetch",
        "--force",
        &depth,
        &remote.authed,
        &head_spec,
        &base_spec,
    ];
    git(Some(&dir), &fetch, token).await?;

    git(
        Some(&dir),
        &["checkout", "--force", "--detach", &pr_ref(pr_number)],
        token,
    )
    .await?;

    // A shallow fetch can land both tips without their common ancestor, and
    // then the three-dot diff the model was told to run covers the whole
    // branch. One deepen covers the PRs where that happens; beyond that the
    // checkout stops being cheap, so a failure here is logged and the review
    // goes ahead with what it has.
    let base_remote = format!("refs/remotes/origin/{base_ref}");
    if git(Some(&dir), &["merge-base", "HEAD", &base_remote], token)
        .await
        .is_err()
    {
        let deepen = [
            "fetch",
            "--force",
            "--deepen=200",
            &remote.authed,
            &head_spec,
            &base_spec,
        ];
        if let Err(e) = git(Some(&dir), &deepen, token).await {
            tracing::warn!("could not deepen {repo}#{pr_number} to reach a merge base: {e}");
        }
    }
    Ok(dir)
}

/// The PR's base branch. `main` is a last resort, not an assumption: the
/// checkout fetches this ref by name, and the prompt tells the model to diff
/// against it, so the two must agree on one answer.
fn base_ref(meta: &Value) -> String {
    meta.pointer("/base/ref")
        .and_then(|r| r.as_str())
        .unwrap_or("main")
        .to_string()
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
    let base = base_ref(meta);
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
        // Scrubbed as well: this is model output on its way into a public
        // comment, and the model can read the repository's git config.
        let raw = redact_any(&raw_output.chars().take(4000).collect::<String>());
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
}

// ---------------------------------------------------------------------------
// pi engine
// ---------------------------------------------------------------------------

pub async fn run_pi(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    installation_id: i64,
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

    if let Err(e) = run_pi_inner(gh, cfg, repo, pr_number, installation_id, lang).await {
        // Third layer: the errors from `git` are already redacted, but this is
        // the boundary where a string becomes public, and it is the one place
        // that has to hold regardless of what produced the string.
        let e = clamp(&redact_any(&e));
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
    installation_id: i64,
    lang: Lang,
) -> Result<(), String> {
    let meta = gh
        .get_pr(repo, pr_number)
        .await
        .map_err(|e| e.to_string())?;
    let base_ref = base_ref(&meta);

    // Held for the whole run, including the checkout: the session directory is
    // shared by every PR of this repository, and it is what pi reads to
    // remember the project between reviews.
    let lock = lock_for(&format!("pi:{repo}"));
    let _serialized = lock.lock().await;

    let token = installation_token(gh, cfg, installation_id).await?;
    let dir = ensure_checkout(cfg, repo, pr_number, &base_ref, &token).await?;

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
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(clamp(&redact(
            &format!("pi exited {}: {}", out.status, stderr.trim()),
            &token,
        )));
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
    installation_id: i64,
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

    if let Err(e) = run_codex_inner(gh, cfg, repo, pr_number, installation_id, lang).await {
        let e = clamp(&redact_any(&e));
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
    installation_id: i64,
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
    let base_ref = base_ref(&meta);

    // No lock: the working tree and the output file below are both per PR, and
    // a second review of the same PR never reaches this function.
    let token = installation_token(gh, cfg, installation_id).await?;
    let dir = ensure_checkout(cfg, repo, pr_number, &base_ref, &token).await?;

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

    let out_file = codex_out_file(cfg, repo, pr_number, head_sha);
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
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(clamp(&redact(
            &format!("codex exited {}: {}", out.status, stderr.trim()),
            &token,
        )));
    }
    let last = tokio::fs::read_to_string(&out_file)
        .await
        .map_err(|e| format!("codex output file: {e}"))?;
    // The verdict is published, so the file has served its purpose; leaving it
    // would accumulate one per commit of every PR ever reviewed.
    let _ = tokio::fs::remove_file(&out_file).await;
    let status = post_subproc_result(gh, cfg, repo, pr_number, &last, "codex", lang).await;
    if status != "ok" {
        return Err(status);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Engine dispatch
// ---------------------------------------------------------------------------

/// Pick and run an engine. `installation_id` is used only by the subprocess
/// engines, which exchange it for a clone token themselves.
///
/// One review per PR at a time. The guard sits here rather than inside the
/// subprocess engines so that it covers all four: a duplicate `@bot review`
/// costs a model run on `builtin` and `agent` too, and the second verdict
/// arrives after the first with no way for a reader to tell which is current.
pub async fn run_review(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    installation_id: i64,
    lang: Lang,
) -> String {
    let Some(_in_flight) = InFlight::claim(format!("{repo}#{pr_number}")) else {
        let _ = gh
            .post_issue_comment(
                repo,
                pr_number,
                lang.pick(
                    "⏳ A review of this PR is already running — waiting for that one \
rather than starting a second.",
                    "⏳ 本 PR 已有一轮审查正在进行 —— 等这一轮的结果即可,不再重复发起。",
                ),
            )
            .await;
        return "already-running".into();
    };

    let choice = cfg.review_engine.to_lowercase();
    match choice.as_str() {
        "builtin" => crate::review::run_builtin(gh, cfg, repo, pr_number, lang).await,
        "agent" => crate::agent::run_agent_review(gh, cfg, repo, pr_number, lang).await,
        "pi" => run_pi(gh, cfg, repo, pr_number, installation_id, lang).await,
        "codex" => run_codex(gh, cfg, repo, pr_number, installation_id, lang).await,
        _ => {
            // auto: pi → codex → agent → builtin
            if engine_available(&cfg.pi_path).await {
                run_pi(gh, cfg, repo, pr_number, installation_id, lang).await
            } else if engine_available(&cfg.codex_path).await {
                run_codex(gh, cfg, repo, pr_number, installation_id, lang).await
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

    /// One directory per PR. Sharing one per repository meant a review of #10
    /// could be reading a tree checked out at #11's head; the session directory
    /// stays per repository on purpose, because it is the project memory.
    #[test]
    fn test_repo_dir_paths() {
        let cfg = crate::config::Config {
            data_dir: "/data".into(),
            ..test_cfg()
        };
        assert_eq!(
            repo_dir(&cfg, "octocat/hello", 10),
            Path::new("/data/repos/octocat__hello/pr-10")
        );
        assert_ne!(
            repo_dir(&cfg, "octocat/hello", 10),
            repo_dir(&cfg, "octocat/hello", 11)
        );
        assert_eq!(
            sessions_dir(&cfg, "octocat/hello"),
            Path::new("/data/sessions/octocat__hello")
        );
    }

    /// Was a single global file, so whichever review finished first read
    /// whatever was there — including another PR's verdict.
    #[test]
    fn codex_output_is_per_pr_and_per_commit() {
        let cfg = crate::config::Config {
            data_dir: "/data".into(),
            ..test_cfg()
        };
        assert_eq!(
            codex_out_file(&cfg, "octocat/hello", 10, "abcdef0123456789"),
            Path::new("/data/codex/octocat__hello-pr10-abcdef012345.md")
        );
        assert_ne!(
            codex_out_file(&cfg, "octocat/hello", 10, "aaaaaaaaaaaa"),
            codex_out_file(&cfg, "octocat/hello", 11, "aaaaaaaaaaaa"),
        );
        // A re-review after a push must not read the previous commit's file.
        assert_ne!(
            codex_out_file(&cfg, "octocat/hello", 10, "aaaaaaaaaaaa"),
            codex_out_file(&cfg, "octocat/hello", 10, "bbbbbbbbbbbb"),
        );
    }

    // -----------------------------------------------------------------------
    // Redaction
    // -----------------------------------------------------------------------

    #[test]
    fn redact_removes_the_token_everywhere_it_appears() {
        let token = "ghs_abcdefghijklmnopqrstuvwxyz012345";
        // In the clone URL, which is what git quotes back on failure.
        let msg = format!(
            "fatal: unable to access 'https://x-access-token:{token}@github.com/o/r.git/': 403"
        );
        let out = redact(&msg, token);
        assert!(!out.contains(token), "{out}");
        assert!(
            out.contains("x-access-token:***@github.com/o/r.git"),
            "{out}"
        );
        // Bare, and more than once.
        let twice = format!("{token} then {token}");
        assert_eq!(redact(&twice, token), "*** then ***");
    }

    /// The guard that makes the helper safe to call unconditionally. An empty
    /// needle would have `str::replace` insert `***` between every character,
    /// so the message least likely to be understood — the one where the token
    /// went missing — would be the one destroyed.
    #[test]
    fn redact_leaves_everything_alone_for_an_empty_or_short_token() {
        let msg = "fatal: could not read Username for 'https://github.com'";
        assert_eq!(redact(msg, ""), msg);
        assert_eq!(redact(msg, "abc"), msg);
        assert_eq!(redact(msg, "1234567"), msg, "7 chars is still too short");
        // Eight is long enough to be deliberate.
        assert_eq!(redact("xx12345678xx", "12345678"), "xx***xx");
    }

    /// The boundary pass, which has no token to compare against.
    #[test]
    fn redact_any_scrubs_by_shape() {
        let out = redact_any(
            "remote: https://x-access-token:ghs_0123456789abcdefghij@github.com/o/r.git failed",
        );
        assert!(!out.contains("ghs_"), "{out}");
        assert!(out.contains("https://***@github.com/o/r.git"), "{out}");

        // A bare token with no URL around it.
        assert_eq!(
            redact_any("token ghp_0123456789abcdefghijklmn expired"),
            "token *** expired"
        );
        // And nothing to do on ordinary text, including a URL without credentials.
        let plain = "fatal: couldn't find remote ref refs/pull/1/head at https://github.com/o/r";
        assert_eq!(redact_any(plain), plain);
    }

    // -----------------------------------------------------------------------
    // One review per PR
    // -----------------------------------------------------------------------

    #[test]
    fn a_second_review_of_the_same_pr_is_refused_until_the_first_ends() {
        let first = InFlight::claim("o/r#1".to_string()).expect("first claim");
        assert!(
            InFlight::claim("o/r#1".to_string()).is_none(),
            "the same PR must not be claimed twice"
        );
        // A different PR is unrelated work.
        let other = InFlight::claim("o/r#2".to_string());
        assert!(other.is_some());
        drop(other);

        drop(first);
        assert!(
            InFlight::claim("o/r#1".to_string()).is_some(),
            "the claim must be released when the review ends"
        );
    }

    // -----------------------------------------------------------------------
    // The checkout sequence, against a real git
    // -----------------------------------------------------------------------

    /// Self-cleaning temp directory (no dev-dependency, matching
    /// `tests/key_config.rs`).
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir(tag: &str) -> TempDir {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("xero-{tag}-{unique}"));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn git_sync(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with a `main` branch and a `refs/pull/1/head` that is *not*
    /// on it — the shape GitHub serves for an open PR.
    fn upstream_with_a_pr(dir: &Path) -> (String, String) {
        git_sync(dir, &["init", "--quiet", "--initial-branch=main", "."]);
        std::fs::write(dir.join("base.txt"), "base\n").unwrap();
        git_sync(dir, &["add", "."]);
        git_sync(dir, &["commit", "--quiet", "-m", "base"]);
        let base_sha = git_sync(dir, &["rev-parse", "HEAD"]);

        git_sync(dir, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(dir.join("pr.txt"), "the change\n").unwrap();
        git_sync(dir, &["add", "."]);
        git_sync(dir, &["commit", "--quiet", "-m", "the change"]);
        let head_sha = git_sync(dir, &["rev-parse", "HEAD"]);
        // Publish it the way GitHub does, and put the branch back so a naive
        // clone of the default branch would *not* see the change.
        git_sync(dir, &["update-ref", "refs/pull/1/head", &head_sha]);
        git_sync(dir, &["checkout", "--quiet", "main"]);
        git_sync(dir, &["branch", "--quiet", "-D", "feature"]);
        (base_sha, head_sha)
    }

    /// The regression that made "the first review of a PR" a coin toss: the
    /// cold path cloned the default branch and never fetched the PR ref, so the
    /// review read the base, not the change. Asserted on the *first* call.
    #[tokio::test]
    async fn first_checkout_lands_on_the_pr_head() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("git not available; skipping");
            return;
        }

        let upstream = tempdir("upstream");
        let (base_sha, head_sha) = upstream_with_a_pr(&upstream.0);
        let data = tempdir("data");
        let cfg = crate::config::Config {
            data_dir: data.0.to_string_lossy().into_owned(),
            checkout_depth: 10,
            ..test_cfg()
        };

        let remote = Remote::local(&upstream.0);
        let dir = checkout(&cfg, &remote, "octocat/hello", 1, "main", "unused-token")
            .await
            .expect("first checkout");

        assert_eq!(dir, repo_dir(&cfg, "octocat/hello", 1));
        assert_eq!(git_sync(&dir, &["rev-parse", "HEAD"]), head_sha);
        assert!(dir.join("pr.txt").exists(), "the PR's file must be here");
        // The base branch came too, or `git diff origin/main...HEAD` — which is
        // what the prompt tells the model to run — has no left side.
        assert_eq!(
            git_sync(&dir, &["rev-parse", "refs/remotes/origin/main"]),
            base_sha
        );
        assert_eq!(
            git_sync(&dir, &["merge-base", "HEAD", "origin/main"]),
            base_sha
        );

        // Layer one: nothing on disk may carry a credential.
        let config = std::fs::read_to_string(dir.join(".git").join("config")).unwrap();
        assert!(
            !config.contains("x-access-token") && !config.contains('@'),
            ".git/config must not carry credentials:\n{config}"
        );

        // And the same sequence again is a no-op that still ends on the head —
        // one code path, so warm and cold agree.
        let again = checkout(&cfg, &remote, "octocat/hello", 1, "main", "unused-token")
            .await
            .expect("second checkout");
        assert_eq!(git_sync(&again, &["rev-parse", "HEAD"]), head_sha);
    }

    /// The production leak: a git failure was formatted straight into the error
    /// string and posted as `❌ review failed: {e}`, so an installation token
    /// went into a public comment.
    ///
    /// The token is put into a *ref name* rather than into the URL, on purpose.
    /// Recent git versions strip credentials from their own "unable to access"
    /// message, so a test written that way passes here and proves nothing about
    /// our code. Git does echo ref names verbatim, and the invariant we actually
    /// own is the wrapper's: **whatever** git writes goes through [`redact`]
    /// before a caller can see it. That is what this pins down.
    #[tokio::test]
    async fn a_failing_git_command_never_hands_back_the_token() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("git not available; skipping");
            return;
        }

        let token = "ghs_abcdefghijklmnopqrstuvwxyz012345";
        let dir = tempdir("leak");
        git(Some(&dir.0), &["init", "--quiet", "."], token)
            .await
            .unwrap();

        let upstream = tempdir("leak-upstream");
        git_sync(&upstream.0, &["init", "--quiet", "--bare", "."]);
        let remote = Remote::local(&upstream.0);
        let spec = format!("+refs/heads/{token}:refs/xero/leak");

        let err = git(
            Some(&dir.0),
            &["fetch", "--force", &remote.authed, &spec],
            token,
        )
        .await
        .expect_err("fetching a ref that does not exist must fail");

        assert!(
            !err.contains(token),
            "token survived into the error:\n{err}"
        );
        assert!(
            err.contains("***"),
            "the token's place should be marked:\n{err}"
        );
        // Still a usable diagnosis — redaction that ate the message would just
        // move the problem.
        assert!(err.contains("git fetch failed"), "{err}");
    }

    /// Two PRs of one repository must not share a tree.
    #[tokio::test]
    async fn two_prs_of_one_repo_get_separate_trees() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("git not available; skipping");
            return;
        }

        let upstream = tempdir("upstream2");
        let (_, head_sha) = upstream_with_a_pr(&upstream.0);
        // A second PR, one commit further along.
        git_sync(&upstream.0, &["checkout", "--quiet", "--detach", &head_sha]);
        std::fs::write(upstream.0.join("pr2.txt"), "second change\n").unwrap();
        git_sync(&upstream.0, &["add", "."]);
        git_sync(&upstream.0, &["commit", "--quiet", "-m", "second change"]);
        let head2 = git_sync(&upstream.0, &["rev-parse", "HEAD"]);
        git_sync(&upstream.0, &["update-ref", "refs/pull/2/head", &head2]);
        git_sync(&upstream.0, &["checkout", "--quiet", "main"]);

        let data = tempdir("data2");
        let cfg = crate::config::Config {
            data_dir: data.0.to_string_lossy().into_owned(),
            checkout_depth: 10,
            ..test_cfg()
        };
        let remote = Remote::local(&upstream.0);
        let one = checkout(&cfg, &remote, "octocat/hello", 1, "main", "t")
            .await
            .unwrap();
        let two = checkout(&cfg, &remote, "octocat/hello", 2, "main", "t")
            .await
            .unwrap();

        assert_ne!(one, two);
        assert_eq!(git_sync(&one, &["rev-parse", "HEAD"]), head_sha);
        assert_eq!(git_sync(&two, &["rev-parse", "HEAD"]), head2);
        assert!(!one.join("pr2.txt").exists(), "#1 must not see #2's change");
    }

    fn test_cfg() -> crate::config::Config {
        crate::config::Config::from_env()
    }
}
