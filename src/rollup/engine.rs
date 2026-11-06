use super::*;
use crate::{
    config::Config,
    github::{Client, GhError},
    trigger_state::Store,
};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Green,
    Failed,
    Pending,
}
/// All named checks from GitHub Actions must succeed on this candidate. Missing,
/// skipped, neutral and cancelled checks cannot authorize a merge.
pub fn github_verdict(checks: &[Value], required: &[String], sha: &str) -> Verdict {
    if required.is_empty() {
        return Verdict::Pending;
    }
    let mut pending = false;
    for name in required {
        let check = checks
            .iter()
            .filter(|v| {
                v["name"] == *name && v["head_sha"] == sha && v["app"]["slug"] == "github-actions"
            })
            .max_by_key(|v| v["id"].as_i64().unwrap_or(0));
        match check.and_then(|v| v["conclusion"].as_str()) {
            Some("success") => (),
            Some(
                "failure" | "timed_out" | "cancelled" | "action_required" | "skipped" | "neutral",
            ) => return Verdict::Failed,
            _ => pending = true,
        }
    }
    if pending {
        Verdict::Pending
    } else {
        Verdict::Green
    }
}
pub(crate) fn passed_provider(
    github: Option<&Verdict>,
    forgejo: Option<&Verdict>,
) -> Option<&'static str> {
    if github == Some(&Verdict::Green) {
        Some("github")
    } else if forgejo == Some(&Verdict::Green) {
        Some("forgejo")
    } else {
        None
    }
}

pub async fn github_ci(
    gh: &Client,
    repo: &str,
    sha: &str,
    branch: &str,
    required: &[String],
) -> Result<Verdict> {
    let runs = gh
        .get(&format!(
            "/repos/{repo}/actions/runs?head_sha={sha}&event=push&branch={}&per_page=100",
            crate::github::enc_seg(branch)
        ))
        .await?;
    if runs["total_count"].as_u64().unwrap_or(u64::MAX) > 100 {
        return Err("too many workflow runs to verify".into());
    }
    let mut latest: BTreeMap<i64, &Value> = BTreeMap::new();
    for run in runs["workflow_runs"]
        .as_array()
        .ok_or("missing workflow runs")?
    {
        if run["head_sha"] != sha || run["event"] != "push" || run["head_branch"] != branch {
            continue;
        }
        let workflow = run["workflow_id"]
            .as_i64()
            .ok_or("workflow identity missing")?;
        if latest
            .get(&workflow)
            .is_none_or(|old| old["id"].as_i64() < run["id"].as_i64())
        {
            latest.insert(workflow, run);
        }
    }
    let mut checks = Vec::new();
    for run in latest.values() {
        let id = run["id"].as_i64().ok_or("run identity missing")?;
        let jobs = gh
            .get(&format!(
                "/repos/{repo}/actions/runs/{id}/jobs?filter=latest&per_page=100"
            ))
            .await?;
        if jobs["total_count"].as_u64().unwrap_or(u64::MAX) > 100 {
            return Err("too many workflow jobs to verify".into());
        }
        for job in jobs["jobs"].as_array().ok_or("missing workflow jobs")? {
            if job["head_sha"] != sha || job["run_id"] != id {
                return Err("workflow job commit/run mismatch".into());
            }
            checks.push(json!({"id":job["id"],"name":job["name"],"head_sha":sha,"app":{"slug":"github-actions"},"conclusion":job["conclusion"]}));
        }
    }
    Ok(github_verdict(&checks, required, sha))
}
async fn forgejo_target(
    gh: &Client,
    cfg: &Config,
    repo: &Repository,
    p: &Policy,
) -> Result<(crate::forgejo::ForgejoClient, String)> {
    let key = crate::config::cache::RepositoryKey {
        installation_id: repo.installation,
        repository_id: repo.id,
    };
    let state = crate::config::cache::RepositoryConfigCache::shared()
        .load(gh, key, &repo.name)
        .await;
    let bridge = state
        .snapshot()
        .map_err(Clone::clone)?
        .config
        .forgejo_bridge
        .as_ref()
        .map_err(Clone::clone)?;
    if !bridge.enabled || !bridge.sync_refs {
        return Err("Forgejo fallback requires the explicit ordinary-repository bridge".into());
    }
    let name = bridge
        .destination
        .as_deref()
        .ok_or("Forgejo destination missing")?;
    let fj = crate::forgejo::ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)?;
    crate::forgejo::bridge::authorize_forgejo(&fj, cfg, name, &repo.name, repo.id, bridge)
        .await
        .map_err(|e| e.to_string())?;
    let local = fj.repository_config(name).await?.rollup?;
    if !local.enabled
        || local.reviewers != p.reviewers
        || local.forgejo_required_checks != p.forgejo_required_checks
    {
        return Err("Forgejo fallback policy must agree on both repositories".into());
    }
    Ok((fj, name.into()))
}
/// Forgejo 17's Actions API exposes exact commit identity and run jobs. Require
/// the newest push run of every configured workflow, with every job successful.
pub(crate) async fn forgejo_ci(
    fj: &crate::forgejo::ForgejoClient,
    repo: &str,
    batch: &Batch,
    required: &[String],
) -> Result<Verdict> {
    let mut pending = false;
    for workflow in required {
        let route = format!(
            "/repos/{repo}/actions/runs?head_sha={}&ref={}&workflow_id={}&event=push&limit=50",
            batch.tip,
            crate::github::enc_seg(&format!("refs/heads/{}", batch.branch)),
            crate::github::enc_seg(workflow)
        );
        let v = fj.get(&route).await?;
        let runs = v["workflow_runs"]
            .as_array()
            .ok_or("Forgejo run list missing")?;
        if v["total_count"].as_u64().unwrap_or(u64::MAX) > 50 {
            return Err("too many Forgejo runs to verify".into());
        }
        let run = runs
            .iter()
            .filter(|r| {
                r["commit_sha"] == batch.tip
                    && r["workflow_id"] == *workflow
                    && r["event"] == "push"
            })
            .max_by_key(|r| r["id"].as_i64().unwrap_or(0));
        let Some(run) = run else {
            pending = true;
            continue;
        };
        match run["status"].as_str() {
            Some("success") => {
                let id = run["id"].as_i64().ok_or("Forgejo run id missing")?;
                let jobs = fj
                    .get(&format!("/repos/{repo}/actions/runs/{id}/jobs"))
                    .await?;
                let jobs = jobs
                    .as_array()
                    .or_else(|| jobs["jobs"].as_array())
                    .ok_or("Forgejo jobs missing")?;
                if jobs.is_empty()
                    || jobs
                        .iter()
                        .any(|j| j["status"] != "success" || j["run_id"] != id)
                {
                    return Ok(Verdict::Failed);
                }
            }
            Some("failure" | "cancelled" | "skipped") => return Ok(Verdict::Failed),
            _ => pending = true,
        }
    }
    Ok(if pending || required.is_empty() {
        Verdict::Pending
    } else {
        Verdict::Green
    })
}
fn label(v: &Verdict) -> String {
    match v {
        Verdict::Green => "passed",
        Verdict::Failed => "failed",
        Verdict::Pending => "pending",
    }
    .into()
}

/// One running batch per repository. A split retains both halves and preserves
/// order; every later half is rebuilt on the main that actually landed.
pub(crate) fn finish(repo: &mut Repository, id: u64, outcome: &str, detail: &str) {
    let b = repo
        .batches
        .iter_mut()
        .find(|b| b.id == id)
        .expect("active batch");
    b.state = outcome.into();
    b.detail = detail.into();
    b.finished = Some(now());
    let members = b.members.clone();
    let tries = b.try_only;
    if outcome == "failed" && members.len() > 1 && !tries {
        let mid = members.len() / 2;
        repo.split
            .splice(0..0, [members[..mid].to_vec(), members[mid..].to_vec()]);
    }
    for key in &members {
        if let Some(e) = repo
            .entries
            .iter_mut()
            .find(|e| &e.key == key && e.batch == Some(id))
        {
            if e.state != "testing" {
                continue;
            }
            e.state = match outcome {
                "merged" => "merged",
                "passed" => "passed",
                "failed" if members.len() == 1 || tries => "failed",
                "paused" => "failed",
                _ => "queued",
            }
            .into();
            e.detail = detail.into();
        }
    }
    repo.active = None;
}
async fn audit(
    gh: &Client,
    cfg: &Config,
    store: &Store,
    repo: &mut Repository,
    id: u64,
) -> Result<()> {
    let b = repo.batches.iter().find(|b| b.id == id).unwrap().clone();
    if b.audit_pr.is_some() {
        return Ok(());
    }
    let owner = repo.name.split('/').next().ok_or("owner missing")?;
    let prs = gh
        .get_all(&format!(
            "/repos/{}/pulls?state=all&head={}&per_page=100",
            repo.name,
            crate::github::enc_seg(&format!("{owner}:{}", b.branch))
        ))
        .await?;
    if let Some(pr) = prs
        .iter()
        .find(|v| v["head"]["ref"] == b.branch && v["base"]["ref"] == b.base_branch)
    {
        let n = pr["number"].as_i64().ok_or("audit PR number missing")?;
        repo.batches
            .iter_mut()
            .find(|b| b.id == id)
            .unwrap()
            .audit_pr = Some(n);
        store.rollup_save(repo)?;
        return Ok(());
    }
    if b.audit_attempted {
        return Err("audit PR creation outcome unknown; waiting for remote reconciliation".into());
    }
    repo.batches
        .iter_mut()
        .find(|b| b.id == id)
        .unwrap()
        .audit_attempted = true;
    store.rollup_save(repo)?;
    let members = b
        .approvals
        .iter()
        .map(|e| {
            format!(
                "- #{} · `{}` · approved by @{} via {}",
                e.number, e.head, e.approved_by, e.origin.provider
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let queue_link = if repo.public {
        format!("[Live queue]({})", ingress::queue_url(cfg, &repo.name))
    } else {
        "This project is not publicly listed. Use the queue command for status.".into()
    };
    let body=format!("<!-- xero-rollup:{} -->\n\n{members}\n\nBase: `{}`\nCandidate: `{}`\n\nGitHub → Forgejo fallback. One complete provider must pass on this exact candidate.\n\n{queue_link}\n\nThis PR is an audit record; the queue advances the exact tested commit.",b.id,b.base,b.tip);
    let result = gh
        .create_pr(
            &repo.name,
            &b.branch,
            &b.base_branch,
            &format!(
                "Rollup #{} · {} PRs{}",
                b.id,
                b.members.len(),
                if b.try_only { " · try only" } else { "" }
            ),
            &body,
        )
        .await;
    let result = match result {
        Ok(value) => value,
        Err(error) => {
            // A definitive refusal created nothing. Transport/5xx/timeouts
            // remain unknown and are reconciled by listing before any retry.
            if matches!(
                error,
                GhError::Api {
                    status: 400..=499,
                    ..
                }
            ) && !matches!(error, GhError::Api { status: 408, .. })
            {
                repo.batches
                    .iter_mut()
                    .find(|b| b.id == id)
                    .unwrap()
                    .audit_attempted = false;
                store.rollup_save(repo)?;
            }
            return Err(error.into());
        }
    };
    repo.batches
        .iter_mut()
        .find(|b| b.id == id)
        .unwrap()
        .audit_pr = result["number"].as_i64();
    store.rollup_save(repo)?;
    Ok(())
}
async fn drive(gh: &Client, cfg: &Config, store: &Store, repo: &mut Repository) -> Result<()> {
    // Hide public snapshots until both opt-in and current visibility are reverified.
    repo.public = false;
    store.rollup_save(repo)?;
    let p = ingress::policy(gh, repo).await?;
    let metadata = gh.repo_info(&repo.name).await?;
    if metadata["id"] != repo.id || metadata["archived"] == true {
        return Err("repository identity changed or archived".into());
    }
    repo.public = p.public && metadata["private"] == false;
    repo.checked = now();
    let main = metadata["default_branch"]
        .as_str()
        .ok_or("default branch missing")?
        .to_string();
    let base = gh.get_branch_ref(&repo.name, &main).await?["object"]["sha"]
        .as_str()
        .ok_or("main SHA missing")?
        .to_string();
    if let Some(id) = repo.active {
        let b = repo
            .batches
            .iter()
            .find(|b| b.id == id)
            .ok_or("batch missing")?
            .clone();
        // A restart after successful advance must not merge or publish success twice.
        if b.state == "advancing" && base == b.tip {
            finish(repo, id, "merged", "候选提交已合入主分支");
            return Ok(());
        }
        if b.base != base {
            finish(repo, id, "cancelled", "main 已前进，基于新主分支重建测试");
            return Ok(());
        }
        for (key, head) in b.members.iter().zip(&b.heads) {
            let e = repo.entry(key).ok_or("batch member missing")?;
            if e.state != "testing" || e.head != *head || e.batch != Some(id) {
                finish(repo, id, "cancelled", "批准或成员已变化，重新组批");
                return Ok(());
            }
            if let Err(error) = ingress::validate_member(gh, cfg, store, repo, &p, e).await {
                let message = error.to_string();
                if message.contains("stale") || message.contains("PR must be open") {
                    repo.entries
                        .iter_mut()
                        .find(|e| e.key == *key)
                        .unwrap()
                        .state = "stale".into();
                    finish(
                        repo,
                        id,
                        "cancelled",
                        "提交已变化或 PR 已关闭，等待重新批准",
                    );
                    return Ok(());
                }
                return Err(error);
            }
        }
        if b.state == "building" {
            finish(repo, id, "cancelled", "恢复中断的候选构建");
            return Ok(());
        }
        let reference = match gh.get_branch_ref(&repo.name, &b.branch).await {
            Ok(v) => v,
            Err(GhError::Api { status: 404, .. }) if b.state == "publishing" => {
                finish(repo, id, "cancelled", "候选尚未发布，重新构建");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if reference["object"]["sha"] != b.tip {
            finish(repo, id, "cancelled", "测试分支发生变化，原结果作废");
            return Ok(());
        }
        repo.batches.iter_mut().find(|b| b.id == id).unwrap().state = "testing".into();
        audit(gh, cfg, store, repo, id).await?;
        if let Some(n) = repo.batches.iter().find(|b| b.id == id).unwrap().audit_pr {
            let pr = gh.get_pr(&repo.name, n).await?;
            if pr["state"] != "open" {
                for e in &mut repo.entries {
                    if e.batch == Some(id) && e.state == "testing" {
                        e.state = "cancelled".into();
                    }
                }
                finish(repo, id, "cancelled", "批次审计 PR 已关闭；需重新批准");
                return Ok(());
            }
        }
        let github = github_ci(gh, &repo.name, &b.tip, &b.branch, &p.required_checks).await;
        let mut passed = passed_provider(github.as_ref().ok(), None);
        let github_failed = matches!(github, Ok(Verdict::Failed));
        repo.batches
            .iter_mut()
            .find(|b| b.id == id)
            .unwrap()
            .github_ci = github
            .as_ref()
            .map(label)
            .unwrap_or_else(|_| "unavailable".into());
        let fallback = github_failed
            || github.is_err()
            || now() - b.created >= p.fallback_after_seconds
            || b.fallback_started.is_some();
        if passed.is_none() && fallback {
            let (fj, destination) = forgejo_target(gh, cfg, repo, &p).await?;
            if b.fallback_started.is_none() {
                // Save the handoff before sending. Repeating this exact non-force
                // ref push is idempotent and cannot publish another candidate.
                repo.batches
                    .iter_mut()
                    .find(|b| b.id == id)
                    .unwrap()
                    .forgejo_ci = "publishing".into();
                store.rollup_save(repo)?;
                git::fallback(gh, cfg, repo, &b, &destination).await?;
                repo.batches
                    .iter_mut()
                    .find(|b| b.id == id)
                    .unwrap()
                    .fallback_started = Some(now());
                store.rollup_save(repo)?;
            }
            let branch = fj
                .get(&format!(
                    "/repos/{destination}/branches/{}",
                    crate::github::enc_seg(&b.branch)
                ))
                .await?;
            if branch["commit"]["id"] != b.tip {
                return Err("Forgejo candidate SHA mismatch".into());
            }
            let verdict = forgejo_ci(&fj, &destination, &b, &p.forgejo_required_checks).await?;
            repo.batches
                .iter_mut()
                .find(|b| b.id == id)
                .unwrap()
                .forgejo_ci = label(&verdict);
            if verdict == Verdict::Green {
                passed = passed_provider(github.as_ref().ok(), Some(&verdict));
            } else if github_failed && verdict == Verdict::Failed {
                finish(repo, id, "failed", "两侧完整 CI 均失败，拆分批次定位");
                return Ok(());
            }
        }
        if let Some(provider) = passed {
            repo.batches
                .iter_mut()
                .find(|b| b.id == id)
                .unwrap()
                .passed_on = Some(provider.into());
            if b.try_only {
                finish(repo, id, "passed", "试跑通过；不会合并");
                return Ok(());
            }
            // Recheck live heads, permissions and withdrawals after slow CI API reads.
            for key in &b.members {
                ingress::validate_member(gh, cfg, store, repo, &p, repo.entry(key).unwrap())
                    .await?;
            }
            let current = gh.get_branch_ref(&repo.name, &main).await?;
            if current["object"]["sha"] != b.base {
                finish(repo, id, "cancelled", "main 已更新，重新测试");
                return Ok(());
            }
            // Branch protection should require this App's aggregate gate, so a
            // failed GitHub CI does not mask a complete Forgejo CI success.
            gh.post(&format!("/repos/{}/statuses/{}",repo.name,b.tip),Some(json!({"state":"success","context":"xero/rollup","description":format!("Approved candidate passed on {provider}"),"target_url":ingress::queue_url(cfg, &repo.name)}))).await?;
            repo.batches.iter_mut().find(|b| b.id == id).unwrap().state = "advancing".into();
            store.rollup_save(repo)?;
            gh.update_branch_ref(&repo.name, &main, &b.tip, false)
                .await?;
            finish(
                repo,
                id,
                "merged",
                &format!("{provider} 完整测试通过，已合入主分支"),
            );
            return Ok(());
        }
        let fallback_time = repo
            .batches
            .iter()
            .find(|b| b.id == id)
            .unwrap()
            .fallback_started
            .unwrap_or(b.created);
        if now() - fallback_time > p.timeout_seconds {
            finish(repo, id, "paused", "测试超时；可对同一批准提交 retry");
        }
        return Ok(());
    }
    if !gh
        .list_prs_with_label(&repo.name, &cfg.label_merge_queue_testing)
        .await?
        .is_empty()
    {
        return Err(
            "legacy queue still has a testing batch; drain it before enabling rollup".into(),
        );
    }
    // Resolve withdrawals too: a delayed bridge mapping must not let a second
    // origin's older approval survive a newer cancellation.
    for entry in &mut repo.entries {
        if entry.number == 0 && entry.origin.provider == "forgejo" && entry.state != "superseded" {
            let fj = crate::forgejo::ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)?;
            let (_, name, id, installation, peer) = crate::forgejo::bridge::rollup_binding(
                &fj,
                cfg,
                store,
                &entry.origin.repo,
                entry.origin.number,
            )
            .await?;
            if id != repo.id
                || installation != repo.installation
                || !name.eq_ignore_ascii_case(&repo.name)
            {
                return Err("repository binding changed".into());
            }
            if let Some(number) = peer {
                entry.number = number;
            }
        }
    }
    reconcile_duplicates(repo);
    // No admission from labels or web Approve: only durable, permission-checked r+.
    let mut eligible = Vec::new();
    for i in 0..repo.entries.len() {
        if !repo.entries[i].waiting() {
            continue;
        }
        let entry = repo.entries[i].clone();
        match ingress::validate_member(gh, cfg, store, repo, &p, &entry).await {
            Ok(number) => {
                repo.entries[i].number = number;
                repo.entries[i].state = "queued".into();
                repo.entries[i].detail = "等待组批".into();
                eligible.push(entry.key);
            }
            Err(error) => {
                let msg = error.to_string();
                if msg.contains("stale") || msg.contains("PR must be open") {
                    repo.entries[i].state = "stale".into();
                    repo.entries[i].detail = "提交已变化或 PR 已关闭，请重新批准".into();
                } else {
                    repo.entries[i].state = "waiting".into();
                    repo.entries[i].detail = "等待同步、权限或 API 恢复".into();
                    // Remote failures must reach the repository backoff. Keeping
                    // only an entry-level waiting state would poll an exhausted
                    // installation every 15 seconds and never increase its delay.
                    if error.downcast_ref::<GhError>().is_some()
                        || error
                            .downcast_ref::<crate::forgejo::ForgejoError>()
                            .is_some()
                        || msg.contains("API returned")
                        || msg.contains("github api returned")
                    {
                        return Err(error);
                    }
                    tracing::warn!(
                        "rollup admission {}: {}",
                        repo.name,
                        crate::redact::scrub(&msg)
                    );
                }
            }
        }
    }
    reconcile_duplicates(repo);
    eligible.retain(|k| repo.entry(k).is_some_and(Entry::waiting));
    eligible.sort_by_key(|k| repo.entry(k).unwrap().created);
    let group = select_group(repo, &eligible, &p);
    if group.is_empty() {
        return Ok(());
    }
    repo.sequence += 1;
    let id = repo.sequence;
    let tries = repo.entry(&group[0]).unwrap().try_only;
    let batch = Batch {
        id,
        members: group.clone(),
        heads: group
            .iter()
            .map(|k| repo.entry(k).unwrap().head.clone())
            .collect(),
        approvals: group
            .iter()
            .map(|k| repo.entry(k).unwrap().clone())
            .collect(),
        base,
        base_branch: main,
        branch: format!(
            "{INTERNAL_PREFIX}{}/{id}",
            if tries { "try" } else { "rollup" }
        ),
        tip: String::new(),
        state: "building".into(),
        created: now(),
        finished: None,
        detail: "构建候选提交".into(),
        try_only: tries,
        audit_pr: None,
        audit_attempted: false,
        cleaned: false,
        forgejo_recorded: false,
        github_ci: "pending".into(),
        forgejo_ci: "not_started".into(),
        fallback_started: None,
        passed_on: None,
    };
    for key in &group {
        let e = repo.entries.iter_mut().find(|e| e.key == *key).unwrap();
        e.state = "testing".into();
        e.batch = Some(id);
    }
    repo.active = Some(id);
    repo.batches.push(batch.clone());
    store.rollup_save(repo)?;
    let prepared = match git::prepare(gh, cfg, repo, &batch).await {
        Ok(v) => v,
        Err(e) => {
            if e.to_string().contains("candidate merge conflict") {
                finish(repo, id, "failed", "候选合并冲突，拆分批次定位");
            } else {
                finish(repo, id, "cancelled", "构建暂不可用，等待重试");
            }
            return Err(e);
        }
    };
    let b = repo.batches.iter_mut().find(|b| b.id == id).unwrap();
    b.tip = prepared.tip.clone();
    b.state = "publishing".into();
    store.rollup_save(repo)?;
    prepared.publish(repo, &batch.branch).await?;
    repo.batches.iter_mut().find(|b| b.id == id).unwrap().state = "testing".into();
    Ok(())
}

pub(crate) fn select_group(repo: &mut Repository, eligible: &[String], p: &Policy) -> Vec<String> {
    let splitting = !repo.split.is_empty();
    let mut candidates = if splitting {
        let requested = repo.split.remove(0);
        requested
            .into_iter()
            .filter(|k| eligible.contains(k))
            .collect::<Vec<_>>()
    } else {
        eligible.to_vec()
    };
    let Some(first) = candidates.first() else {
        return vec![];
    };
    let e = repo.entry(first).unwrap();
    if !splitting && now() - e.created < p.collect_seconds {
        return vec![];
    }
    // Split keys outlive approval and policy changes. Reapply the same limits
    // as a fresh batch, so try-only and rollup=never entries always run alone.
    let size = if !e.rollup || e.try_only {
        1
    } else {
        candidates
            .iter()
            .take_while(|k| {
                let e = repo.entry(k).unwrap();
                e.rollup && !e.try_only
            })
            .take(p.max_batch)
            .count()
    };
    let remaining = candidates.split_off(size);
    if splitting && !remaining.is_empty() {
        // Keep the rest ahead of later split halves and newly queued entries.
        repo.split.insert(0, remaining);
    }
    candidates
}
async fn cleanup(gh: &Client, cfg: &Config, store: &Store, repo: &mut Repository) -> Result<()> {
    let Some(index) = repo.batches.iter().position(|b| {
        b.finished.is_some() && (!b.cleaned || (b.state == "merged" && !b.forgejo_recorded))
    }) else {
        return Ok(());
    };
    let batch = repo.batches[index].clone();
    if !internal_branch(&batch.branch) {
        return Err("refusing to clean a non-internal branch".into());
    }
    if batch.state == "merged" && !batch.forgejo_recorded {
        let key = crate::config::cache::RepositoryKey {
            installation_id: repo.installation,
            repository_id: repo.id,
        };
        for entry in &batch.approvals {
            let (fj, name, number) =
                crate::forgejo::bridge::rollup_peer(gh, cfg, store, &repo.name, key, entry.number)
                    .await?
                    .ok_or("merged PR is missing its Forgejo binding")?;
            record_forgejo_merge(&fj, &name, number, entry, &batch).await?;
        }
        repo.batches[index].forgejo_recorded = true;
    }
    if batch.cleaned {
        return Ok(());
    }
    if let Some(number) = batch.audit_pr {
        let pr = gh.get_pr(&repo.name, number).await?;
        if pr["head"]["ref"] != batch.branch {
            return Err("audit PR head changed".into());
        }
        if pr["state"] == "open" {
            gh.patch(
                &format!("/repos/{}/pulls/{number}", repo.name),
                Some(json!({"state":"closed"})),
            )
            .await?;
        }
    }
    match gh.delete_branch_ref(&repo.name, &batch.branch).await {
        Ok(()) | Err(GhError::Api { status: 404, .. }) => (),
        Err(GhError::Api {
            status: 422,
            message,
        }) if message == "Reference does not exist" => (),
        Err(e) => return Err(e.into()),
    }
    if batch.fallback_started.is_some() {
        let p = ingress::policy(gh, repo).await?;
        let (fj, name) = forgejo_target(gh, cfg, repo, &p).await?;
        match fj
            .delete(&format!(
                "/repos/{name}/branches/{}",
                crate::github::enc_seg(&batch.branch)
            ))
            .await
        {
            Ok(_) | Err(crate::forgejo::ForgejoError::Api { status: 404, .. }) => (),
            Err(e) => return Err(e.into()),
        }
    }
    repo.batches[index].cleaned = true;
    Ok(())
}

pub async fn pump_all(cfg: &Config, store: &Store) -> Result<()> {
    if !cfg.rollup_enabled {
        return Ok(());
    }
    use futures::StreamExt;
    futures::stream::iter(store.rollup_all()?)
        .for_each_concurrent(4, |snapshot| async move {
            if let Err(error) = pump_repository(cfg, store, snapshot).await {
                tracing::warn!(
                    "rollup repository: {}",
                    crate::redact::scrub(&error.to_string())
                );
            }
        })
        .await;
    Ok(())
}
async fn pump_repository(cfg: &Config, store: &Store, snapshot: Repository) -> Result<()> {
    if snapshot.next_attempt > now() {
        return Ok(());
    }
    let _lock = lock(&snapshot.name).await;
    let mut repo = store
        .rollup_load(&snapshot.name)?
        .ok_or("queue disappeared")?;
    let result = match Client::installation_resolved(cfg, repo.installation).await {
        Ok(gh) => {
            if let Err(e) = cleanup(&gh, cfg, store, &mut repo).await {
                tracing::warn!("rollup cleanup: {}", crate::redact::scrub(&e.to_string()));
            }
            match tokio::time::timeout(
                std::time::Duration::from_secs(300),
                drive(&gh, cfg, store, &mut repo),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => Err("queue transition timed out".into()),
            }
        }
        Err(e) => Err(e.into()),
    };
    match result {
        Ok(()) => {
            repo.failures = 0;
            repo.next_attempt = 0;
            repo.detail.clear();
        }
        Err(e) => {
            repo.failures = repo.failures.saturating_add(1);
            repo.next_attempt = now() + (15 * (1_i64 << repo.failures.min(6))).min(900);
            repo.detail = "等待 API、权限或配置恢复；队列会自动重试".into();
            tracing::warn!(repo=%repo.name,error=%crate::redact::scrub(&e.to_string()),"rollup waiting");
        }
    }
    store.rollup_save(&repo)?;
    Ok(())
}

/// A delayed bridge mapping may reveal two decisions for one canonical PR.
/// Keep the newest decision, including withdrawals, and retire the other entry.
pub(crate) fn reconcile_duplicates(repo: &mut Repository) {
    let mut newest: BTreeMap<i64, usize> = BTreeMap::new();
    for i in 0..repo.entries.len() {
        let e = &repo.entries[i];
        if e.number <= 0 || e.state == "superseded" {
            continue;
        }
        if let Some(&previous) = newest.get(&e.number) {
            let old = &repo.entries[previous];
            let replace = (e.decision_time, e.state == "cancelled", e.decision_comment)
                > (
                    old.decision_time,
                    old.state == "cancelled",
                    old.decision_comment,
                );
            let retired = if replace { previous } else { i };
            if replace {
                newest.insert(e.number, i);
            }
            repo.entries[retired].state = "superseded".into();
        } else {
            newest.insert(e.number, i);
        }
    }
}

/// Code was already fast-forwarded on GitHub and copied by the bridge. Record
/// that fact without creating another merge commit. Forgejo's native manual
/// merge endpoint verifies MergeCommitID is contained in the PR's base branch.
pub(crate) async fn record_forgejo_merge(
    fj: &crate::forgejo::ForgejoClient,
    repo: &str,
    number: i64,
    entry: &Entry,
    batch: &Batch,
) -> Result<()> {
    let pr = fj.get(&format!("/repos/{repo}/pulls/{number}")).await?;
    if pr["merged"] == true {
        return Ok(());
    }
    if pr["head"]["sha"] != entry.head || pr["base"]["ref"] != batch.base_branch {
        return Err(
            "Forgejo PR changed after integration; refusing to mark another revision merged".into(),
        );
    }
    fj.post(
        &format!("/repos/{repo}/pulls/{number}/merge"),
        json!({
            "Do":"manually-merged", "MergeCommitID":batch.tip, "head_commit_id":entry.head,
            "force_merge":false, "delete_branch_after_merge":false
        }),
    )
    .await?;
    Ok(())
}
