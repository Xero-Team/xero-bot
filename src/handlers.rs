//! Command execution: takes parsed commands from a PR comment and performs the
//! corresponding GitHub API actions, posting a reply per command.

use crate::commands::Command;
use crate::config::Config;
use crate::github::{Client, GhError};

pub struct CommentContext {
    pub repo: String,
    pub pr_number: i64,
    pub commenter: String,
    pub pr_author: String,
    pub installation_id: i64,
}

pub fn help_text(bot_name: &str) -> String {
    format!(
        "### 🤖 xero-bot 命令参考\n\n\
| 命令 | 说明 |\n|---|---|\n\
| `@{bot_name} review` | AI 代码审查(增量:结合上一轮审查与新提交) |\n\
| `@{bot_name} codeql` | CodeQL 质量报告(读取仓库存量告警并映射到本次变更) |\n\
| `@{bot_name} ping` | 健康检查 |\n\
| `@{bot_name} help` | 显示本帮助 |\n\
| `r? @user` | 请求 @user 审查(自动指派为 reviewer) |\n\
| `@{bot_name} cc @user…` | 抄送/通知指定用户 |\n\
| `@{bot_name} ready` / `?r` | 标记等待审查(打 `waiting-on-review`) |\n\
| `@{bot_name} author` | 标记等待作者(打 `waiting-on-author`) |\n\
| `@{bot_name} blocked` | 标记受阻(打 `blocked`) |\n\
| `@{bot_name} label +a -b` | 添加/移除标签 |\n\
| `@{bot_name} assign @user` | 指派给 @user |\n\
| `@{bot_name} claim` | 认领(指派给自己) |\n\
| `@{bot_name} unclaim` | 释放指派 |\n\
| `@{bot_name} r+` | 代审批(需 write 权限;bot 以你的名义提交 APPROVE) |\n\
| `@{bot_name} r+ as @user` | 以 @user 名义代审批(用于转发他处给出的批准) |\n\
| `@{bot_name} r-` | 撤回 bot 的审批 |\n\n\
_冲突的 PR 会被自动打上 `needs-rebase` 标签并提醒。_"
    )
}

/// Collapse a GitHub call into the short result label that `dispatch` logs,
/// recording the underlying error first.
///
/// The label alone can't distinguish a missing App permission from a bad
/// installation token, so the `GhError` — which carries the HTTP status — must
/// reach the log or a failure is undiagnosable from the outside.
fn labeled(what: &str, result: Result<(), GhError>) -> String {
    match result {
        Ok(()) => "ok".into(),
        Err(e) => {
            tracing::warn!("{what} failed: {e}");
            "error".into()
        }
    }
}

/// Execute all parsed commands from one comment, in order.
///
/// `diagnostics` are pre-rendered messages about parts of the comment that
/// couldn't be understood; they may be present with no commands at all, which
/// is the whole point — a mistyped command used to vanish without a word.
pub async fn handle_comment(
    gh: &Client,
    cfg: &Config,
    ctx: &CommentContext,
    commands: Vec<Command>,
    diagnostics: Vec<String>,
) -> Vec<String> {
    let mut results = Vec::new();

    // Posted before the commands run: `review` can take minutes, and a note
    // saying half the comment was misunderstood is only useful while the author
    // is still looking.
    if let Some(body) = crate::commands::diag::render_messages(&diagnostics, &cfg.bot_name) {
        let r = labeled(
            "diagnostics reply",
            gh.post_issue_comment(&ctx.repo, ctx.pr_number, &body).await,
        );
        results.push(format!("diagnostics:{r}"));
    }

    for cmd in commands {
        let r = handle_one(gh, cfg, ctx, cmd).await;
        results.push(r);
    }
    results
}

async fn handle_one(gh: &Client, cfg: &Config, ctx: &CommentContext, cmd: Command) -> String {
    match cmd {
        Command::Ping => labeled(
            "ping reply",
            gh.post_issue_comment(&ctx.repo, ctx.pr_number, "pong 🏓")
                .await,
        ),
        Command::Help => labeled(
            "help reply",
            gh.post_issue_comment(&ctx.repo, ctx.pr_number, &help_text(&cfg.bot_name))
                .await,
        ),
        Command::Review => {
            if !ctx_is_pr(ctx) {
                let _ = gh
                    .post_issue_comment(&ctx.repo, ctx.pr_number, "⚠️ review 命令只在 PR 上有效。")
                    .await;
                return "not-a-pr".into();
            }
            if !cfg.ai_ready() && cfg.review_engine == "builtin" {
                let _ = gh
                    .post_issue_comment(
                        &ctx.repo,
                        ctx.pr_number,
                        "⚠️ 未配置 AI(缺 AI_BASE_URL/AI_API_KEY/AI_MODEL),无法审查。",
                    )
                    .await;
                return "ai-not-configured".into();
            }
            // token for subprocess engines: installation token via REST
            let token = gh
                .installation_token(cfg, ctx.installation_id)
                .await
                .unwrap_or_default();
            crate::engines_subproc::run_review(gh, cfg, &ctx.repo, ctx.pr_number, &token).await
        }
        Command::Codeql => {
            crate::codeql::run_codeql_report(gh, cfg, &ctx.repo, ctx.pr_number).await
        }
        Command::RequestReview { user } => {
            // triagebot: assignment is the review request
            match gh
                .add_assignees(&ctx.repo, ctx.pr_number, &[user.clone()])
                .await
            {
                Ok(()) => {
                    let _ = gh
                        .post_issue_comment(
                            &ctx.repo,
                            ctx.pr_number,
                            &format!("已指派 @{user} 为 reviewer,请审查 🙏"),
                        )
                        .await;
                    "ok".into()
                }
                Err(e) => {
                    let msg = match &e {
                        GhError::Api { status, .. } if *status == 403 || *status == 422 => format!(
                            "⚠️ 无法指派 @{user}:用户需要有仓库写权限、或是组织成员、或曾在该 PR 留言。"
                        ),
                        _ => format!("⚠️ 指派失败: `{e}`"),
                    };
                    let _ = gh.post_issue_comment(&ctx.repo, ctx.pr_number, &msg).await;
                    format!("error: {e}")
                }
            }
        }
        Command::Cc { users } => {
            let mentions: Vec<String> = users.iter().map(|u| format!("@{u}")).collect();
            let _ = gh
                .post_issue_comment(
                    &ctx.repo,
                    ctx.pr_number,
                    &format!("cc {} (via @{})", mentions.join(" "), ctx.commenter),
                )
                .await;
            "ok".into()
        }
        Command::Ready | Command::Author | Command::Blocked => {
            set_status_label(gh, cfg, ctx, cmd).await
        }
        Command::Label { add, remove } => {
            // permission gate: label changes require at least triage access —
            // GitHub enforces this API-side; we just try and report.
            let mut ok = true;
            if !add.is_empty() {
                if let Err(e) = gh.add_labels(&ctx.repo, ctx.pr_number, &add).await {
                    tracing::warn!("add labels {add:?} on {}#{}: {e}", ctx.repo, ctx.pr_number);
                    let _ = gh
                        .post_issue_comment(
                            &ctx.repo,
                            ctx.pr_number,
                            &format!("⚠️ 添加标签失败: `{e}`"),
                        )
                        .await;
                    ok = false;
                }
            }
            for label in &remove {
                if let Err(e) = gh.remove_label(&ctx.repo, ctx.pr_number, label).await {
                    // 404 = label not present; not an error worth reporting
                    if !matches!(&e, GhError::Api { status: 404, .. }) {
                        tracing::warn!(
                            "remove label {label} on {}#{}: {e}",
                            ctx.repo,
                            ctx.pr_number
                        );
                        let _ = gh
                            .post_issue_comment(
                                &ctx.repo,
                                ctx.pr_number,
                                &format!("⚠️ 移除标签失败: `{e}`"),
                            )
                            .await;
                        ok = false;
                    }
                }
            }
            if ok {
                let mut parts: Vec<String> = Vec::new();
                if !add.is_empty() {
                    parts.push(format!(
                        "+{}",
                        add.iter()
                            .map(|l| format!("`{l}`"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
                if !remove.is_empty() {
                    parts.push(format!(
                        "-{}",
                        remove
                            .iter()
                            .map(|l| format!("`{l}`"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
                let _ = gh
                    .post_issue_comment(
                        &ctx.repo,
                        ctx.pr_number,
                        &format!("已更新标签: {}", parts.join(" ")),
                    )
                    .await;
            }
            if ok {
                "ok".into()
            } else {
                "error".into()
            }
        }
        Command::Assign { user } => match gh
            .add_assignees(&ctx.repo, ctx.pr_number, &[user.clone()])
            .await
        {
            Ok(()) => {
                let _ = gh
                    .post_issue_comment(&ctx.repo, ctx.pr_number, &format!("已指派给 @{user}。"))
                    .await;
                "ok".into()
            }
            Err(e) => {
                let _ = gh
                    .post_issue_comment(&ctx.repo, ctx.pr_number, &format!("⚠️ 指派失败: `{e}`"))
                    .await;
                format!("error: {e}")
            }
        },
        Command::Claim => {
            match gh
                .add_assignees(&ctx.repo, ctx.pr_number, &[ctx.commenter.clone()])
                .await
            {
                Ok(()) => {
                    let _ = gh
                        .post_issue_comment(
                            &ctx.repo,
                            ctx.pr_number,
                            &format!("@{} 已认领。", ctx.commenter),
                        )
                        .await;
                    "ok".into()
                }
                Err(e) => {
                    let _ = gh
                        .post_issue_comment(
                            &ctx.repo,
                            ctx.pr_number,
                            &format!("⚠️ 认领失败: `{e}`"),
                        )
                        .await;
                    format!("error: {e}")
                }
            }
        }
        Command::Unclaim => {
            match gh
                .remove_assignees(&ctx.repo, ctx.pr_number, &[ctx.commenter.clone()])
                .await
            {
                Ok(()) => {
                    let _ = gh
                        .post_issue_comment(
                            &ctx.repo,
                            ctx.pr_number,
                            &format!("@{} 已释放指派。", ctx.commenter),
                        )
                        .await;
                    "ok".into()
                }
                Err(e) => {
                    let _ = gh
                        .post_issue_comment(
                            &ctx.repo,
                            ctx.pr_number,
                            &format!("⚠️ 释放失败: `{e}`"),
                        )
                        .await;
                    format!("error: {e}")
                }
            }
        }
        Command::Approve { on_behalf_of } => handle_approve(gh, cfg, ctx, on_behalf_of).await,
        Command::Reject => handle_reject(gh, cfg, ctx).await,
    }
}

fn ctx_is_pr(_ctx: &CommentContext) -> bool {
    // classify() only produces PrComment for issues with a pull_request
    // pointer; issue comments are filtered earlier. Always true here.
    true
}

/// ready/author/blocked: add one status label, remove its siblings.
async fn set_status_label(gh: &Client, cfg: &Config, ctx: &CommentContext, cmd: Command) -> String {
    let (add, label_desc) = match cmd {
        Command::Ready => (&cfg.label_waiting_review, "等待审查"),
        Command::Author => (&cfg.label_waiting_author, "等待作者"),
        Command::Blocked => (&cfg.label_blocked, "受阻"),
        _ => unreachable!(),
    };
    let siblings: Vec<String> = [
        &cfg.label_waiting_review,
        &cfg.label_waiting_author,
        &cfg.label_blocked,
    ]
    .into_iter()
    .filter(|l| l.as_str() != add.as_str())
    .cloned()
    .collect();

    let mut ok = true;
    if let Err(e) = gh
        .add_labels(&ctx.repo, ctx.pr_number, &[add.clone()])
        .await
    {
        tracing::warn!("add label {add} on {}#{}: {e}", ctx.repo, ctx.pr_number);
        let _ = gh
            .post_issue_comment(&ctx.repo, ctx.pr_number, &format!("⚠️ 打标签失败: `{e}`"))
            .await;
        ok = false;
    }
    if ok {
        for l in &siblings {
            if let Err(e) = gh.remove_label(&ctx.repo, ctx.pr_number, l).await {
                if !matches!(&e, GhError::Api { status: 404, .. }) {
                    // removing a non-existent label is fine; anything else worth logging
                    tracing::warn!("remove label {l}: {e}");
                }
            }
        }
        let _ = gh
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                &format!("状态已更新: **{label_desc}**(`{add}`)。"),
            )
            .await;
    }
    if ok {
        "ok".into()
    } else {
        "error".into()
    }
}

/// r+: permission-gated approval relay (bors-style).
async fn handle_approve(
    gh: &Client,
    _cfg: &Config,
    ctx: &CommentContext,
    on_behalf_of: Option<String>,
) -> String {
    // 1. commenter must have write/maintain/admin
    let perm = match gh.collaborator_permission(&ctx.repo, &ctx.commenter).await {
        Ok(p) => p,
        Err(e) => {
            let _ = gh
                .post_issue_comment(&ctx.repo, ctx.pr_number, &format!("⚠️ 无法校验权限: `{e}`"))
                .await;
            return format!("error: {e}");
        }
    };
    if !matches!(perm.as_str(), "admin" | "maintain" | "write") {
        let _ = gh
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                &format!(
                    "⚠️ @{commenter} 的 `r+` 被拒绝:需要仓库 write 及以上权限(当前: {perm})。",
                    commenter = ctx.commenter
                ),
            )
            .await;
        return "denied".into();
    }

    // 2. PR author cannot approve their own PR (GitHub itself blocks this for
    //    the real author; here the bot is the author of the review, so we
    //    enforce the semantic manually)
    let credited = on_behalf_of.unwrap_or_else(|| ctx.commenter.clone());
    if credited.eq_ignore_ascii_case(&ctx.pr_author) {
        let _ = gh
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                &format!("⚠️ 不能审批自己的 PR(`{credited}` 是本 PR 作者)。"),
            )
            .await;
        return "self-approve".into();
    }

    // 3. post APPROVE review, crediting the human
    let body = if credited == ctx.commenter {
        format!(
            "✅ Approved on behalf of @{commenter} (r+ by @{commenter}, relayed by xero-bot).",
            commenter = ctx.commenter
        )
    } else {
        format!(
            "✅ Approved on behalf of @{credited} (r+ by {commenter}, relayed by xero-bot).",
            commenter = ctx.commenter
        )
    };
    match gh
        .post_approve_review(&ctx.repo, ctx.pr_number, &body)
        .await
    {
        Ok(_) => {
            let _ = gh.post_issue_comment(&ctx.repo, ctx.pr_number, &body).await;
            "ok".into()
        }
        Err(e) => {
            let _ = gh
                .post_issue_comment(&ctx.repo, ctx.pr_number, &format!("⚠️ 代审批失败: `{e}`"))
                .await;
            format!("error: {e}")
        }
    }
}

/// r-: withdraw — dismiss our own previous APPROVE reviews.
async fn handle_reject(gh: &Client, _cfg: &Config, ctx: &CommentContext) -> String {
    let reviews = match gh.list_pr_reviews(&ctx.repo, ctx.pr_number).await {
        Ok(r) => r,
        Err(e) => {
            let _ = gh
                .post_issue_comment(&ctx.repo, ctx.pr_number, &format!("⚠️ 列出审查失败: `{e}`"))
                .await;
            return format!("error: {e}");
        }
    };
    // A review authored by this App has login `slug[bot]`; comparing that to a
    // bare slug never matched, so `r-` always claimed there was nothing to
    // withdraw — even right after a successful `r+`.
    let slug = crate::github::normalize_login(&gh.app_slug);
    if slug.is_empty() {
        let _ = gh
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                "⚠️ 无法确定 bot 自身身份,无法撤回审批(请检查 APP_SLUG / BOT_NAME)。",
            )
            .await;
        return "no-app-slug".into();
    }
    let mine: Vec<i64> = reviews
        .iter()
        .filter(|r| {
            r.get("user")
                .and_then(|u| u.get("login"))
                .and_then(|l| l.as_str())
                .map(|l| crate::github::normalize_login(l) == slug)
                .unwrap_or(false)
                && r.get("state").and_then(|s| s.as_str()) == Some("APPROVED")
        })
        .filter_map(|r| r.get("id").and_then(|i| i.as_i64()))
        .collect();

    if mine.is_empty() {
        let _ = gh
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                "没有可撤回的 bot 审批(此前未在本 PR 上 `r+`)。",
            )
            .await;
        return "nothing-to-dismiss".into();
    }

    let mut dismissed = 0;
    for id in &mine {
        if let Err(e) = gh
            .dismiss_review(
                &ctx.repo,
                ctx.pr_number,
                *id,
                &format!("r- by @{}: approval withdrawn", ctx.commenter),
            )
            .await
        {
            tracing::warn!("dismiss review {id}: {e}");
        } else {
            dismissed += 1;
        }
    }
    let _ = gh
        .post_issue_comment(
            &ctx.repo,
            ctx.pr_number,
            &format!("已撤回 {dismissed} 个 bot 审批(r- by @{})。", ctx.commenter),
        )
        .await;
    "ok".into()
}
