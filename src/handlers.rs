//! Command execution: takes parsed commands from a PR comment and performs the
//! corresponding GitHub API actions, posting a reply per command.

use crate::commands::Command;
use crate::config::Config;
use crate::github::Client;

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
    }
}
