use super::*;
use crate::{
    commands::Command,
    config::{
        cache::{RepositoryConfigCache, RepositoryKey},
        Config,
    },
    forgejo::ForgejoClient,
    github::Client,
    trigger_state::Store,
};
use serde_json::Value;

pub fn controls(command: &Command) -> bool {
    matches!(
        command,
        Command::Approve { .. }
            | Command::ApproveRollup { .. }
            | Command::Reject
            | Command::Rollup { .. }
            | Command::Try
            | Command::Retry
            | Command::Queue
    )
}
pub fn approval(command: &Command) -> bool {
    matches!(
        command,
        Command::Approve { .. } | Command::ApproveRollup { .. }
    )
}
fn credited<'a>(command: &'a Command, actor: &'a str) -> &'a str {
    match command {
        Command::Approve { on_behalf_of } | Command::ApproveRollup { on_behalf_of, .. } => {
            on_behalf_of.as_deref().unwrap_or(actor)
        }
        _ => actor,
    }
}
pub async fn enabled_for(gh: &Client, cfg: &Config, name: &str, installation: i64) -> Result<bool> {
    if !cfg.rollup_enabled {
        return Ok(false);
    }
    let meta = gh.repo_info(name).await?;
    let id = meta["id"].as_i64().ok_or("repository id unavailable")?;
    let state = RepositoryConfigCache::shared()
        .load(
            gh,
            RepositoryKey {
                installation_id: installation,
                repository_id: id,
            },
            name,
        )
        .await;
    Ok(state
        .snapshot()
        .map_err(Clone::clone)?
        .config
        .rollup
        .as_ref()
        .map_err(Clone::clone)?
        .enabled)
}

pub async fn policy(gh: &Client, repo: &Repository) -> Result<Policy> {
    let state = RepositoryConfigCache::shared()
        .load(
            gh,
            RepositoryKey {
                installation_id: repo.installation,
                repository_id: repo.id,
            },
            &repo.name,
        )
        .await;
    let p = state
        .snapshot()
        .map_err(Clone::clone)?
        .config
        .rollup
        .as_ref()
        .map_err(Clone::clone)?
        .clone();
    if !p.enabled {
        return Err("rollup is disabled for this repository".into());
    }
    let bridge = state
        .snapshot()
        .map_err(Clone::clone)?
        .config
        .forgejo_bridge
        .as_ref()
        .map_err(Clone::clone)?;
    if !bridge.enabled || !bridge.sync_refs {
        return Err("rollup requires an explicitly bound ordinary-repository bridge".into());
    }
    Ok(p)
}
pub async fn write(gh: &Client, repo: &str, login: &str) -> Result<()> {
    if !matches!(
        gh.collaborator_permission(repo, login).await?.as_str(),
        "write" | "maintain" | "admin"
    ) {
        return Err("write permission is required".into());
    }
    Ok(())
}
pub async fn forgejo_write(fj: &ForgejoClient, repo: &str, login: &str) -> Result<()> {
    let p = fj
        .get(&format!(
            "/repos/{repo}/collaborators/{}/permission",
            crate::github::enc_seg(login)
        ))
        .await?;
    if !matches!(
        p["permission"]
            .as_str()
            .or(p["role_name"].as_str())
            .unwrap_or(""),
        "write" | "maintain" | "admin" | "owner" | "push"
    ) {
        return Err("Forgejo write permission is required".into());
    }
    Ok(())
}
pub fn pr_head(pr: &Value) -> Result<String> {
    if pr["state"] != "open" || pr["draft"] == true || pr["merged"] == true {
        return Err("PR must be open and ready for review".into());
    }
    let h = pr["head"]["sha"]
        .as_str()
        .filter(|s| matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("PR has no valid head SHA")?;
    if internal_branch(pr["head"]["ref"].as_str().unwrap_or("")) {
        return Err("internal rollup PRs cannot enter the queue".into());
    }
    Ok(h.into())
}
fn not_author(pr: &Value, users: &[&str]) -> Result<()> {
    let author = pr["user"]["login"]
        .as_str()
        .ok_or("PR author unavailable")?;
    if users.iter().any(|u| u.eq_ignore_ascii_case(author)) {
        return Err("PR authors cannot approve their own changes".into());
    }
    Ok(())
}
fn map_user<'a>(p: &'a Policy, user: &str) -> Result<&'a str> {
    p.reviewers
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(user))
        .map(|(_, b)| b.as_str())
        .ok_or_else(|| "an explicit Forgejo-to-GitHub reviewer mapping is required".into())
}

/// Local queue mutation only, after all remote authorization has succeeded.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(
    repo: &mut Repository,
    command: &Command,
    origin: Origin,
    number: i64,
    head: String,
    approver: String,
    command_key: String,
    source_time: i64,
    comment_id: i64,
) -> Result<String> {
    if repo.commands.contains(&command_key) {
        return Ok("此指令已记录，无需重复入队。".into());
    }
    let index = repo.entries.iter().position(|e| {
        e.state != "superseded"
            && ((number > 0 && e.number == number)
                || (e.origin.provider == origin.provider
                    && e.origin.repo.eq_ignore_ascii_case(&origin.repo)
                    && e.origin.number == origin.number))
    });
    if let Some(i) = index {
        let previous = &repo.entries[i];
        if source_time < previous.decision_time
            || (source_time == previous.decision_time
                && ((previous.decision_provider == origin.provider
                    && comment_id <= previous.decision_comment)
                    || (previous.state == "cancelled"
                        && previous.decision_provider != origin.provider)))
        {
            return Err("a newer queue decision already supersedes this command".into());
        }
    }
    let decision_provider = origin.provider.clone();
    match command {
        Command::Approve { .. }
        | Command::ApproveRollup { .. }
        | Command::Try
        | Command::Reject
            if index.is_none() || !matches!(command, Command::Reject) =>
        {
            if index
                .is_some_and(|i| repo.entries[i].state == "testing" && repo.entries[i].head == head)
            {
                return Ok("当前提交已在测试中；批次成员保持固定。".into());
            }
            let rollup = match command {
                Command::ApproveRollup { enabled, .. } => *enabled,
                _ => index.is_some_and(|i| repo.entries[i].rollup),
            };
            let entry = Entry {
                key: index
                    .map(|i| repo.entries[i].key.clone())
                    .unwrap_or_else(|| {
                        format!(
                            "{}:{}:{}",
                            origin.provider,
                            origin.repo.to_ascii_lowercase(),
                            origin.number
                        )
                    }),
                number,
                head,
                title: String::new(),
                approved_by: approver,
                origin,
                rollup,
                try_only: matches!(command, Command::Try),
                created: source_time,
                decision_time: source_time,
                decision_comment: comment_id,
                decision_provider: decision_provider.clone(),
                state: if matches!(command, Command::Reject) {
                    "cancelled"
                } else if number == 0 {
                    "waiting"
                } else {
                    "queued"
                }
                .into(),
                detail: if number == 0 {
                    "等待 PR 双向同步"
                } else {
                    "等待组批"
                }
                .into(),
                batch: None,
            };
            match index {
                Some(i) => repo.entries[i] = entry,
                None => repo.entries.push(entry),
            }
        }
        Command::Reject => {
            let e = index
                .map(|i| &mut repo.entries[i])
                .ok_or("PR is not in the queue")?;
            e.state = "cancelled".into();
            e.detail = "合并授权已撤销".into();
            e.created = source_time;
        }
        Command::Rollup { enabled } => {
            let e = index
                .map(|i| &mut repo.entries[i])
                .ok_or("approve with r+ before changing the batching policy")?;
            if e.state == "testing" {
                return Err("use r- before changing a running batch".into());
            }
            e.rollup = *enabled;
        }
        Command::Retry => {
            let e = index
                .map(|i| &mut repo.entries[i])
                .ok_or("no approved revision to retry")?;
            if e.head != head || matches!(e.state.as_str(), "cancelled" | "stale" | "merged") {
                return Err(
                    "approval is no longer valid; review the current revision and use r+".into(),
                );
            }
            if e.state != "testing" {
                e.state = "queued".into();
                e.batch = None;
                e.detail = "等待重试".into();
            }
            repo.next_attempt = 0;
        }
        _ => return Err("unsupported queue operation".into()),
    }
    let index = index.unwrap_or(repo.entries.len() - 1);
    let e = &mut repo.entries[index];
    e.decision_time = source_time;
    e.decision_comment = comment_id;
    e.decision_provider = decision_provider;
    repo.remember(command_key);
    Ok("已记录队列操作。批准仅对当前提交有效；新提交需要重新 r+。".into())
}

pub async fn github(
    gh: &Client,
    cfg: &Config,
    ctx: &crate::handlers::CommentContext,
    command: &Command,
) -> Result<String> {
    let (store, event) = crate::trigger_state::runtime::rollup_context()
        .ok_or("durable command context is required")?;
    let _lock = lock(&ctx.repo).await;
    let metadata = gh.repo_info(&ctx.repo).await?;
    let id = metadata["id"]
        .as_i64()
        .ok_or("repository identity unavailable")?;
    let mut repo = store
        .rollup_load(&ctx.repo)?
        .unwrap_or_else(|| Repository::new(&ctx.repo, id, ctx.installation_id));
    if repo.id != id || repo.installation != ctx.installation_id {
        return Err("repository binding changed".into());
    }
    let p = policy(gh, &repo).await?;
    if matches!(command, Command::Queue) {
        repo.public = p.public && metadata["private"] == false;
        repo.checked = now();
        store.rollup_save(&repo)?;
        return Ok(status(&repo, cfg));
    }
    write(gh, &ctx.repo, &ctx.commenter).await?;
    let credit = credited(command, &ctx.commenter);
    write(gh, &ctx.repo, credit).await?;
    let pr = gh.get_pr(&ctx.repo, ctx.pr_number).await?;
    let head = pr_head(&pr)?;
    if approval(command) {
        not_author(&pr, &[&ctx.commenter, credit])?;
    }
    let source_time = chrono::DateTime::parse_from_rfc3339(&event.source_time)?.timestamp();
    let key = crate::trigger_state::manual_key(
        id,
        event.comment_id.ok_or("comment identity unavailable")?,
        command,
    );
    let origin = Origin {
        provider: "github".into(),
        repo: ctx.repo.clone(),
        number: ctx.pr_number,
        login: ctx.commenter.clone(),
    };
    let message = apply(
        &mut repo,
        command,
        origin,
        ctx.pr_number,
        head,
        credit.into(),
        key,
        source_time,
        event.comment_id.ok_or("comment identity unavailable")?,
    )?;
    if let Some(e) = repo.entries.iter_mut().find(|e| e.number == ctx.pr_number) {
        e.title = pr["title"].as_str().unwrap_or("").into();
    }
    repo.public = p.public && metadata["private"] == false;
    repo.checked = now();
    store.rollup_save(&repo)?;
    Ok(format!("{message}\n\n{}", status(&repo, cfg)))
}

#[allow(clippy::too_many_arguments)]
pub async fn forgejo(
    fj: &ForgejoClient,
    cfg: &Config,
    store: &Store,
    name: &str,
    number: i64,
    actor: &str,
    comment: i64,
    source_time: &str,
    command: &Command,
) -> Result<String> {
    let (gh, name_gh, id, installation, peer) =
        crate::forgejo::bridge::rollup_binding(fj, cfg, store, name, number).await?;
    let _lock = lock(&name_gh).await;
    let mut repo = store
        .rollup_load(&name_gh)?
        .unwrap_or_else(|| Repository::new(&name_gh, id, installation));
    let p = policy(&gh, &repo).await?;
    let local = fj.repository_config(name).await?.rollup?;
    if !local.enabled || local.reviewers != p.reviewers {
        return Err("both repositories must enable rollup with identical reviewer mappings".into());
    }
    if matches!(command, Command::Queue) {
        return Ok(status(&repo, cfg));
    }
    forgejo_write(fj, name, actor).await?;
    let credit = credited(command, actor);
    forgejo_write(fj, name, credit).await?;
    let mapped_actor = map_user(&p, actor)?;
    let mapped_credit = map_user(&p, credit)?;
    write(&gh, &name_gh, mapped_actor).await?;
    write(&gh, &name_gh, mapped_credit).await?;
    let pr = fj.get(&format!("/repos/{name}/pulls/{number}")).await?;
    let head = pr_head(&pr)?;
    if approval(command) {
        not_author(&pr, &[actor, credit])?;
        if let Some(n) = peer {
            not_author(
                &gh.get_pr(&name_gh, n).await?,
                &[mapped_actor, mapped_credit],
            )?;
        }
    }
    if comment <= 0 {
        return Err("source comment identity is required".into());
    }
    let source_time = chrono::DateTime::parse_from_rfc3339(source_time)?.timestamp();
    let key = format!("forgejo:{name}:{comment}:{command:?}");
    let origin = Origin {
        provider: "forgejo".into(),
        repo: name.into(),
        number,
        login: actor.into(),
    };
    let message = apply(
        &mut repo,
        command,
        origin,
        peer.unwrap_or(0),
        head,
        mapped_credit.into(),
        key,
        source_time,
        comment,
    )?;
    if let Some(e) = repo.entries.iter_mut().find(|e| {
        e.origin.provider == "forgejo"
            && e.origin.repo.eq_ignore_ascii_case(name)
            && e.origin.number == number
    }) {
        e.title = pr["title"].as_str().unwrap_or("").into();
    }
    store.rollup_save(&repo)?;
    Ok(format!("{message}\n\n{}", status(&repo, cfg)))
}
pub(crate) fn queue_url(cfg: &Config, repo: &str) -> String {
    format!(
        "{}/queue/{}",
        cfg.public_url.trim_end_matches('/'),
        crate::github::enc_path(repo)
    )
}
fn status(repo: &Repository, cfg: &Config) -> String {
    let waiting = repo.entries.iter().filter(|e| e.waiting()).count();
    let testing = repo.entries.iter().filter(|e| e.state == "testing").count();
    let page = if repo.public {
        format!("[查看 Rollup 队列]({})", queue_url(cfg, &repo.name))
    } else {
        "此项目的队列未公开展示；使用 queue 查询状态。".into()
    };
    format!("排队 {waiting} · 测试 {testing}\n{page}")
}

/// Revalidate BOTH sources immediately before starting and before advancing.
pub async fn validate_member(
    gh: &Client,
    cfg: &Config,
    store: &Store,
    repo: &Repository,
    p: &Policy,
    entry: &Entry,
) -> Result<i64> {
    write(gh, &repo.name, &entry.approved_by).await?;
    let number = if entry.origin.provider == "forgejo" {
        let fj = ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)?;
        let (_, name, id, installation, n) = crate::forgejo::bridge::rollup_binding(
            &fj,
            cfg,
            store,
            &entry.origin.repo,
            entry.origin.number,
        )
        .await?;
        if !name.eq_ignore_ascii_case(&repo.name)
            || id != repo.id
            || installation != repo.installation
        {
            return Err("repository binding changed".into());
        }
        n.ok_or("waiting for paired GitHub PR")?
    } else {
        entry.number
    };
    if entry.origin.provider == "forgejo" {
        let fj = ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)?;
        let source = fj
            .get(&format!(
                "/repos/{}/pulls/{}",
                entry.origin.repo, entry.origin.number
            ))
            .await?;
        if pr_head(&source)? != entry.head {
            return Err("source head changed: approval is stale".into());
        }
    }
    let pr = gh.get_pr(&repo.name, number).await?;
    if pr_head(&pr)? != entry.head {
        return Err(if entry.origin.provider == "forgejo" {
            "waiting for paired head synchronization"
        } else {
            "head changed: approval is stale"
        }
        .into());
    }
    if !entry.try_only {
        for login in blocking_reviewers(
            &gh.list_pr_reviews(&repo.name, number).await?,
            entry.created,
        ) {
            match write(gh, &repo.name, &login).await {
                Ok(()) => return Err("review requested changes: approval is stale".into()),
                Err(error) if error.to_string() == "write permission is required" => (),
                Err(error) => return Err(error),
            }
        }
    }
    let metadata = gh.repo_info(&repo.name).await?;
    if metadata["id"] != repo.id || pr["base"]["ref"] != metadata["default_branch"] {
        return Err("target repository/base changed".into());
    }
    if !entry.try_only {
        not_author(&pr, &[&entry.approved_by])?;
    }
    if entry.origin.provider == "github" {
        write(gh, &repo.name, &entry.origin.login).await?;
        if !entry.try_only {
            not_author(&pr, &[&entry.origin.login])?;
        }
    }
    let key = RepositoryKey {
        installation_id: repo.installation,
        repository_id: repo.id,
    };
    if let Some((fj, name, n)) =
        crate::forgejo::bridge::rollup_peer(gh, cfg, store, &repo.name, key, number).await?
    {
        let local = fj.repository_config(&name).await?.rollup?;
        if !local.enabled || local.reviewers != p.reviewers {
            return Err("bilateral rollup policy mismatch".into());
        }
        let local_credit = p
            .reviewers
            .iter()
            .find(|(_, b)| b.eq_ignore_ascii_case(&entry.approved_by))
            .map(|(a, _)| a.as_str())
            .ok_or("reviewer mapping missing")?;
        let actor = if entry.origin.provider == "forgejo" {
            entry.origin.login.as_str()
        } else {
            p.reviewers
                .iter()
                .find(|(_, b)| b.eq_ignore_ascii_case(&entry.origin.login))
                .map(|(a, _)| a.as_str())
                .ok_or("actor mapping missing")?
        };
        forgejo_write(&fj, &name, local_credit).await?;
        forgejo_write(&fj, &name, actor).await?;
        write(gh, &repo.name, map_user(p, actor)?).await?;
        let paired = fj.get(&format!("/repos/{name}/pulls/{n}")).await?;
        if pr_head(&paired)? != entry.head {
            return Err("waiting for paired head synchronization".into());
        }
        if !entry.try_only {
            not_author(&paired, &[local_credit, actor])?;
            let reviews = fj
                .get_all(&format!("/repos/{name}/pulls/{n}/reviews"), 50)
                .await?;
            for login in blocking_reviewers(&reviews, entry.created) {
                match forgejo_write(&fj, &name, &login).await {
                    Ok(()) => return Err("review requested changes: approval is stale".into()),
                    Err(error) if error.to_string() == "Forgejo write permission is required" => (),
                    Err(error) => return Err(error),
                }
            }
        }
        if paired["base"]["ref"].as_str() != pr["base"]["ref"].as_str() {
            return Err("paired PR base mismatch".into());
        }
    }
    Ok(number)
}

/// Latest effective review per reviewer; comments cannot erase a requested change.
pub(crate) fn blocking_reviewers(reviews: &[Value], approved: i64) -> Vec<String> {
    let mut latest: BTreeMap<String, (i64, i64, bool)> = BTreeMap::new();
    for review in reviews {
        let state = review["state"].as_str().unwrap_or("").to_ascii_uppercase();
        let blocks = matches!(
            state.as_str(),
            "CHANGES_REQUESTED" | "REQUEST_CHANGES" | "REJECTED"
        );
        if !blocks && !matches!(state.as_str(), "APPROVED" | "DISMISSED") {
            continue;
        }
        let Some(user) = review["user"]["login"].as_str() else {
            continue;
        };
        let time = review["submitted_at"]
            .as_str()
            .or(review["submitted"].as_str())
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp())
            .unwrap_or(i64::MAX);
        let id = review["id"].as_i64().unwrap_or(0);
        let key = user.to_ascii_lowercase();
        if latest.get(&key).is_none_or(|(_, old, _)| id > *old) {
            latest.insert(key, (time, id, blocks && review["dismissed"] != true));
        }
    }
    latest
        .into_iter()
        .filter(|(_, (time, _, blocks))| *blocks && *time >= approved)
        .map(|(user, _)| user)
        .collect()
}
