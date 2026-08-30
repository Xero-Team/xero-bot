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
        | `@{bot_name} ping` | 健康检查 |\n\
        | `@{bot_name} help` | 显示本帮助 |\n\n\
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
    }
}
