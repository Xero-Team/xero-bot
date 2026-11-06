//! Forgejo comment-command execution.
//!
//! The command language is shared with GitHub, but the provider operations are
//! not: Forgejo assigns through `PATCH /issues/{index}`, uses `limit` pagination,
//! and has no GitHub Checks, CodeQL, or merge-queue equivalents.

use serde_json::json;
use sha2::{Digest, Sha256};

use super::{ForgejoClient, ForgejoError, ForgejoEvent};
use crate::commands::{parse_commands, resolve_commands, Command};
use crate::config::repository::Comments;
use crate::config::Config;
use crate::lang::{detect, Lang};
use crate::trigger_state::{SessionWake, Store};

const APPROVAL_MARKER: &str = "<!-- xero-forgejo-approval -->";

#[derive(Clone)]
struct CommentJob {
    repo: String,
    number: i64,
    repository_id: Option<i64>,
    issue_id: Option<i64>,
    comment_id: Option<i64>,
    user_id: Option<i64>,
    source_at: Option<String>,
    body: String,
    commenter: String,
    author: String,
    is_pr: bool,
}

struct CommandContext<'a> {
    client: &'a ForgejoClient,
    cfg: &'a Config,
    job: &'a CommentJob,
    bot_login: &'a str,
    store: &'a Store,
    lang: Lang,
}

/// Execute a verified Forgejo issue/PR comment in the background.
pub async fn handle_issue_comment(
    cfg: &Config,
    store: &Store,
    event: ForgejoEvent,
) -> Result<(), String> {
    let ForgejoEvent::IssueComment {
        repo,
        number,
        repository_id,
        issue_id,
        comment_id,
        user_id,
        source_at,
        body,
        commenter,
        author,
        is_pr,
        ..
    } = event
    else {
        return Err("expected Forgejo issue comment".into());
    };
    let client = ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)
        .map_err(|error| error.to_string())?;
    handle_issue_comment_with_client(
        &client,
        cfg,
        store,
        CommentJob {
            repo,
            number,
            repository_id,
            issue_id,
            comment_id,
            user_id,
            source_at,
            body,
            commenter,
            author,
            is_pr,
        },
    )
    .await
}

async fn handle_issue_comment_with_client(
    client: &ForgejoClient,
    cfg: &Config,
    store: &Store,
    job: CommentJob,
) -> Result<(), String> {
    if !valid_repository(&job.repo) || job.number <= 0 {
        return Err("invalid Forgejo repository or issue number".into());
    }
    let current_user = client
        .current_user()
        .await
        .map_err(|error| format!("cannot resolve Forgejo bot identity: {error}"))?;
    let bot_login = current_user["login"]
        .as_str()
        .filter(|login| !login.is_empty())
        .ok_or("Forgejo /user response has no login")?;
    if job.commenter.eq_ignore_ascii_case(bot_login)
        || job.commenter.eq_ignore_ascii_case(&cfg.forgejo_bot_name)
        || job.commenter.to_ascii_lowercase().ends_with("[bot]")
    {
        return Ok(());
    }

    let lang = detect(&job.body).unwrap_or_default();
    let parsed = parse_commands(&cfg.forgejo_bot_name, &job.body);
    if parsed.commands.is_empty() && parsed.diagnostics.is_empty() {
        return Ok(());
    }
    let comments = match load_comments(client, &job.repo).await {
        Ok(comments) => comments,
        Err(error) => {
            let _ = reply(
                client,
                &job.repo,
                job.number,
                lang.pick(
                    "⚠️ Could not load a valid `.github/xero-bot.toml` policy for this Forgejo repository; no command was executed.",
                    "⚠️ 无法加载该 Forgejo 仓库的有效 `.github/xero-bot.toml` 策略，未执行任何指令。",
                ),
            )
            .await;
            return Err(error);
        }
    };

    let session = session_wake(cfg, &job);
    let mut session_allowed: Option<bool> = None;
    let mut wake = false;
    let mut permitted = Vec::new();
    let mut diagnostics = parsed.diagnostics;
    let mut blocked = Vec::new();

    for candidate in parsed.commands {
        let self_request = matches!(&candidate.command, Command::Assign { user } | Command::RequestReview { user }
            if crate::github::normalize_login(user) == crate::github::normalize_login(&cfg.forgejo_bot_name)
                || user.eq_ignore_ascii_case(bot_login));
        if self_request {
            diagnostics.push(crate::commands::diag::Diagnostic::SelfRequestIgnored {
                span: candidate.span.clone(),
            });
            continue;
        }
        let mut gate = comments.gate(candidate.id(), job.is_pr, candidate.is_explicit(), false);
        if matches!(&gate, Err(problem) if problem.code == crate::config::repository::ReasonCode::SessionRequired)
        {
            let open = match session.as_ref() {
                Some(call) => {
                    if session_allowed.is_none() {
                        session_allowed = Some(
                            match store.session_before_at(call, comments.ttl_days, now_ms()) {
                                Ok(wake) => wake.is_some(),
                                Err(error) => {
                                    tracing::error!(repo=%job.repo, "Forgejo session storage unavailable: {error}");
                                    let _ = reply(client, &job.repo, job.number, lang.pick(
                                    "⚠️ Session storage is unavailable; no command was executed.",
                                    "⚠️ 会话存储不可用，未执行任何指令。"
                                )).await;
                                    return Err("Forgejo session storage unavailable".into());
                                }
                            },
                        );
                    }
                    session_allowed.unwrap_or(false)
                }
                None => false,
            };
            gate = comments.gate(candidate.id(), job.is_pr, candidate.is_explicit(), open);
        }
        if let Err(problem) = gate {
            if problem.code == crate::config::repository::ReasonCode::SessionRequired
                && super::inbox::can_defer_session()
            {
                return Err("waiting for an earlier Forgejo session wake".into());
            }
            blocked.push(problem.message(lang));
            continue;
        }
        if let Err(refusal) = authorize(client, cfg, &job, &candidate.command, lang).await {
            blocked.push(refusal);
            continue;
        }
        if candidate.is_explicit()
            && !matches!(
                candidate.command,
                Command::Review | Command::Codeql | Command::Queue
            )
            && comments.mode(candidate.id()) != crate::config::repository::ManualMode::AlwaysMention
        {
            wake = true;
        }
        permitted.push(candidate);
    }

    if wake {
        if let Some(wake) = session.as_ref() {
            store
                .record_wake(wake)
                .map_err(|error| format!("Forgejo session storage unavailable: {error}"))?;
        }
    }

    let commands = resolve_commands(&cfg.forgejo_bot_name, permitted, &mut diagnostics);
    let mut message = crate::commands::diag::render(&diagnostics, &cfg.forgejo_bot_name, lang)
        .unwrap_or_default();
    if !blocked.is_empty() {
        if !message.is_empty() {
            message.push_str("\n\n");
        }
        message.push_str("⚠️ ");
        message.push_str(&blocked.join("\n- "));
    }
    if !message.is_empty() {
        let _ = reply(client, &job.repo, job.number, &message).await;
    }

    let ctx = CommandContext {
        client,
        cfg,
        job: &job,
        bot_login,
        store,
        lang,
    };
    for command in commands {
        let status = handle_one(&ctx, command).await;
        tracing::info!(
            repo = %job.repo,
            issue = job.number,
            command = %status,
            "Forgejo comment command completed"
        );
    }
    Ok(())
}

async fn load_comments(client: &ForgejoClient, repo: &str) -> Result<Comments, String> {
    client
        .repository_config(repo)
        .await?
        .comments
        .map_err(|problem| problem.to_string())
}

pub(super) fn valid_repository(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && ![owner, name]
            .into_iter()
            .any(|part| part == "." || part.contains(".."))
        && [owner, name].into_iter().all(|part| {
            part.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        })
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn session_wake(cfg: &Config, job: &CommentJob) -> Option<SessionWake> {
    let repository_id = job.repository_id.filter(|id| *id > 0)?;
    let issue_id = job.issue_id.filter(|id| *id > 0).unwrap_or(job.number);
    let user_id = job.user_id.filter(|id| *id > 0)?;
    let comment_id = job.comment_id.filter(|id| *id > 0)?;
    let source_at = job
        .source_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
        .filter(|value| *value > 0)?;
    Some(SessionWake {
        installation_id: forgejo_namespace(&cfg.forgejo_base_url),
        repository_id,
        thread_number: issue_id,
        user_id,
        comment_id,
        source_at,
    })
}

fn forgejo_namespace(base_url: &str) -> i64 {
    let canonical = reqwest::Url::parse(base_url.trim())
        .map(|url| url.to_string())
        .unwrap_or_else(|_| base_url.trim().to_string());
    let canonical = canonical.trim_end_matches('/').trim_end_matches("/api/v1");
    let digest = Sha256::digest(format!("forgejo:{canonical}").as_bytes());
    let value =
        u64::from_be_bytes(digest[..8].try_into().expect("sha256 prefix")) & i64::MAX as u64;
    value.max(1) as i64
}

async fn authorize(
    client: &ForgejoClient,
    cfg: &Config,
    job: &CommentJob,
    command: &Command,
    lang: Lang,
) -> Result<(), String> {
    if let Command::Label { add, remove } = command {
        if add.iter().chain(remove).any(|label| {
            [
                &cfg.label_merge_queue_queued,
                &cfg.label_merge_queue_testing,
            ]
            .into_iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(label))
        }) {
            return Err(lang
                .pick(
                    "⚠️ Merge-queue control labels cannot be changed with `label`.",
                    "⚠️ 不能用 `label` 修改合并队列控制标签。",
                )
                .to_string());
        }
    }
    match command {
        Command::Reject | Command::Rollup { .. } | Command::Try | Command::Retry => {
            require_write(client, &job.repo, &job.commenter, lang).await
        }
        Command::Approve { on_behalf_of } | Command::ApproveRollup { on_behalf_of, .. } => {
            if on_behalf_of.is_some() && !cfg.r_plus_allow_on_behalf {
                return Err(lang
                    .pick(
                        "⚠️ Approval on behalf of another user is disabled by deployment policy.",
                        "⚠️ 本部署未开启代他人审批。",
                    )
                    .to_string());
            }
            if job.author.trim().is_empty() {
                return Err(lang
                    .pick(
                        "⚠️ Could not verify the PR author; no approval was created.",
                        "⚠️ 无法确认 PR 作者，未创建审批。",
                    )
                    .into());
            }
            let credited = on_behalf_of.as_deref().unwrap_or(&job.commenter);
            if job.commenter.eq_ignore_ascii_case(&job.author)
                || credited.eq_ignore_ascii_case(&job.author)
            {
                return Err(lang
                    .pick(
                        "⚠️ The PR author cannot approve their own PR or be credited with an approval.",
                        "⚠️ PR 作者不能自我审批，也不能被归功审批。",
                    )
                    .to_string());
            }
            require_write(client, &job.repo, &job.commenter, lang).await?;
            if !credited.eq_ignore_ascii_case(&job.commenter) {
                require_write(client, &job.repo, credited, lang).await?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

async fn require_write(
    client: &ForgejoClient,
    repo: &str,
    user: &str,
    lang: Lang,
) -> Result<(), String> {
    let value = client
        .get(&format!(
            "/repos/{repo}/collaborators/{}/permission",
            crate::github::enc_seg(user)
        ))
        .await
        .map_err(|error| {
            format!(
                "{} ({error})",
                lang.pick(
                    "⚠️ Could not verify your Forgejo repository permission; no change was made.",
                    "⚠️ 无法确认你的 Forgejo 仓库权限，未执行修改。",
                )
            )
        })?;
    let permission = value["permission"]
        .as_str()
        .or_else(|| value["role_name"].as_str())
        .unwrap_or("");
    if matches!(
        permission.to_ascii_lowercase().as_str(),
        "write" | "maintain" | "admin" | "owner" | "push"
    ) {
        return Ok(());
    }
    Err(format!(
        "{} (@{user}: {permission})",
        lang.pick(
            "⚠️ Write access or above is required.",
            "⚠️ 需要 write 或更高权限。"
        )
    ))
}

async fn handle_one(ctx: &CommandContext<'_>, command: Command) -> &'static str {
    let durable_queue = if ctx.cfg.rollup_enabled && crate::rollup::ingress::controls(&command) {
        match ctx
            .client
            .repository_config(&ctx.job.repo)
            .await
            .and_then(|p| p.rollup.map_err(|e| e.to_string()))
        {
            Ok(p) => p.enabled,
            Err(_) => {
                let _ = reply(
                    ctx.client,
                    &ctx.job.repo,
                    ctx.job.number,
                    "⚠️ 无法验证队列配置，请稍后重试。",
                )
                .await;
                return "queue-policy-unavailable";
            }
        }
    } else {
        false
    };
    if durable_queue && crate::rollup::ingress::controls(&command) {
        let result = crate::rollup::ingress::forgejo(
            ctx.client,
            ctx.cfg,
            ctx.store,
            &ctx.job.repo,
            ctx.job.number,
            &ctx.job.commenter,
            ctx.job.comment_id.unwrap_or(0),
            ctx.job.source_at.as_deref().unwrap_or(""),
            &command,
        )
        .await;
        let (status, body) = match result {
            Ok(body) => ("rollup", body),
            Err(e) => (
                "rollup-error",
                format!("⚠️ {}", crate::redact::scrub(&e.to_string())),
            ),
        };
        let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, &body).await;
        return status;
    }

    let lang = ctx.lang;
    match command {
        Command::Ping => {
            let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, "pong").await;
            "ping"
        }
        Command::Help => {
            let text = help_text(&ctx.cfg.forgejo_bot_name, lang);
            let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, &text).await;
            "help"
        }
        Command::Claim => {
            assign_membership(ctx, None, true).await;
            "claim"
        }
        Command::Unclaim => {
            assign_membership(ctx, None, false).await;
            "unclaim"
        }
        Command::Assign { user } => {
            assign_membership(ctx, Some(&user), true).await;
            "assign"
        }
        Command::Cc { users } => {
            let mentions = users
                .iter()
                .map(|user| format!("@{user}"))
                .collect::<Vec<_>>()
                .join(" ");
            let commenter = &ctx.job.commenter;
            let body = crate::t!(
                lang,
                "cc {mentions} (via @{commenter})",
                "cc {mentions}（via @{commenter}）"
            );
            let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, &body).await;
            "cc"
        }
        Command::Label { add, remove } => {
            set_labels(ctx, &add, &remove).await;
            "label"
        }
        Command::Ready | Command::Author | Command::Blocked => {
            set_status(ctx, command).await;
            "status"
        }
        Command::RequestReview { user } => {
            if ctx.job.is_pr {
                request_review(ctx, &user).await;
            } else {
                assign_membership(ctx, Some(&user), true).await;
            }
            "review-request"
        }
        Command::Approve { on_behalf_of } => {
            approve(ctx, on_behalf_of.as_deref()).await;
            "approve"
        }
        Command::Reject => {
            reject(ctx).await;
            "reject"
        }
        Command::ApproveRollup { .. }
        | Command::Rollup { .. }
        | Command::Try
        | Command::Retry
        | Command::Review
        | Command::Codeql
        | Command::Queue => {
            let _ = reply(
                ctx.client,
                &ctx.job.repo,
                ctx.job.number,
                lang.pick(
                    "⚠️ This GitHub-only feature is not available on the Forgejo provider yet; no action was taken.",
                    "⚠️ 该 GitHub 专属能力尚未在 Forgejo provider 上提供，未执行任何操作。",
                ),
            )
            .await;
            "unsupported"
        }
    }
}

async fn assign_membership(ctx: &CommandContext<'_>, target: Option<&str>, add: bool) {
    let login = target.unwrap_or(&ctx.job.commenter);
    let route = format!("/repos/{}/issues/{}", ctx.job.repo, ctx.job.number);
    let issue = match ctx.client.get(&route).await {
        Ok(issue) => issue,
        Err(error) => {
            let _ = reply_error(ctx, "read issue assignees", &error).await;
            return;
        }
    };
    let assignees = match issue.get("assignees") {
        None | Some(serde_json::Value::Null) => Some(Vec::new()),
        Some(serde_json::Value::Array(users)) => users
            .iter()
            .map(|user| {
                user["login"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .collect::<Option<Vec<_>>>(),
        Some(_) => None,
    };
    let Some(mut assignees) = assignees else {
        let _ = reply_error(
            ctx,
            "read issue assignees",
            &ForgejoError::InvalidResponse("missing or malformed assignee list".into()),
        )
        .await;
        return;
    };
    if add {
        if !assignees
            .iter()
            .any(|user| user.eq_ignore_ascii_case(login))
        {
            assignees.push(login.to_string());
        }
    } else {
        assignees.retain(|user| !user.eq_ignore_ascii_case(login));
    }
    match ctx
        .client
        .patch(&route, json!({"assignees": assignees}))
        .await
    {
        Ok(updated) => {
            let present = updated["assignees"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|user| user["login"].as_str())
                .any(|user| user.eq_ignore_ascii_case(login));
            let body = if add && present {
                crate::t!(ctx.lang, "✅ Assigned @{login}.", "✅ 已指派 @{login}。")
            } else if !add && !present {
                crate::t!(
                    ctx.lang,
                    "✅ Removed @{login} from assignees.",
                    "✅ 已将 @{login} 从指派中移除。"
                )
            } else {
                crate::t!(
                    ctx.lang,
                    "⚠️ Forgejo accepted the request but the assignee list did not change.",
                    "⚠️ Forgejo 接受了请求，但指派列表没有变化。"
                )
            };
            let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, &body).await;
        }
        Err(error) => {
            let _ = reply_error(ctx, "update assignees", &error).await;
        }
    }
}

async fn change_labels(
    ctx: &CommandContext<'_>,
    add: &[String],
    remove: &[String],
) -> Result<(), ForgejoError> {
    let mut labels = ctx
        .client
        .get_all(&format!("/repos/{}/labels", ctx.job.repo), 50)
        .await?;
    // Organization labels may be available without being repository-local.
    if add
        .iter()
        .chain(remove)
        .any(|name| !labels.iter().any(|v| v["name"].as_str() == Some(name)))
    {
        let owner = ctx.job.repo.split('/').next().unwrap_or_default();
        match ctx
            .client
            .get_all(&format!("/orgs/{owner}/labels"), 50)
            .await
        {
            Ok(org_labels) => labels.extend(org_labels),
            Err(ForgejoError::Api { status: 404, .. }) => {}
            Err(error) => return Err(error),
        }
    }
    let resolve = |name: &str| -> Result<Option<i64>, ForgejoError> {
        let mut matches = labels.iter().filter(|v| v["name"].as_str() == Some(name));
        let Some(label) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err(ForgejoError::InvalidResponse(format!(
                "ambiguous label: {name}"
            )));
        }
        label["id"]
            .as_i64()
            .filter(|id| *id > 0)
            .map(Some)
            .ok_or_else(|| ForgejoError::InvalidResponse("label has no numeric ID".into()))
    };
    let ids = add
        .iter()
        .map(|name| {
            resolve(name)?.ok_or_else(|| {
                ForgejoError::InvalidResponse(format!("label does not exist: {name}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let removals = remove
        .iter()
        .map(|name| resolve(name))
        .collect::<Result<Vec<_>, _>>()?;
    let route = format!("/repos/{}/issues/{}/labels", ctx.job.repo, ctx.job.number);
    if !ids.is_empty() {
        ctx.client.post(&route, json!({"labels":ids})).await?;
    }
    for id in removals.into_iter().flatten() {
        ctx.client.delete(&format!("{route}/{id}")).await?;
    }
    Ok(())
}

async fn set_labels(ctx: &CommandContext<'_>, add: &[String], remove: &[String]) {
    match change_labels(ctx, add, remove).await {
        Ok(()) => {
            let _ = reply(
                ctx.client,
                &ctx.job.repo,
                ctx.job.number,
                ctx.lang.pick("✅ Labels updated.", "✅ 标签已更新。"),
            )
            .await;
        }
        Err(error) => {
            let _ = reply_error(ctx, "change labels", &error).await;
        }
    }
}

async fn set_status(ctx: &CommandContext<'_>, command: Command) {
    let (wanted, label) = match command {
        Command::Ready => (
            ctx.lang.pick("waiting for review", "等待审查"),
            &ctx.cfg.label_waiting_review,
        ),
        Command::Author => (
            ctx.lang.pick("waiting on the author", "等待作者"),
            &ctx.cfg.label_waiting_author,
        ),
        Command::Blocked => (ctx.lang.pick("blocked", "受阻"), &ctx.cfg.label_blocked),
        _ => unreachable!(),
    };
    let siblings: Vec<String> = [
        &ctx.cfg.label_waiting_review,
        &ctx.cfg.label_waiting_author,
        &ctx.cfg.label_blocked,
    ]
    .into_iter()
    .filter(|candidate| candidate.as_str() != label.as_str())
    .cloned()
    .collect();
    match super::inbox::status_is_current() {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            let _ = reply_error(
                ctx,
                "check status ordering",
                &ForgejoError::InvalidResponse(error),
            )
            .await;
            return;
        }
    }
    if let Err(error) = change_labels(ctx, std::slice::from_ref(label), &siblings).await {
        let _ = reply_error(ctx, "set status labels", &error).await;
        return;
    }
    let body = crate::t!(
        ctx.lang,
        "Status updated: **{wanted}** (`{label}`).",
        "状态已更新：**{wanted}**（`{label}`）。"
    );
    let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, &body).await;
}

async fn request_review(ctx: &CommandContext<'_>, user: &str) {
    match ctx
        .client
        .post(
            &format!(
                "/repos/{}/pulls/{}/requested_reviewers",
                ctx.job.repo, ctx.job.number
            ),
            json!({"reviewers": [user]}),
        )
        .await
    {
        Ok(_) => {
            let body = crate::t!(
                ctx.lang,
                "✅ Requested review from @{user}.",
                "✅ 已请求 @{user} 审查。"
            );
            let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, &body).await;
        }
        Err(error) => {
            let _ = reply_error(ctx, "request review", &error).await;
        }
    }
}

async fn approve(ctx: &CommandContext<'_>, on_behalf_of: Option<&str>) {
    let credited = on_behalf_of.unwrap_or(&ctx.job.commenter);
    let body = format!("Approved for @{credited} via xero-bot.\n\n{APPROVAL_MARKER}");
    match ctx
        .client
        .post(
            &format!("/repos/{}/pulls/{}/reviews", ctx.job.repo, ctx.job.number),
            json!({"body": body, "event": "APPROVED"}),
        )
        .await
    {
        Ok(_) => {
            let message = crate::t!(
                ctx.lang,
                "✅ Approval recorded for @{credited}.",
                "✅ 已为 @{credited} 记录审批。"
            );
            let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, &message).await;
        }
        Err(error) => {
            let _ = reply_error(ctx, "approve pull request", &error).await;
        }
    }
}

async fn reject(ctx: &CommandContext<'_>) {
    let reviews = match ctx
        .client
        .get_all(
            &format!("/repos/{}/pulls/{}/reviews", ctx.job.repo, ctx.job.number),
            50,
        )
        .await
    {
        Ok(reviews) => reviews,
        Err(error) => {
            let _ = reply_error(ctx, "list reviews", &error).await;
            return;
        }
    };
    let mine: Vec<i64> = reviews
        .iter()
        .filter(|review| {
            review["user"]["login"]
                .as_str()
                .is_some_and(|login| login.eq_ignore_ascii_case(ctx.bot_login))
                && review["state"].as_str() == Some("APPROVED")
                && review["body"]
                    .as_str()
                    .is_some_and(|body| body.contains(APPROVAL_MARKER))
        })
        .filter_map(|review| review["id"].as_i64().filter(|id| *id > 0))
        .collect();
    if mine.is_empty() {
        let _ = reply(
            ctx.client,
            &ctx.job.repo,
            ctx.job.number,
            ctx.lang.pick(
                "No xero-bot approval to withdraw.",
                "没有可撤回的 xero-bot 审批。",
            ),
        )
        .await;
        return;
    }
    let mut dismissed = 0usize;
    let mut last_error = None;
    for review in mine {
        match ctx
            .client
            .post(
                &format!(
                    "/repos/{}/pulls/{}/reviews/{review}/dismissals",
                    ctx.job.repo, ctx.job.number
                ),
                json!({"message": format!("r- by @{}", ctx.job.commenter)}),
            )
            .await
        {
            Ok(_) => dismissed += 1,
            Err(error) => last_error = Some(error),
        }
    }
    let body = if let Some(error) = last_error {
        crate::t!(
            ctx.lang,
            "⚠️ Withdrew {dismissed} approval(s), but some withdrawals failed: {error}",
            "⚠️ 已撤回 {dismissed} 个审批，但部分撤回失败：{error}"
        )
    } else if dismissed > 0 {
        crate::t!(
            ctx.lang,
            "✅ Withdrew {dismissed} xero-bot approval(s).",
            "✅ 已撤回 {dismissed} 个 xero-bot 审批。"
        )
    } else {
        format!(
            "{} {}",
            ctx.lang
                .pick("❌ Could not withdraw approval:", "❌ 撤回审批失败："),
            "unknown"
        )
    };
    let _ = reply(ctx.client, &ctx.job.repo, ctx.job.number, &body).await;
}

async fn reply_error(
    ctx: &CommandContext<'_>,
    action: &str,
    error: &ForgejoError,
) -> Result<(), ForgejoError> {
    let body = format!(
        "{} `{action}`: `{error}`",
        ctx.lang
            .pick("⚠️ Forgejo request failed", "⚠️ Forgejo 请求失败")
    );
    reply(ctx.client, &ctx.job.repo, ctx.job.number, &body).await
}

async fn reply(
    client: &ForgejoClient,
    repo: &str,
    number: i64,
    body: &str,
) -> Result<(), ForgejoError> {
    client
        .post(
            &format!("/repos/{repo}/issues/{number}/comments"),
            json!({"body": crate::redact::scrub(body)}),
        )
        .await
        .map(|_| ())
}

fn help_text(bot_name: &str, lang: Lang) -> String {
    match lang {
        Lang::En => format!(
            "**Forgejo xero-bot commands**\n\n\
             `ping`, `help`, `claim`/`take`, `unclaim`/`untake`, `label +a -b`, \
             `assign @user`, `cc @user`, `ready`, `author`, `blocked`, `r? @user`, \
             `r+`, `r-`.\n\n\
             Mention `@{bot_name}` for commands whose repository policy requires it. \
             `review`, `codeql`, `queue`, GitHub Checks, and the GitHub merge queue are \
             not available on the Forgejo provider.\n\n\
             `ready` changes status labels only; use `r? @reviewer` to notify a reviewer.\n\n\
             Configuration: `.github/xero-bot.toml` on the Forgejo default branch."
        ),
        Lang::Zh => format!(
            "**Forgejo xero-bot 指令**\n\n\
             `ping`、`help`、`claim`/`take`、`unclaim`/`untake`、`label +a -b`、\
             `assign @user`、`cc @user`、`ready`、`author`、`blocked`、`r? @user`、\
             `r+`、`r-`。\n\n\
             仓库策略要求时请显式 @{bot_name}。`review`、`codeql`、`queue`、\
             GitHub Checks 和 GitHub 合并队列暂不适用于 Forgejo provider。\n\n\
             `ready` 仅修改状态标签；请用 `r? @reviewer` 通知审查者。\n\n\
             配置来源：Forgejo 默认分支上的 `.github/xero-bot.toml`。"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "xero-forgejo-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cfg() -> Config {
        let mut cfg = Config::from_env();
        cfg.forgejo_bot_name = "xero-review".into();
        cfg.forgejo_base_url = "https://forgejo.invalid".into();
        cfg.forgejo_token = "test-token".into();
        cfg.forgejo_webhook_secret = "webhook-secret".into();
        cfg
    }

    fn comment_job(body: &str, is_pr: bool) -> CommentJob {
        CommentJob {
            repo: "team/repo".into(),
            number: 7,
            repository_id: Some(10),
            issue_id: Some(70),
            comment_id: Some(100),
            user_id: Some(20),
            source_at: Some("2026-11-03T00:00:00Z".into()),
            body: body.into(),
            commenter: "alice".into(),
            author: "bob".into(),
            is_pr,
        }
    }

    async fn mock_identity_and_policy(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/user"))
            .and(header("authorization", "token test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "xero-review"})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/team/repo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"default_branch": "main"})),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/team/repo/branches/main"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"commit":{"id":"a".repeat(40)}})),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/team/repo/contents/.github"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/team/repo/contents/.github/xero-bot.toml",
            ))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "not found"})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn claim_reads_policy_patches_assignees_and_posts_a_reply() {
        let server = MockServer::start().await;
        mock_identity_and_policy(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/team/repo/issues/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 7, "assignees": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/repos/team/repo/issues/7"))
            .and(body_string_contains("\"assignees\""))
            .and(body_string_contains("\"alice\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 7, "assignees": [{"login": "alice"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/issues/7/comments"))
            .and(body_string_contains("Assigned @alice"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 11})))
            .expect(1)
            .mount(&server)
            .await;

        let dir = Dir::new();
        let store = Store::open(&dir.0).unwrap();
        let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
        handle_issue_comment_with_client(&client, &cfg(), &store, comment_job("claim", false))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn approve_requires_write_and_creates_a_marked_review() {
        let server = MockServer::start().await;
        mock_identity_and_policy(&server).await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/team/repo/collaborators/alice/permission",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "write"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/pulls/7/reviews"))
            .and(body_string_contains("\"event\":\"APPROVED\""))
            .and(body_string_contains(APPROVAL_MARKER))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 3})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/issues/7/comments"))
            .and(body_string_contains("Approval recorded for @alice"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 12})))
            .expect(1)
            .mount(&server)
            .await;

        let dir = Dir::new();
        let store = Store::open(&dir.0).unwrap();
        let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
        handle_issue_comment_with_client(
            &client,
            &cfg(),
            &store,
            comment_job("@xero-review r+", true),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pull_request_review_request_uses_forgejo_reviewer_endpoint() {
        let server = MockServer::start().await;
        mock_identity_and_policy(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/pulls/7/requested_reviewers"))
            .and(body_string_contains(r#""reviewers":["alice"]"#))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"reviewers": []})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/issues/7/comments"))
            .and(body_string_contains("Requested review from @alice"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 14})))
            .expect(1)
            .mount(&server)
            .await;

        let dir = Dir::new();
        let store = Store::open(&dir.0).unwrap();
        let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
        handle_issue_comment_with_client(&client, &cfg(), &store, comment_job("r? @alice", true))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reject_requires_write_and_dismisses_only_marked_bot_reviews() {
        let server = MockServer::start().await;
        mock_identity_and_policy(&server).await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/team/repo/collaborators/alice/permission",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"permission": "write"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/team/repo/pulls/7/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": 1, "state": "APPROVED", "body": "human", "user": {"login": "alice"}},
                {"id": 2, "state": "APPROVED", "body": format!("approval\n{APPROVAL_MARKER}"), "user": {"login": "xero-review"}}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/pulls/7/reviews/2/dismissals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 2})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/issues/7/comments"))
            .and(body_string_contains("Withdrew 1 xero-bot approval"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 15})))
            .expect(1)
            .mount(&server)
            .await;

        let dir = Dir::new();
        let store = Store::open(&dir.0).unwrap();
        let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
        handle_issue_comment_with_client(
            &client,
            &cfg(),
            &store,
            comment_job("@xero-review r-", true),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn self_approval_is_refused_before_permission_or_review_writes() {
        let server = MockServer::start().await;
        mock_identity_and_policy(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/team/repo/issues/7/comments"))
            .and(body_string_contains("PR author cannot approve"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 13})))
            .expect(1)
            .mount(&server)
            .await;

        let dir = Dir::new();
        let store = Store::open(&dir.0).unwrap();
        let client = ForgejoClient::new(&server.uri(), "test-token").unwrap();
        let mut job = comment_job("@xero-review r+", true);
        job.author = "alice".into();
        handle_issue_comment_with_client(&client, &cfg(), &store, job)
            .await
            .unwrap();
    }
    include!("commands_audit_tests.rs");
}
