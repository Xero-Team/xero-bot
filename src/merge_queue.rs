//! Merge queue: approved PRs are merged into a staging branch one
//! batch at a time, CI runs on the staging pushes, and a green batch advances
//! main. A red batch drops its tail member (the newest) and re-tests the
//! rest — tail-dropping is bisecting by construction.
//!
//! State lives entirely in GitHub — no database:
//!
//! | State | Carrier |
//! |---|---|
//! | queued | `merge queue: queued` label |
//! | batch member | `merge queue: testing` label |
//! | batch contents & order | the staging branch's merge-commit chain, each message `xero-bot: merge #n (head <sha>)` |
//! | main advance | the open staging→main PR carrying [`ADVANCE_MARKER`] in its body |
//!
//! Every step is idempotent and re-derivable from those carriers, so a
//! restart mid-batch costs nothing: the next driver tick rebuilds the picture
//! and resumes. octocrab retries are deliberately disabled (see
//! [`crate::github::client_builder`]) — the idempotency is here, not in the
//! HTTP layer.
//!
//! The driver never runs concurrently: one poll loop (plus the /cron
//! trigger) is the only writer of the staging branch. Two writers moving the
//! same ref would race, and the label dance is not atomic either.

use serde_json::Value;

use crate::config::Config;
use crate::github::{Client, GhError};
use crate::lang::Lang;
use crate::t;

/// Prefix of the message every staging merge commit carries. A commit whose
/// message starts with this belongs to the current batch; the first commit
/// without it is the batch's base (usually main's tip at batch start).
///
/// The head sha is recorded here because the PR list alone cannot answer
/// "was this member's head changed mid-batch?" — the standard merge queue
/// failure mode of an author force-pushing while their PR is under test.
pub const MARKER_PREFIX: &str = "xero-bot: merge #";

/// Hidden marker in the body of the PR that advances main (staging→main).
/// Recognizing our own advance PR by content, not by author, is the same
/// trick as [`crate::github::REVIEW_MARKER`] — a degraded or renamed bot
/// login must not orphan the batch.
pub const ADVANCE_MARKER: &str = "<!-- xero-bot-merge-queue-advance -->";

/// One staging merge commit: which PR, at which head sha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEntry {
    pub pr: i64,
    /// The PR head sha at merge time (from the marker, not the API).
    pub head_sha: String,
    /// The merge commit itself (the staging ref pointed here after this
    /// member was folded in) — the reset target when a later member fails.
    pub merge_commit_sha: String,
}

/// Parse the staging commit list (newest first, as `/commits` returns) into
/// the batch chain, oldest member first.
///
/// Walks until the first commit whose message lacks the marker; that commit
/// is the base the batch was built on and is not part of it. A malformed
/// marker line ends the walk rather than being skipped — a torn marker means
/// the chain cannot be trusted to line up with the labels, and treating the
/// prefix as trustworthy is exactly the guess this parser exists to refuse.
pub fn parse_chain(commits: &[Value]) -> Vec<ChainEntry> {
    let mut chain = Vec::new();
    for c in commits {
        let Some(message) = c.pointer("/commit/message").and_then(|m| m.as_str()) else {
            break;
        };
        // The sha of this commit itself (same for the API's "sha" field).
        let Some(sha) = c.get("sha").and_then(|s| s.as_str()) else {
            break;
        };
        if let Some(rest) = message.strip_prefix(MARKER_PREFIX) {
            // "xero-bot: merge #123 (head abc1234…)" — the parenthetical is
            // what enqueue wrote; tolerate its absence for commits written by
            // older versions or by hand.
            let pr = rest
                .split([' ', ')'])
                .next()
                .and_then(|n| n.parse::<i64>().ok());
            let head_sha = rest
                .split_once("(head ")
                .and_then(|(_, tail)| tail.split(')').next())
                .unwrap_or("")
                .to_string();
            match pr {
                Some(pr) => chain.push(ChainEntry {
                    pr,
                    head_sha,
                    merge_commit_sha: sha.to_string(),
                }),
                // A marker without a number can be matched to no PR — stop.
                None => break,
            }
        } else {
            break;
        }
    }
    chain.reverse();
    chain
}

/// The ref a merge request must name as its head: plain for same-repo
/// branches, `user:branch` for a fork.
///
/// `head.label` is exactly that GitHub-shaped string, but a fork owner whose
/// name contains a colon would corrupt it — `head.repo.full_name` is the
/// truth of where the branch lives.
pub fn head_ref_of(pr: &Value) -> String {
    let same_repo = pr.pointer("/head/repo/full_name");
    let branch = pr
        .pointer("/head/ref")
        .and_then(|r| r.as_str())
        .unwrap_or("");
    match same_repo {
        Some(_) => branch.to_string(),
        None => {
            // The fork's full_name is under head.repo only when the fork is
            // still reachable; a deleted fork cannot be merged anyway.
            let owner = pr
                .pointer("/head/label")
                .and_then(|l| l.as_str())
                .and_then(|l| l.split(':').next())
                .unwrap_or("");
            format!("{owner}:{branch}")
        }
    }
}

/// One PR payload from the issues search (`list_prs_with_label`), reduced to
/// what the queue needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub number: i64,
}

/// Pure decision: which member (by chain index) should be dropped, if any,
/// when the staging CI comes back `Failed`.
///
/// The tail member is the suspect — the batch was green-less with everyone
/// before them, so removing the newest unknown re-tests the known-good
/// prefix. None only when the batch is somehow empty; an empty chain with a
/// red CI means the failure is on the base itself, which is not the queue's
/// to fix.
pub fn culprit_of(chain: &[ChainEntry]) -> Option<usize> {
    if chain.is_empty() {
        None
    } else {
        Some(chain.len() - 1)
    }
}

// ---------------------------------------------------------------------------
// Webhook side: reviews and closes drive the queue
// ---------------------------------------------------------------------------

/// An entry point into the queue accepted a PR. `Already` covers the two
/// idempotent paths (label already present, or a stale enqueue racing the
/// driver); the caller logs rather than re-replies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued,
    Already,
    Refused(String),
}

impl std::fmt::Display for EnqueueOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnqueueOutcome::Queued => f.write_str("queued"),
            EnqueueOutcome::Already => f.write_str("already-queued"),
            EnqueueOutcome::Refused(why) => f.write_str(&format!("refused: {why}")),
        }
    }
}

/// Handle a human review that passed the routing guards.
///
/// APPROVED → enqueue (permission re-checked here, not trusted from routing:
/// routing and execution are decoupled by the work queue, and a reviewer
/// demoted between the two must not ride on the earlier verdict).
/// CHANGES_REQUESTED → dequeue; a write+ reviewer pulling their approval is
/// the same as `r-`.
pub async fn handle_review(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    state: &str,
    reviewer: &str,
) -> String {
    match state {
        "APPROVED" => {
            let permission = match gh.collaborator_permission(repo, reviewer).await {
                Ok(p) => p,
                Err(e) => return format!("error: permission check: {e}"),
            };
            if !matches!(permission.as_str(), "admin" | "maintain" | "write") {
                // Reply on the PR so the reviewer sees why nothing happened —
                // silence here reads as a swallowed approval.
                let lang = crate::lang::for_pr(gh, repo, pr_number, None).await;
                let body = t!(
                    lang,
                    "ℹ️ @{reviewer} approved, but the merge queue needs write access or above (currently: {permission}).",
                    "ℹ️ @{reviewer} 已批准,但合并队列需要 write 及以上权限(当前: {permission})。"
                );
                let _ = gh.post_issue_comment(repo, pr_number, &body).await;
                return "permission-denied".into();
            }
            let outcome = enqueue(gh, cfg, repo, pr_number).await;
            outcome_string(&outcome)
        }
        "CHANGES_REQUESTED" => {
            let permission = match gh.collaborator_permission(repo, reviewer).await {
                Ok(p) => p,
                Err(e) => return format!("error: permission check: {e}"),
            };
            if !matches!(permission.as_str(), "admin" | "maintain" | "write") {
                return "permission-denied".into();
            }
            let lang = crate::lang::for_pr(gh, repo, pr_number, None).await;
            let note = t!(
                lang,
                "➖ Removed from the merge queue: @{reviewer} requested changes.",
                "➖ 已移出合并队列:@{reviewer} 提出了修改要求。"
            );
            dequeue_pr(gh, cfg, repo, pr_number, &note).await
        }
        other => format!("unrouted state {other}"),
    }
}

/// `pull_request closed`: if the PR was queued or in the batch, take it out.
///
/// `merged` PRs skip the comment — the queue knows (or will learn) the PR is
/// gone, and a "removed from queue" note on a just-merged PR is noise.
pub async fn handle_pr_closed(gh: &Client, cfg: &Config, repo: &str, pr_number: i64) -> String {
    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    if pr.get("state").and_then(|s| s.as_str()) == Some("open") {
        // The webhook can deliver for an issue masquerading as a PR pull, or
        // race a reopen; only a genuinely closed PR dequeues.
        return "still-open".into();
    }
    let merged = pr.get("merged").and_then(|m| m.as_bool()).unwrap_or(false);
    if merged {
        // The batch's own health check discovers a merged member and resets
        // around it; see `pump_one`.
        return "merged".into();
    }
    let lang = crate::lang::for_pr(gh, repo, pr_number, None).await;
    dequeue_pr(
        gh,
        cfg,
        repo,
        pr_number,
        &t!(
            lang,
            "➖ Removed from the merge queue: the PR was closed.",
            "➖ 已移出合并队列:PR 已关闭。"
        ),
    )
    .await
}

/// Put a PR in the queue: validate, label, acknowledge.
///
/// Idempotent by label: a second enqueue on an already-queued PR answers
/// "already" instead of stacking a second label write. The base-branch check
/// is v1's shape limit — merging a PR aimed at another branch into `staging`
/// would fold the wrong diff into main.
pub async fn enqueue(gh: &Client, cfg: &Config, repo: &str, pr_number: i64) -> EnqueueOutcome {
    if !cfg.merge_queue_enabled {
        return EnqueueOutcome::Refused("merge queue disabled".into());
    }
    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(p) => p,
        Err(e) => {
            return EnqueueOutcome::Refused(format!("cannot read PR: {e}"));
        }
    };
    if pr.get("state").and_then(|s| s.as_str()) != Some("open") {
        return EnqueueOutcome::Refused("PR is not open".into());
    }
    let lang = crate::lang::for_pr(gh, repo, pr_number, None).await;

    // Target the default branch only. Comparing against the repo's
    // default_branch (not hardcoding "main") is what makes forks and renamed
    // defaults work.
    let base = pr
        .pointer("/base/ref")
        .and_then(|b| b.as_str())
        .unwrap_or("");
    let default_branch = match gh.repo_info(repo).await {
        Ok(r) => r
            .get("default_branch")
            .and_then(|b| b.as_str())
            .unwrap_or("main")
            .to_string(),
        Err(e) => return EnqueueOutcome::Refused(format!("cannot read repo: {e}")),
    };
    if base != default_branch {
        let (base_name, default_name) = (base, default_branch.as_str());
        let body = t!(
            lang,
            "⚠️ The merge queue only takes PRs targeting `{default_name}` (this one targets `{base_name}`).",
            "⚠️ 合并队列只接受目标是 `{default_name}` 的 PR(本 PR 的目标是 `{base_name}`)。"
        );
        let _ = gh.post_issue_comment(repo, pr_number, &body).await;
        return EnqueueOutcome::Refused(format!("base is {base}, not {default_branch}"));
    }

    // Already in the system? One label read answers for both states.
    let labels = match gh.list_labels(repo, pr_number).await {
        Ok(l) => l,
        Err(e) => return EnqueueOutcome::Refused(format!("cannot read labels: {e}")),
    };
    if labels.iter().any(|l| l == &cfg.label_merge_queue_testing) {
        return EnqueueOutcome::Already;
    }
    if labels.iter().any(|l| l == &cfg.label_merge_queue_queued) {
        return EnqueueOutcome::Already;
    }

    // GitHub computes mergeability lazily; `mergeable == Some(false)` is a
    // definite conflict, `None`/`Some(true)` are "proceed and let the driver
    // sort it out" — the conflict path re-reports with a clear message.
    if pr.get("mergeable").and_then(|m| m.as_bool()) == Some(false) {
        let default_name = default_branch.as_str();
        let body = t!(
            lang,
            "⚠️ This PR conflicts with `{default_name}`; rebase first, then re-run `r+`.",
            "⚠️ 本 PR 与 `{default_name}` 冲突;请先 rebase,再重新执行 `r+`。"
        );
        let _ = gh.post_issue_comment(repo, pr_number, &body).await;
        return EnqueueOutcome::Refused("conflicts with base".into());
    }

    if let Err(e) = gh
        .add_labels(
            repo,
            pr_number,
            std::slice::from_ref(&cfg.label_merge_queue_queued),
        )
        .await
    {
        return EnqueueOutcome::Refused(format!("cannot label: {e}"));
    }
    // Queue position is not computed here — the driver sorts by number, and a
    // position that races the labels is a claim we can't back. The ack says
    // what is true without a second read: it is queued.
    let queued_label = cfg.label_merge_queue_queued.as_str();
    let staging_name = cfg.merge_queue_staging_branch.as_str();
    let bot = cfg.bot_name.as_str();
    let body = t!(
        lang,
        "🧪 Queued for merge (`{queued_label}`). CI runs on the `{staging_name}` branch once a batch starts; use `@{bot} queue` to see the queue.",
        "🧪 已加入合并队列(`{queued_label}`)。批次开始后 CI 会在 `{staging_name}` 分支上运行;用 `@{bot} queue` 查看队列。"
    );
    let _ = gh.post_issue_comment(repo, pr_number, &body).await;
    EnqueueOutcome::Queued
}

/// Remove a PR from the queue, however deep it got.
///
/// The label is always the first attempt; a member that is already merged
/// into staging is *also* left for the driver's health check — the chain is
/// the truth about what testing, and a stale label removed here would not
/// unwind the branch. `note` may be empty (driver-driven drops carry their
/// own explanation).
pub async fn dequeue_pr(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    pr_number: i64,
    note: &str,
) -> String {
    let mut removed = false;
    for label in [
        &cfg.label_merge_queue_queued,
        &cfg.label_merge_queue_testing,
    ] {
        match gh.remove_label(repo, pr_number, label).await {
            Ok(_) => removed = true,
            // Removing a non-existent label is the normal "wasn't queued" case.
            Err(GhError::Api { status: 404, .. }) => {}
            Err(e) => return format!("error: remove {label}: {e}"),
        }
    }
    if removed && !note.is_empty() {
        let _ = gh.post_issue_comment(repo, pr_number, note).await;
    }
    if removed {
        "dequeued".into()
    } else {
        "was-not-queued".into()
    }
}

/// Queue status as a comment body: current batch (from the staging chain),
/// CI verdict on it, and the waiting list.
///
/// Rendered even when both lists are empty — a `queue` command that answers
/// silence looks broken.
pub async fn render_queue_status(gh: &Client, cfg: &Config, repo: &str, lang: Lang) -> String {
    if !cfg.merge_queue_enabled {
        return t!(
            lang,
            "ℹ️ The merge queue is disabled (set `MERGE_QUEUE_ENABLED=true`).",
            "ℹ️ 合并队列未启用(需设置 `MERGE_QUEUE_ENABLED=true`)。"
        );
    }
    let staging = &cfg.merge_queue_staging_branch;

    // The chain: everything on staging since the first non-marker commit.
    let chain = match gh.list_commits(repo, staging, 50).await {
        Ok(commits) => parse_chain(&commits),
        Err(_) => Vec::new(),
    };

    // CI verdict on the staging tip — the same fold the review prompt uses,
    // so `queue` and the driver can never disagree about what CI said.
    let ci = if chain.is_empty() {
        None
    } else {
        let tip = &chain[chain.len() - 1].merge_commit_sha;
        Some(crate::review::ci_state_for(gh, repo, tip).await)
    };

    let queued: Vec<String> = match gh
        .list_prs_with_label(repo, &cfg.label_merge_queue_queued)
        .await
    {
        Ok(prs) => prs
            .iter()
            .filter_map(|p| p.get("number").and_then(|n| n.as_i64()))
            .map(|n| format!("#{n}"))
            .collect(),
        Err(_) => Vec::new(),
    };

    let mut body = String::from("### Merge queue\n\n");
    if chain.is_empty() {
        body.push_str(&t!(
            lang,
            "**No batch is being tested.**",
            "**当前没有在测试的批次。**"
        ));
        body.push_str("\n\n");
    } else {
        let members = chain
            .iter()
            .map(|e| format!("#{}", e.pr))
            .collect::<Vec<_>>()
            .join(", ");
        let ci_line = match &ci {
            Some(crate::review::CiState::Green(names)) => {
                format!("✅ green ({})", names.join(", "))
            }
            Some(crate::review::CiState::Failed(names)) => {
                format!("❌ failed: {}", names.join(", "))
            }
            Some(crate::review::CiState::Pending(_)) => "⏳ pending".to_string(),
            Some(crate::review::CiState::Unknown) | None => "❓ unknown".to_string(),
        };
        let (staging_name, member_list, ci_text) =
            (staging.as_str(), members.as_str(), ci_line.as_str());
        let section = t!(
            lang,
            "**Testing on `{staging_name}`:** {member_list}\n**CI:** {ci_text}\n\n",
            "**正在 `{staging_name}` 上测试:** {member_list}\n**CI 状态:** {ci_text}\n\n"
        );
        body.push_str(&section);
    }
    if queued.is_empty() {
        body.push_str(&t!(lang, "**Queued:** —", "**排队中:** —"));
    } else {
        let list = queued.join(", ");
        let list_text = list.as_str();
        body.push_str(&t!(
            lang,
            "**Queued:** {list_text}",
            "**排队中:** {list_text}"
        ));
    }
    body.push_str("\n\n_(`r+` to enqueue, `r-` to withdraw)_");
    body
}

/// The loggable string for an enqueue outcome.
fn outcome_string(o: &EnqueueOutcome) -> String {
    match o {
        EnqueueOutcome::Queued => "queued".into(),
        EnqueueOutcome::Already => "already".into(),
        EnqueueOutcome::Refused(why) => format!("refused: {why}"),
    }
}

// ---------------------------------------------------------------------------
// Driver — the only writer of the staging branch
// ---------------------------------------------------------------------------

/// What one pump tick did, for the log and the /cron summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpOutcome {
    /// Nothing needed doing.
    Idle,
    /// A batch was built and staged for testing.
    Started(Vec<i64>),
    /// Still waiting on CI for the current batch.
    Testing,
    /// A member failed and was dropped; the rest of the batch continues.
    Dropped(i64, String),
    /// main was advanced with the batch.
    Advanced(Vec<i64>, String),
    /// The batch could not be advanced (branch protection, main moved).
    AdvanceBlocked(String),
    /// A hard failure (permissions, API); `pump_all` backs off this repo.
    Error(String),
}

impl std::fmt::Display for PumpOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PumpOutcome::Idle => f.write_str("idle"),
            PumpOutcome::Started(prs) => write!(f, "started {prs:?}"),
            PumpOutcome::Testing => f.write_str("testing"),
            PumpOutcome::Dropped(pr, why) => write!(f, "dropped #{pr}: {why}"),
            PumpOutcome::Advanced(prs, sha) => write!(f, "advanced {prs:?} as {sha}"),
            PumpOutcome::AdvanceBlocked(why) => write!(f, "advance blocked: {why}"),
            PumpOutcome::Error(e) => write!(f, "error: {e}"),
        }
    }
}

/// Process-local hard-failure backoff: `repo → backoff deadline`.
///
/// A 403 (missing permission) on the staging writes will not heal in 30s,
/// and retrying every tick posts a comment each time on someone's PR — the
/// exact spam the poll loop must not produce. Mirrors the static-guard
/// pattern of `REVIEW_LOCKS`; the deadline is wall-clock so the entry
/// expires on its own.
static BACKOFF: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
> = std::sync::OnceLock::new();

const BACKOFF_DURATION: std::time::Duration = std::time::Duration::from_secs(30 * 60);

fn backoff() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    BACKOFF.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn in_backoff(repo: &str) -> bool {
    let mut map = backoff().lock().unwrap();
    match map.get(repo) {
        Some(deadline) if *deadline > std::time::Instant::now() => true,
        _ => {
            map.remove(repo);
            false
        }
    }
}

fn start_backoff(repo: &str) {
    backoff().lock().unwrap().insert(
        repo.to_string(),
        std::time::Instant::now() + BACKOFF_DURATION,
    );
}

/// Pump every installation the App can see, like the rebase sweep.
///
/// Serial on purpose: two concurrent pumps would race the staging ref and
/// the label dance. The 200ms per-repo pacing matches `rebase::sweep`.
pub async fn pump_all(cfg: &Config) -> String {
    if !cfg.merge_queue_enabled {
        return "merge queue disabled".into();
    }
    let app = match crate::github::Client::app_client(cfg) {
        Ok(c) => c,
        Err(e) => return format!("app-client-error: {e}"),
    };
    let installations: Vec<Value> =
        match crate::github::paginate(&app, "/app/installations?per_page=100").await {
            Ok(v) => v,
            Err(e) => return format!("installations-error: {e}"),
        };

    let mut repos = 0usize;
    let mut errors = 0usize;
    let mut skipped = 0usize;

    for inst in &installations {
        let Some(inst_id) = inst.get("id").and_then(|i| i.as_i64()) else {
            continue;
        };
        let Ok(gh) = crate::github::Client::installation_resolved(cfg, inst_id).await else {
            errors += 1;
            continue;
        };
        let repos_list = match crate::github::Client::installation_repositories_via(&gh).await {
            Ok(r) => r,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        for repo in repos_list {
            repos += 1;
            if in_backoff(&repo) {
                skipped += 1;
                continue;
            }
            match pump_one(&gh, cfg, &repo).await {
                PumpOutcome::Idle | PumpOutcome::Testing => {}
                PumpOutcome::Error(e) => {
                    // A 403 on the writes is a configuration problem; back
                    // off before it becomes a comment per tick.
                    if e.contains("403") {
                        start_backoff(&repo);
                    }
                    errors += 1;
                    tracing::warn!("merge queue pump {repo}: {e}");
                }
                other => tracing::info!("merge queue pump {repo}: {other}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    let summary =
        format!("merge queue pump done: {repos} repos, {errors} errors, {skipped} backed off");
    tracing::info!("{summary}");
    summary
}

/// One idempotent transition per repo. Called every tick; each call does at
/// most one staging mutation so a stuck member can't turn into a storm.
pub async fn pump_one(gh: &Client, cfg: &Config, repo: &str) -> PumpOutcome {
    let default_branch = match gh.repo_info(repo).await {
        Ok(r) => r
            .get("default_branch")
            .and_then(|b| b.as_str())
            .unwrap_or("main")
            .to_string(),
        Err(e) => return PumpOutcome::Error(format!("repo info: {e}")),
    };
    let staging = cfg.merge_queue_staging_branch.clone();

    // The staging ref may be absent (no batch ever, or cleaned up after one).
    let staging_ref = gh.get_branch_ref(repo, &staging).await;
    let staging_head = match &staging_ref {
        Ok(r) => r
            .pointer("/object/sha")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        Err(GhError::Api { status: 404, .. }) => String::new(),
        Err(e) => return PumpOutcome::Error(format!("staging ref: {e}")),
    };

    // The batch's members are the PRs wearing the testing label — the label is
    // the state (the codebase convention), and it survives what the commit
    // chain cannot: GitHub is free to word a merge commit itself (observed on
    // a fast-forward-able fold: `Merge <head> into <base>`, our
    // `commit_message` dropped on the floor), so a marker-based parse of the
    // staging history can come back empty while a batch is very much under
    // test. The chain read below is advisory — it supplies order and merge
    // shas when present, and is rebuilt from labels when not.
    let labeled_members: Vec<i64> = match gh
        .list_prs_with_label(repo, &cfg.label_merge_queue_testing)
        .await
    {
        Ok(prs) => prs
            .iter()
            .filter_map(|p| p.get("number").and_then(|n| n.as_i64()))
            .collect(),
        Err(e) => return PumpOutcome::Error(format!("testing list: {e}")),
    };

    let marked_chain = if staging_head.is_empty() {
        Vec::new()
    } else {
        let commits = match gh.list_commits(repo, &staging, 50).await {
            Ok(c) => c,
            Err(e) => return PumpOutcome::Error(format!("staging commits: {e}")),
        };
        parse_chain(&commits)
    };

    // Reconcile: the chain the driver works with. Marker entries win for
    // order/merge-shas (they are what staging actually points at); testing
    // members the chain doesn't know about are appended in ascending number
    // order with the staging tip as their nominal merge commit — the CI read
    // uses the tip either way, and a reset needs a real sha, which the marked
    // entries or main provide.
    let mut chain = marked_chain.clone();
    for n in &labeled_members {
        if !chain.iter().any(|e| e.pr == *n) {
            chain.push(ChainEntry {
                pr: *n,
                head_sha: String::new(),
                merge_commit_sha: staging_head.clone(),
            });
        }
    }
    // Members the chain knows but the labels don't: a previous tick's label
    // removal was interrupted, or someone stripped a label by hand. Trust the
    // labels (they are the state) — a PR that left the batch without its
    // label is out; rebuild around it so staging stops containing it.
    let stripped: Vec<ChainEntry> = marked_chain
        .iter()
        .filter(|e| !labeled_members.contains(&e.pr))
        .cloned()
        .collect();
    if !stripped.is_empty() && !marked_chain.is_empty() {
        // Rebuild from the first stripped entry onward: their merge commits
        // must come off staging, and any still-labeled members after them
        // must be re-merged (their commits die with the reset).
        let idx = marked_chain
            .iter()
            .position(|e| e.pr == stripped[0].pr)
            .unwrap_or(0);
        return rebuild_from(gh, cfg, repo, &marked_chain, idx, "label removed mid-batch").await;
    }

    if chain.is_empty() {
        return start_batch(gh, cfg, repo, &staging, &default_branch, &staging_head).await;
    }

    // Health check: every member must still be open at the sha we merged.
    if let Some(outcome) = check_members(gh, cfg, repo, &chain).await {
        return outcome;
    }

    // CI on the staging tip. Failed and Pending-with-timeout both end the
    // batch (differently); green advances it.
    let tip = staging_head.clone();
    if tip.is_empty() {
        // Labeled members but no staging branch: the branch was deleted under
        // us (the exact bug this reconciliation exists to prevent). Rebuild
        // the batch from scratch — start_batch force-resets/creates staging
        // from main and re-folds every member.
        return start_batch(gh, cfg, repo, &staging, &default_branch, "").await;
    }
    let ci = crate::review::ci_state_for(gh, repo, &tip).await;
    match &ci {
        crate::review::CiState::Green(_) => {
            advance_main(gh, cfg, repo, &staging, &default_branch, &chain).await
        }
        crate::review::CiState::Failed(names) => {
            drop_culprit(gh, cfg, repo, &staging, &chain, names).await
        }
        crate::review::CiState::Pending(_) => PumpOutcome::Testing,
        crate::review::CiState::Unknown => {
            // No visible CI at all — same grace clock as Pending, because
            // "the CI never ran" and "the CI is slow" look identical here.
            if batch_timed_out(gh, cfg, repo, &chain).await {
                timeout_batch(gh, cfg, repo, &chain).await
            } else {
                PumpOutcome::Testing
            }
        }
    }
}

/// Start a new batch from the queued PRs, or idle if there are none.
///
/// `staging_head` is the staging ref the caller already read (`""` when the
/// branch is absent); re-reading it here would double the request for no
/// information.
async fn start_batch(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    staging: &str,
    default_branch: &str,
    staging_head: &str,
) -> PumpOutcome {
    let queued = match gh
        .list_prs_with_label(repo, &cfg.label_merge_queue_queued)
        .await
    {
        Ok(p) => p,
        Err(e) => return PumpOutcome::Error(format!("queued list: {e}")),
    };
    let mut numbers: Vec<i64> = queued
        .iter()
        .filter_map(|p| p.get("number").and_then(|n| n.as_i64()))
        .collect();
    numbers.sort_unstable();
    numbers.truncate(cfg.merge_queue_max_batch);
    if numbers.is_empty() {
        // Nothing to do. The only staging that is safe to clean up is one
        // that already points at main's tip — a *pure leftover*. A staging
        // holding anything else belongs to a batch we may not be able to
        // read (GitHub has been observed wording merge commits itself,
        // dropping our marker), and deleting it killed a batch under test
        // once already: the CI that was about to run on it never ran, and
        // the members sat labeled and silent until they timed out.
        if !staging_head.is_empty() && cfg.merge_queue_cleanup_staging {
            let main_sha = gh
                .get_branch_ref(repo, default_branch)
                .await
                .ok()
                .and_then(|r| {
                    r.pointer("/object/sha")
                        .and_then(|s| s.as_str())
                        .map(String::from)
                })
                .unwrap_or_default();
            if !main_sha.is_empty() && staging_head == main_sha {
                if let Err(e) = gh.delete_branch_ref(repo, staging).await {
                    tracing::warn!("merge queue: delete unused {staging} on {repo}: {e}");
                }
            } else if !main_sha.is_empty() {
                // Reset the leftover to main instead of deleting it: the
                // next batch needs staging at main's tip anyway, and a
                // reset is the same write the batch start would do.
                if let Err(e) = gh.update_branch_ref(repo, staging, &main_sha, true).await {
                    return PumpOutcome::Error(format!("staging reset: {e}"));
                }
            }
        }
        return PumpOutcome::Idle;
    }

    let lang = crate::lang::for_pr(gh, repo, numbers[0], None).await;

    // Build the branch from main's tip. Freshly reset every time — the batch
    // must be tested on top of what main is *now*, not what it was when the
    // last batch started.
    let main_sha = match gh.get_branch_ref(repo, default_branch).await {
        Ok(r) => r
            .pointer("/object/sha")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => return PumpOutcome::Error(format!("main ref: {e}")),
    };
    if main_sha.is_empty() {
        return PumpOutcome::Error("main ref has no sha".into());
    }
    if staging_head.is_empty() {
        if let Err(e) = gh.create_branch_ref(repo, staging, &main_sha).await {
            return PumpOutcome::Error(format!("staging create: {e}"));
        }
    } else if staging_head != main_sha {
        if let Err(e) = gh.update_branch_ref(repo, staging, &main_sha, true).await {
            return PumpOutcome::Error(format!("staging reset: {e}"));
        }
    }

    // Fold each head in, in order. A conflict drops that PR (with a clear
    // comment) and keeps going — one unbuildable member must not block the
    // rest.
    let mut merged = Vec::new();
    let mut conflicts = Vec::new();
    for n in &numbers {
        let pr = match gh.get_pr(repo, *n).await {
            Ok(p) => p,
            Err(e) => {
                conflicts.push((*n, format!("cannot read PR: {e}")));
                continue;
            }
        };
        if pr.get("state").and_then(|s| s.as_str()) != Some("open") {
            conflicts.push((*n, "closed".into()));
            continue;
        }
        let head_ref = head_ref_of(&pr);
        let head_sha = pr
            .pointer("/head/sha")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let message = format!("{MARKER_PREFIX}#{n} (head {head_sha})");
        match gh.merge_branches(repo, staging, &head_ref, &message).await {
            Ok(_) => merged.push(*n),
            Err(GhError::Api {
                status: 409,
                message,
            }) => conflicts.push((*n, message)),
            Err(e) => return PumpOutcome::Error(format!("merge #{n}: {e}")),
        }
    }

    // Swap labels: members → testing. A member that conflicted keeps neither
    // label after dequeue_pr; a member that merged loses queued.
    for n in &merged {
        let _ = gh
            .remove_label(repo, *n, &cfg.label_merge_queue_queued)
            .await;
        let _ = gh
            .add_labels(
                repo,
                *n,
                std::slice::from_ref(&cfg.label_merge_queue_testing),
            )
            .await;
    }
    for (n, why) in &conflicts {
        dequeue_pr(gh, cfg, repo, *n, &conflict_comment(repo, *n, why, lang)).await;
    }

    if merged.is_empty() {
        // Every member conflicted; the staging branch now says nothing. Reset
        // it back to main so the next batch starts clean.
        let _ = gh.update_branch_ref(repo, staging, &main_sha, true).await;
        return PumpOutcome::Idle;
    }
    let list = merged
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let (staging_name, default_name, batch_list) = (staging, default_branch, list.as_str());
    let body = t!(
        lang,
        "🚀 Testing batch {batch_list} on `{staging_name}` — CI is running on the staging push; \
I'll advance `{default_name}` when it's green.",
        "🚀 正在 `{staging_name}` 上测试批次 {batch_list} —— CI 会在 staging 的 push 上运行;\
全绿后我会推进 `{default_name}`。"
    );
    // One comment, on the oldest member: a batch announcement on every
    // member is N times the noise for the same information.
    let _ = gh.post_issue_comment(repo, merged[0], &body).await;
    PumpOutcome::Started(merged)
}

/// A member's PR changed under us (force-push, closed, merged elsewhere).
///
/// Rebuild: reset staging to just before the offender, drop them, re-merge
/// everyone after them (their merge commits are gone with the reset). One
/// health-driven rebuild per tick keeps the recovery bounded.
async fn check_members(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    chain: &[ChainEntry],
) -> Option<PumpOutcome> {
    for (idx, entry) in chain.iter().enumerate() {
        let pr = match gh.get_pr(repo, entry.pr).await {
            Ok(p) => p,
            Err(e) => {
                return Some(
                    rebuild_from(gh, cfg, repo, chain, idx, &format!("unreadable: {e}")).await,
                );
            }
        };
        if pr.get("state").and_then(|s| s.as_str()) != Some("open") {
            let why = if pr.get("merged").and_then(|m| m.as_bool()) == Some(true) {
                "merged elsewhere"
            } else {
                "closed"
            };
            return Some(rebuild_from(gh, cfg, repo, chain, idx, why).await);
        }
        let head_now = pr
            .pointer("/head/sha")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        // An empty recorded head (hand-written marker) can't prove a change;
        // skipping it is the honest reading rather than forcing a rebuild.
        if !entry.head_sha.is_empty() && head_now != entry.head_sha {
            return Some(
                rebuild_from(gh, cfg, repo, chain, idx, "head was changed mid-batch").await,
            );
        }
    }
    None
}

/// Reset staging to just before `chain[idx]`, drop that member, and re-merge
/// the members after them. Returns the outcome to log.
async fn rebuild_from(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    chain: &[ChainEntry],
    idx: usize,
    why: &str,
) -> PumpOutcome {
    let entry = &chain[idx];
    let lang = crate::lang::for_pr(gh, repo, entry.pr, None).await;

    // The reset target: the merge commit *before* the offender, or main if
    // they were first.
    let reset_sha = if idx == 0 {
        // The chain's first parent — the batch base. Reading it from the
        // offender's commit is the only way to know it without remembering.
        match gh.list_commits(repo, &entry.merge_commit_sha, 1).await {
            Ok(commits) => commits
                .first()
                .and_then(|c| c.pointer("/parents/0/sha"))
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            Err(e) => return PumpOutcome::Error(format!("parent of #{}: {e}", entry.pr)),
        }
    } else {
        chain[idx - 1].merge_commit_sha.clone()
    };
    if reset_sha.is_empty() {
        return PumpOutcome::Error(format!("rebuild of #{}: no parent sha", entry.pr));
    }
    let staging = cfg.merge_queue_staging_branch.clone();
    if let Err(e) = gh.update_branch_ref(repo, &staging, &reset_sha, true).await {
        return PumpOutcome::Error(format!("staging reset for rebuild: {e}"));
    }

    // Drop the offender…
    let _ = dequeue_pr(gh, cfg, repo, entry.pr, "").await;
    let body = t!(
        lang,
        "➖ Removed from the merge queue: {why}.",
        "➖ 已移出合并队列:{why}。"
    )
    .replace("{why}", why);
    let _ = gh.post_issue_comment(repo, entry.pr, &body).await;

    // …and re-merge the members after them (their commits died in the reset).
    let mut remerged = Vec::new();
    for later in &chain[idx + 1..] {
        let pr = match gh.get_pr(repo, later.pr).await {
            Ok(p) => p,
            Err(e) => {
                let _ = dequeue_pr(gh, cfg, repo, later.pr, "").await;
                tracing::warn!("merge queue rebuild: cannot re-read #{}: {e}", later.pr);
                continue;
            }
        };
        let head_ref = head_ref_of(&pr);
        let message = format!("{MARKER_PREFIX}#{} (head {})", later.pr, later.head_sha);
        match gh.merge_branches(repo, &staging, &head_ref, &message).await {
            Ok(_) => remerged.push(later.pr),
            Err(e) => {
                let _ = dequeue_pr(gh, cfg, repo, later.pr, "").await;
                tracing::warn!("merge queue rebuild: re-merge #{} failed: {e}", later.pr);
            }
        }
    }
    PumpOutcome::Dropped(entry.pr, why.to_string())
}

/// The batch failed CI: drop the tail member (the newest unknown), reset
/// staging to before them, and let the prefix re-test.
async fn drop_culprit(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    staging: &str,
    chain: &[ChainEntry],
    failed_names: &[String],
) -> PumpOutcome {
    let Some(idx) = culprit_of(chain) else {
        return PumpOutcome::Idle;
    };
    let culprit = &chain[idx];
    let lang = crate::lang::for_pr(gh, repo, culprit.pr, None).await;

    let reset_sha = if idx == 0 {
        match gh.list_commits(repo, &culprit.merge_commit_sha, 1).await {
            Ok(commits) => commits
                .first()
                .and_then(|c| c.pointer("/parents/0/sha"))
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            Err(e) => return PumpOutcome::Error(format!("parent of #{}: {e}", culprit.pr)),
        }
    } else {
        chain[idx - 1].merge_commit_sha.clone()
    };
    if let Err(e) = gh.update_branch_ref(repo, staging, &reset_sha, true).await {
        return PumpOutcome::Error(format!("staging reset after red: {e}"));
    }

    let _ = dequeue_pr(gh, cfg, repo, culprit.pr, "").await;
    let failing = failed_names.join(", ");
    let (staging_name, failing_checks, culprit_pr) = (staging, failing.as_str(), culprit.pr);
    let body = t!(
        lang,
        "❌ Batch CI failed on `{staging_name}` — failing checks: {failing_checks}.\n\n\
Removing #{culprit_pr} from the queue as the likely culprit (the batch was built in order, \
so the prefix tested green before this merge). Fix the issue and re-run `r+`; \
the rest of the batch re-tests automatically.",
        "❌ 批次在 `{staging_name}` 上 CI 失败 —— 失败的检查:{failing_checks}。\n\n\
将 #{culprit_pr} 移出队列(按批次顺序,此前的成员大概率无责)。修复后重新 `r+`;\
批次其余成员会自动重测。"
    );
    let _ = gh.post_issue_comment(repo, culprit.pr, &body).await;

    // No re-merge needed here: `culprit_of` always returns the tail, so the
    // remaining prefix is already exactly what staging points at after the
    // reset — the next tick simply re-reads its CI.
    PumpOutcome::Dropped(culprit.pr, "CI failed".to_string())
}

/// CI never reached a verdict within the timeout: return the whole batch.
///
/// This is the "CI doesn't run on staging pushes" path — the most common
/// misconfiguration. The comment explains the cause so the fix is findable.
async fn timeout_batch(gh: &Client, cfg: &Config, repo: &str, chain: &[ChainEntry]) -> PumpOutcome {
    let lang = crate::lang::for_pr(gh, repo, chain[0].pr, None).await;
    let members = chain.iter().map(|e| e.pr).collect::<Vec<i64>>();
    for entry in chain {
        let _ = dequeue_pr(gh, cfg, repo, entry.pr, "").await;
    }
    let timeout_h = cfg.merge_queue_ci_timeout_secs / 3600;
    let staging_name = cfg.merge_queue_staging_branch.as_str();
    let list = members
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let batch_list = list.as_str();
    let body = t!(
        lang,
        "⏱️ The batch ({batch_list}) waited over {timeout_h}h on `{staging_name}` without CI reaching a \
verdict, so I've returned every PR to the queue.\n\n\
Most common cause: your CI workflows trigger on `pull_request` only and never run on \
`{staging_name}` pushes. Make sure the workflows that gate this repo run with \
`on: push: branches: [{staging_name}]` (or unfiltered `on: push`), then re-run `r+`.",
        "⏱️ 批次({batch_list})在 `{staging_name}` 上等待超过 {timeout_h} 小时仍未得到 CI 结论,\
已将全部 PR 退回队列。\n\n\
最常见的原因:CI workflow 只在 `pull_request` 上触发,不会在 `{staging_name}` 的 push 上运行。\
请确保仓库的关键 workflow 以 `on: push: branches: [{staging_name}]`(或不限分支的 `on: push`)\
触发,然后重新 `r+`。"
    );
    let _ = gh.post_issue_comment(repo, members[0], &body).await;
    PumpOutcome::Idle
}

/// Has the batch been waiting too long? The last merge commit's committer
/// date is when the batch last changed — the honest "start of the wait".
async fn batch_timed_out(gh: &Client, cfg: &Config, repo: &str, chain: &[ChainEntry]) -> bool {
    let tip = &chain[chain.len() - 1].merge_commit_sha;
    let commits = match gh.list_commits(repo, tip, 1).await {
        Ok(c) => c,
        Err(_) => return false, // can't read the clock → don't fire the timeout
    };
    let Some(date) = commits
        .first()
        .and_then(|c| c.pointer("/commit/committer/date"))
        .and_then(|d| d.as_str())
    else {
        return false;
    };
    // RFC 3339 without a parser dependency: "2026-01-02T03:04:05Z" sorts
    // lexicographically, so comparing string dates would work for ordering —
    // but the timeout needs an age, so parse what we need by hand.
    let secs = rfc3339_to_secs(date);
    match secs {
        Some(then) => {
            let now = crate::github::chrono_now_secs();
            let elapsed = (now - then as i64).max(0) as u64;
            elapsed >= cfg.merge_queue_ci_timeout_secs
        }
        None => false,
    }
}

/// Parse "YYYY-MM-DDTHH:MM:SSZ" into a Unix timestamp. Days-since-epoch via
/// the civil-date algorithm; no chrono dependency added for one field.
fn rfc3339_to_secs(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<u64>().ok();
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Howard Hinnant's days_from_civil (the 719_468 offset is 1970-01-01).
    let y = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    Some((days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64) as u64)
}

/// Advance main: create (or reuse) the staging→main PR and merge it, or
/// fast-forward the ref directly under `MERGE_QUEUE_ADVANCE_METHOD=ref`.
async fn advance_main(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    staging: &str,
    default_branch: &str,
    chain: &[ChainEntry],
) -> PumpOutcome {
    let lang = crate::lang::for_pr(gh, repo, chain[0].pr, None).await;
    let staging_head = chain[chain.len() - 1].merge_commit_sha.clone();
    let members: Vec<i64> = chain.iter().map(|e| e.pr).collect();

    match cfg.merge_queue_advance_method.as_str() {
        "ref" => {
            // True fast-forward. `force: false` — a main that moved by hand
            // refuses the update (non-FF), which is the protection working.
            match gh
                .update_branch_ref(repo, default_branch, &staging_head, false)
                .await
            {
                Ok(_) => complete_batch(gh, cfg, repo, chain, &staging_head, lang).await,
                Err(GhError::Api {
                    status: 422 | 409 | 403,
                    message,
                }) => {
                    PumpOutcome::AdvanceBlocked(format!("ref update refused: {message} (status)"))
                }
                Err(e) => PumpOutcome::Error(format!("ref update: {e}")),
            }
        }
        _ => {
            // PR path. One advance PR per batch; close stale ones first —
            // identified by the marker in the body, not by author.
            if let Err(e) = close_stale_advance_prs(gh, repo, default_branch, staging).await {
                return PumpOutcome::Error(format!("stale advance prs: {e}"));
            }
            let title = format!("xero-bot: advance {default_branch} to {}", staging);
            let list = members
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            let body_text = format!(
                "{ADVANCE_MARKER}\n\nBatch {list} tested green on `{staging}`. Merging this PR \
advances `{default_branch}` to the tested combination.\n\n_Merged automatically by \
xero-bot — approving it approves the batch._"
            );
            let pr = match gh
                .create_pr(repo, staging, default_branch, &title, &body_text)
                .await
            {
                Ok(p) => p,
                // 422 "no commits between main and staging" = main is already
                // at (or past) the staging head. The batch was advanced by
                // someone else or a previous attempt died after merging —
                // either way the merge is done; complete, don't retry.
                Err(GhError::Api { status: 422, .. }) => {
                    return complete_batch(gh, cfg, repo, chain, &staging_head, lang).await;
                }
                Err(e) => return PumpOutcome::Error(format!("create advance pr: {e}")),
            };
            let number = pr
                .get("number")
                .and_then(|n| n.as_i64())
                .unwrap_or_default();
            let title = format!("xero-bot: advance {default_branch} ({list})");
            match gh
                .merge_pr(
                    repo,
                    number,
                    &cfg.merge_queue_advance_merge_method,
                    &title,
                    &format!("Batch {list} tested green on `{staging}` (xero-bot merge queue)."),
                )
                .await
            {
                Ok(v) => {
                    let sha = v
                        .get("sha")
                        .and_then(|s| s.as_str())
                        .unwrap_or(&staging_head)
                        .to_string();
                    complete_batch(gh, cfg, repo, chain, &sha, lang).await
                }
                // 405 "not mergeable": branch protection wants a human review
                // on the advance PR. That's a legitimate out — approving the
                // advance PR approves the batch — so keep the batch and retry.
                Err(GhError::Api {
                    status: 405 | 403,
                    message,
                }) => {
                    let (default_name, detail) = (default_branch, message.as_str());
                    let note = t!(
                        lang,
                        "ℹ️ The batch is green, but advancing `{default_name}` needs a human review on \
the advance PR first ({detail}). Approve it to let the merge through — approving it \
approves the whole batch.",
                        "ℹ️ 批次已全绿,但推进 `{default_name}` 前需要人工批准推进 PR({detail})。\
批准该 PR 即批准整批合并。"
                    );
                    let _ = gh.post_issue_comment(repo, members[0], &note).await;
                    PumpOutcome::AdvanceBlocked(message)
                }
                Err(e) => PumpOutcome::Error(format!("merge advance pr: {e}")),
            }
        }
    }
}

/// Close earlier advance PRs (same head→base, marker in the body). Batch
/// rebuilds leave them pointing at a staging sha that no longer exists.
async fn close_stale_advance_prs(
    gh: &Client,
    repo: &str,
    default_branch: &str,
    staging: &str,
) -> Result<(), GhError> {
    // The pulls list can't filter by head ref server-side; one page of open
    // PRs toward the default branch is plenty — an advance PR is rare.
    let base_query = crate::github::enc_seg(default_branch);
    let open = gh
        .get_all(&format!(
            "/repos/{repo}/pulls?state=open&base={base_query}&per_page=100"
        ))
        .await?;
    for p in open {
        let head_ref = p
            .pointer("/head/ref")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if head_ref != staging {
            continue;
        }
        let has_marker = p
            .get("body")
            .and_then(|b| b.as_str())
            .map(|b| b.contains(ADVANCE_MARKER))
            .unwrap_or(false);
        if !has_marker {
            continue;
        }
        if let Some(n) = p.get("number").and_then(|n| n.as_i64()) {
            gh.patch(
                &format!("/repos/{repo}/pulls/{n}"),
                Some(serde_json::json!({"state": "closed"})),
            )
            .await?;
        }
    }
    Ok(())
}

/// Batch merged into main: announce, clear labels, optionally delete staging.
async fn complete_batch(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    chain: &[ChainEntry],
    merge_sha: &str,
    lang: Lang,
) -> PumpOutcome {
    let list = chain
        .iter()
        .map(|e| format!("#{}", e.pr))
        .collect::<Vec<_>>()
        .join(", ");
    for entry in chain {
        let (merge_point, batch_list) = (merge_sha, list.as_str());
        let body = t!(
            lang,
            "🎉 Batch merged into `main` as `{merge_point}`: {batch_list}. Thank you!",
            "🎉 批次已合并进 `main`({merge_point}):{batch_list}。感谢!"
        );
        let _ = gh.post_issue_comment(repo, entry.pr, &body).await;
        let _ = gh
            .remove_label(repo, entry.pr, &cfg.label_merge_queue_testing)
            .await;
    }
    if cfg.merge_queue_cleanup_staging {
        let staging = cfg.merge_queue_staging_branch.as_str();
        if let Err(e) = gh.delete_branch_ref(repo, staging).await {
            tracing::warn!("merge queue: delete {staging} on {repo} after completion: {e}");
        }
    }
    PumpOutcome::Advanced(chain.iter().map(|e| e.pr).collect(), merge_sha.to_string())
}

fn conflict_comment(repo: &str, pr: i64, why: &str, lang: Lang) -> String {
    let _ = repo;
    let (pr_num, reason) = (pr, why);
    t!(
        lang,
        "⚠️ #{pr_num} could not be merged into the staging branch ({reason}). Rebase onto the latest \
base branch and re-run `r+`.",
        "⚠️ #{pr_num} 无法并入 staging 分支({reason})。请 rebase 到最新基线后重新 `r+`。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A commit value as `/commits` returns it.
    fn commit(sha: &str, message: &str) -> Value {
        json!({"sha": sha, "commit": {"message": message}})
    }

    #[test]
    fn parse_chain_reads_marker_messages() {
        let commits = [
            commit("mm", "xero-bot: merge #456 (head def4567)"),
            commit("ee", "xero-bot: merge #123 (head abc1234)"),
            commit("bb", "Initial commit"),
        ];
        let chain = parse_chain(&commits);
        assert_eq!(
            chain,
            vec![
                ChainEntry {
                    pr: 123,
                    head_sha: "abc1234".into(),
                    merge_commit_sha: "ee".into(),
                },
                ChainEntry {
                    pr: 456,
                    head_sha: "def4567".into(),
                    merge_commit_sha: "mm".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_chain_stops_at_base_and_tolerates_marker_without_head() {
        let commits = [
            commit("mm", "xero-bot: merge #2"),
            commit("bb", "Merge pull request #1 from someone/feature"),
        ];
        let chain = parse_chain(&commits);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].pr, 2);
        assert_eq!(chain[0].head_sha, "");
    }

    #[test]
    fn parse_chain_rejects_marker_without_a_number() {
        // A torn or hand-edited marker can be matched to no PR; trusting the
        // prefix would let labels and chain disagree.
        let commits = [commit("mm", "xero-bot: merge (nonsense)")];
        assert!(parse_chain(&commits).is_empty());
    }

    #[test]
    fn parse_chain_does_not_match_a_superset_prefix() {
        // Another consumer might one day write "xero-bot: merge attempt #3";
        // it must not parse as PR 3.
        let commits = [commit("mm", "xero-bot: merge attempt #3")];
        assert!(parse_chain(&commits).is_empty());
    }

    #[test]
    fn parse_chain_empty_on_empty() {
        assert!(parse_chain(&[]).is_empty());
    }

    #[test]
    fn head_ref_of_same_repo_and_fork() {
        let same = json!({
            "head": {"ref": "feature", "repo": {"full_name": "octo/hello"}}
        });
        assert_eq!(head_ref_of(&same), "feature");

        let fork = json!({
            "head": {"ref": "feature", "label": "someone:feature", "repo": null}
        });
        assert_eq!(head_ref_of(&fork), "someone:feature");
    }

    #[test]
    fn culprit_is_always_the_tail() {
        assert_eq!(culprit_of(&[]), None);
        let one = [ChainEntry {
            pr: 1,
            head_sha: String::new(),
            merge_commit_sha: String::new(),
        }];
        assert_eq!(culprit_of(&one), Some(0));
        let two = [
            one[0].clone(),
            ChainEntry {
                pr: 2,
                head_sha: String::new(),
                merge_commit_sha: String::new(),
            },
        ];
        assert_eq!(culprit_of(&two), Some(1));
    }

    /// The timeout clock reads commit timestamps by hand, so the civil-date
    /// math must be exact at known points. `2020-01-01T00:00:00Z` is
    /// 1_577_836_800 (leap-year era math), `1970-01-02` is one day.
    #[test]
    fn rfc3339_epoch_seconds() {
        assert_eq!(rfc3339_to_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_to_secs("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(rfc3339_to_secs("2020-01-01T00:00:00Z"), Some(1_577_836_800));
        // Leap year: 2024-02-29 is the day after 2024-02-28.
        let feb28 = rfc3339_to_secs("2024-02-28T00:00:00Z").unwrap();
        let feb29 = rfc3339_to_secs("2024-02-29T00:00:00Z").unwrap();
        assert_eq!(feb29 - feb28, 86_400);
        // and 2023-02-29 does not exist — the shape check can't catch it, but
        // the arithmetic must at least not wrap into March silently for a
        // well-formed-but-wrong input. (We don't validate month lengths; the
        // caller reads GitHub's own timestamps.)
        assert!(rfc3339_to_secs("2024-02-29T23:59:59Z").is_some());
        // malformed inputs are refused, never guessed
        assert_eq!(rfc3339_to_secs(""), None);
        assert_eq!(rfc3339_to_secs("2026-01-02 03:04:05"), None);
        assert_eq!(rfc3339_to_secs("2026-13-02T03:04:05Z"), None);
    }
}
