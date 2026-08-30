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
    pub app_slug: String,
    pub installation_id: i64,
}

pub fn help_text(bot_name: &str) -> String {
    format!(
        "### 🤖 xero-bot 命令参考\n\n\
| 命令 | 说明 |\n|---|---|\n\
| `@{bot_name} review` | AI 代码审查(增量:结合上一轮审查与新提交) |\n\
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
| `@{bot_name} unclaim` | 释放指派 |\n\n\
_更多命令陆续加入。_"
    )
}

/// Execute all parsed commands from one comment, in order.
pub async fn handle_comment(
    gh: &Client,
    cfg: &Config,
    ctx: &CommentContext,
    commands: Vec<Command>,
) -> Vec<String> {
    let mut results = Vec::new();
    for cmd in commands {
        let r = handle_one(gh, cfg, ctx, cmd).await;
        results.push(r);
    }
    results
}

async fn handle_one(gh: &Client, cfg: &Config, ctx: &CommentContext, cmd: Command) -> String {
    match cmd {
        Command::Ping => gh
            .post_issue_comment(&ctx.repo, ctx.pr_number, "pong 🏓")
            .await
            .is_ok()
            .then_some("ok".into())
            .unwrap_or_else(|| "error".into()),
        Command::Help => gh
            .post_issue_comment(&ctx.repo, ctx.pr_number, &help_text(&cfg.bot_name))
            .await
            .is_ok()
            .then_some("ok".into())
            .unwrap_or_else(|| "error".into()),
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
            crate::review::run_builtin(gh, cfg, &ctx.repo, ctx.pr_number).await
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
