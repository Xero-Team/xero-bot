//! Shared, read-only command authorization. Trigger modes never grant rights.
use super::CommentContext;
use crate::commands::Command;
use crate::config::Config;
use crate::github::{normalize_login, Client};
use crate::t;

pub(crate) struct Refusal {
    pub status: &'static str,
    pub message: String,
}

/// Check fresh repository permission on every invocation; unknown is a denial.
async fn require_write(
    gh: &Client,
    ctx: &CommentContext,
    user: &str,
    credited: bool,
) -> Result<(), Refusal> {
    let lang = ctx.lang;
    let permission = gh.collaborator_permission(&ctx.repo, user).await.map_err(|error| {
        tracing::warn!("command permission check failed: {error}");
        Refusal {
            status: "permission-error",
            message: t!(lang,
                "⚠️ Could not check @{user}'s permissions. No approval or queue change was made; try again later.",
                "⚠️ 无法确认 @{user} 的仓库权限，未修改审批或合并队列，请稍后重试。"),
        }
    })?;
    if matches!(permission.as_str(), "write" | "maintain" | "admin") {
        return Ok(());
    }
    Err(Refusal {
        status: if credited { "credited-denied" } else { "denied" },
        message: t!(lang,
            "⚠️ @{user}: they need write access or above for this approval operation (currently: {permission}).",
            "⚠️ @{user}：they need write/maintain/admin 权限才能执行此审批操作（当前：{permission}）。"),
    })
}

/// Run before candidate resolution/session creation and again immediately before
/// the privileged handler. Non-approval commands keep their existing authority.
pub(crate) async fn authorize(
    gh: &Client,
    cfg: &Config,
    ctx: &CommentContext,
    command: &Command,
) -> Result<(), Refusal> {
    let lang = ctx.lang;
    let Command::Approve { on_behalf_of } = command else {
        return if matches!(command, Command::Reject) {
            require_write(gh, ctx, &ctx.commenter, false).await
        } else {
            Ok(())
        };
    };
    if let Some(other) = on_behalf_of {
        if !cfg.r_plus_allow_on_behalf {
            return Err(Refusal { status: "on-behalf-disabled",
                message: t!(lang,
                    "⚠️ Approving on behalf of @{other} is disabled here. Set `R_PLUS_ALLOW_ON_BEHALF=true` to enable this deployment feature. Plain `r+` still works.",
                    "⚠️ 本部署未开启代 @{other} 审批，需设置 `R_PLUS_ALLOW_ON_BEHALF=true`。普通 `r+` 不受影响。") });
        }
    }
    let credited = on_behalf_of.as_deref().unwrap_or(&ctx.commenter);
    if normalize_login(&ctx.commenter) == normalize_login(&ctx.pr_author)
        || normalize_login(credited) == normalize_login(&ctx.pr_author)
    {
        return Err(Refusal { status: "self-approve",
            message: t!(lang,
                "⚠️ This would be a self-approval: the PR author cannot request or be credited with approval, including on behalf of someone else.",
                "⚠️ 不能自我审批：PR 作者不能发起审批或被归功审批，代他人审批同样不允许。") });
    }
    require_write(gh, ctx, &ctx.commenter, false).await?;
    if credited != ctx.commenter {
        if !crate::commands::is_valid_login(credited) {
            return Err(Refusal {
                status: "invalid-credited",
                message: t!(
                    lang,
                    "⚠️ `{credited}` isn't a valid GitHub login.",
                    "⚠️ `{credited}` 不是合法的 GitHub 用户名。"
                ),
            });
        }
        require_write(gh, ctx, credited, true).await?;
    }
    Ok(())
}
