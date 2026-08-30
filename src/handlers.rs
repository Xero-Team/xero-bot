//! Command execution: takes parsed commands from a PR comment and performs the
//! corresponding GitHub API actions, posting a reply per command.
//!
//! Every reply is written twice, once per language, and picked by
//! [`CommentContext::lang`] — see [`crate::lang`] for how that is decided. The
//! two wordings sit on adjacent lines so a drift between them is visible in
//! review rather than only in production.

use crate::commands::Command;
use crate::config::Config;
use crate::github::{Client, GhError};
use crate::lang::Lang;
use crate::t;

pub struct CommentContext {
    pub repo: String,
    pub pr_number: i64,
    pub commenter: String,
    pub pr_author: String,
    pub installation_id: i64,
    /// False when this is an issue rather than a pull request. Most commands
    /// don't care — issues and PRs share the issues API — but the four that
    /// reach a `/pulls/` endpoint have to say so instead of failing obscurely.
    pub is_pr: bool,
    /// Which language to answer in, decided from the PR's commits.
    pub lang: Lang,
}

pub fn help_text(bot_name: &str, lang: Lang) -> String {
    match lang {
        Lang::En => format!(
            "### xero-bot commands\n\n\
| Command | What it does |\n|---|---|\n\
| `@{bot_name} review` | AI code review (incremental: last review plus new commits) |\n\
| `@{bot_name} codeql` | CodeQL quality report (existing repo alerts mapped onto this change) |\n\
| `@{bot_name} ping` | Health check |\n\
| `@{bot_name} help` | Show this help |\n\
| `r? @user` | Request review from @user (assigns them as reviewer) |\n\
| `@{bot_name} cc @user…` | Notify the listed users |\n\
| `@{bot_name} ready` / `?r` | Waiting for review (adds `waiting-on-review`) |\n\
| `@{bot_name} author` | Waiting on the author (adds `waiting-on-author`) |\n\
| `@{bot_name} blocked` | Blocked (adds `blocked`) |\n\
| `@{bot_name} label +a -b` | Add / remove labels |\n\
| `@{bot_name} assign @user` | Assign to @user |\n\
| `@{bot_name} claim` | Claim (assign to yourself) |\n\
| `@{bot_name} unclaim` | Release the assignment |\n\
| `@{bot_name} r+` | Relay an approval (needs write; the bot APPROVEs in your name) |\n\
| `@{bot_name} r+ as @user` | Relay an approval crediting @user |\n\
| `@{bot_name} r-` | Withdraw the bot's approval |\n\n\
_Conflicted PRs are labelled `needs-rebase` automatically, with a reminder._"
        ),
        Lang::Zh => format!(
            "### xero-bot 命令参考\n\n\
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
        ),
    }
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
    if let Some(body) =
        crate::commands::diag::render_messages(&diagnostics, &cfg.bot_name, ctx.lang)
    {
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
    let lang = ctx.lang;

    // One gate for all four PR-only commands, where `review` used to have the
    // only ad-hoc check — and that check could never fire, because the context
    // flag it read was hardcoded `true`. Saying so is the point: the dispatch
    // layer used to drop the whole delivery, so the comment got no answer.
    if !ctx.is_pr && cmd.requires_pr() {
        let verb = match &cmd {
            Command::Review => "review",
            Command::Codeql => "codeql",
            Command::Approve { .. } => "r+",
            Command::Reject => "r-",
            // `requires_pr` is exhaustive over the enum, so reaching here means
            // it and this list have drifted apart.
            other => unreachable!("{other:?} is PR-only but unnamed here"),
        };
        let _ = gh
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                &t!(
                    lang,
                    "⚠️ `{verb}` only works on a pull request.",
                    "⚠️ `{verb}` 命令只在 PR 上有效。"
                ),
            )
            .await;
        return "not-a-pr".into();
    }

    match cmd {
        Command::Ping => labeled(
            "ping reply",
            gh.post_issue_comment(&ctx.repo, ctx.pr_number, "pong 🏓")
                .await,
        ),
        Command::Help => labeled(
            "help reply",
            gh.post_issue_comment(&ctx.repo, ctx.pr_number, &help_text(&cfg.bot_name, lang))
                .await,
        ),
        Command::Review => {
            if !cfg.ai_ready() && cfg.review_engine == "builtin" {
                let _ = gh
                    .post_issue_comment(
                        &ctx.repo,
                        ctx.pr_number,
                        lang.pick(
                            "⚠️ No AI configured (missing AI_BASE_URL / AI_API_KEY / AI_MODEL); cannot review.",
                            "⚠️ 未配置 AI(缺 AI_BASE_URL/AI_API_KEY/AI_MODEL),无法审查。",
                        ),
                    )
                    .await;
                return "ai-not-configured".into();
            }
            // The installation token is fetched by the engines that need one —
            // only the subprocess engines do, and only they can report its
            // failure usefully. Fetching it here meant `builtin` and `agent`
            // paid for a token they never touch, and `unwrap_or_default()` fed
            // an empty string into a clone URL, which fails as an
            // authentication error with no hint that the token was the problem.
            crate::engines_subproc::run_review(
                gh,
                cfg,
                &ctx.repo,
                ctx.pr_number,
                ctx.installation_id,
                lang,
            )
            .await
        }
        Command::Codeql => {
            crate::codeql::run_codeql_report(gh, cfg, &ctx.repo, ctx.pr_number, lang).await
        }
        Command::RequestReview { user } => request_review(gh, ctx, &user).await,
        Command::Cc { users } => {
            let mentions = users
                .iter()
                .map(|u| format!("@{u}"))
                .collect::<Vec<_>>()
                .join(" ");
            let commenter = &ctx.commenter;
            // An `@mention` in a comment *is* GitHub's notification mechanism,
            // so posting one is the whole job — but the POST can still fail, and
            // discarding its result reported a delivered `cc` for a comment that
            // never existed.
            labeled(
                "cc reply",
                gh.post_issue_comment(
                    &ctx.repo,
                    ctx.pr_number,
                    &t!(
                        lang,
                        "cc {mentions} (via @{commenter})",
                        "cc {mentions}(via @{commenter})"
                    ),
                )
                .await,
            )
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
                            &t!(
                                lang,
                                "⚠️ Could not add labels: `{e}`",
                                "⚠️ 添加标签失败: `{e}`"
                            ),
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
                                &t!(
                                    lang,
                                    "⚠️ Could not remove labels: `{e}`",
                                    "⚠️ 移除标签失败: `{e}`"
                                ),
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
                let changed = parts.join(" ");
                let _ = gh
                    .post_issue_comment(
                        &ctx.repo,
                        ctx.pr_number,
                        &t!(lang, "Labels updated: {changed}", "已更新标签: {changed}"),
                    )
                    .await;
            }
            if ok {
                "ok".into()
            } else {
                "error".into()
            }
        }
        Command::Assign { user } => {
            assign(
                gh,
                ctx,
                &user,
                t!(lang, "Assigned to @{user}.", "已指派给 @{user}。"),
            )
            .await
        }
        Command::Claim => {
            let who = ctx.commenter.clone();
            assign(
                gh,
                ctx,
                &who,
                t!(lang, "@{who} claimed this.", "@{who} 已认领。"),
            )
            .await
        }
        Command::Unclaim => unclaim(gh, ctx).await,
        Command::Approve { on_behalf_of } => handle_approve(gh, cfg, ctx, on_behalf_of).await,
        Command::Reject => handle_reject(gh, cfg, ctx).await,
    }
}

/// Did GitHub actually end up with this login in the list it echoed back?
///
/// Logins are ASCII, so an ASCII-case comparison is exact here.
fn contains_login(list: &[String], who: &str) -> bool {
    list.iter().any(|l| l.eq_ignore_ascii_case(who))
}

/// `assign` / `claim`: assign one user and check that it took.
///
/// `success` is the caller's wording for the happy path; everything else is the
/// same three outcomes either way. The middle one is the point: the assignees
/// endpoint answers 201 and quietly leaves out a login it won't assign, so both
/// commands used to report success for an assignment that never happened.
async fn assign(gh: &Client, ctx: &CommentContext, user: &str, success: String) -> String {
    let lang = ctx.lang;
    let (msg, status) = match gh
        .add_assignees(&ctx.repo, ctx.pr_number, &[user.to_string()])
        .await
    {
        Ok(after) if contains_login(&after, user) => (success, "ok".to_string()),
        Ok(_) => (
            t!(
                lang,
                "⚠️ GitHub ignored the assignment of @{user} — they need write access to the repo, org membership, or a prior comment here.",
                "⚠️ GitHub 忽略了对 @{user} 的指派 —— 用户需要有仓库写权限、或是组织成员、或曾在此留言。"
            ),
            "ignored".to_string(),
        ),
        Err(e) => {
            tracing::warn!("assign @{user} on {}#{}: {e}", ctx.repo, ctx.pr_number);
            (
                t!(lang, "⚠️ Assignment failed: `{e}`", "⚠️ 指派失败: `{e}`"),
                format!("error: {e}"),
            )
        }
    };
    let _ = gh.post_issue_comment(&ctx.repo, ctx.pr_number, &msg).await;
    status
}

/// `unclaim`: release the commenter's own assignment.
///
/// Reads the assignees first. GitHub answers a removal that changed nothing with
/// the same 200 and the same list as a removal that worked, so "were you
/// assigned?" cannot be answered from the response — and the old code told a
/// user who had never been assigned that their assignment was released.
async fn unclaim(gh: &Client, ctx: &CommentContext) -> String {
    let lang = ctx.lang;
    let who = &ctx.commenter;

    match gh.list_assignees(&ctx.repo, ctx.pr_number).await {
        Ok(before) if !contains_login(&before, who) => {
            let _ = gh
                .post_issue_comment(
                    &ctx.repo,
                    ctx.pr_number,
                    &t!(
                        lang,
                        "@{who} wasn't assigned here, so there was nothing to release.",
                        "@{who} 本来就未被指派,无需释放。"
                    ),
                )
                .await;
            return "not-assigned".into();
        }
        Ok(_) => {}
        // A failed pre-check shouldn't block the removal; it only costs the
        // ability to distinguish the two outcomes, so say less rather than
        // refusing to act.
        Err(e) => tracing::warn!(
            "could not read assignees of {}#{} before unclaim: {e}",
            ctx.repo,
            ctx.pr_number
        ),
    }

    let (msg, status) = match gh
        .remove_assignees(&ctx.repo, ctx.pr_number, &[who.clone()])
        .await
    {
        Ok(after) if !contains_login(&after, who) => (
            t!(
                lang,
                "@{who} released the assignment.",
                "@{who} 已释放指派。"
            ),
            "ok".to_string(),
        ),
        Ok(_) => (
            t!(
                lang,
                "⚠️ GitHub accepted the request but @{who} is still assigned.",
                "⚠️ GitHub 接受了请求,但 @{who} 仍在指派列表中。"
            ),
            "not-removed".to_string(),
        ),
        Err(e) => {
            tracing::warn!("unclaim @{who} on {}#{}: {e}", ctx.repo, ctx.pr_number);
            (
                t!(
                    lang,
                    "⚠️ Could not release the assignment: `{e}`",
                    "⚠️ 释放失败: `{e}`"
                ),
                format!("error: {e}"),
            )
        }
    };
    let _ = gh.post_issue_comment(&ctx.repo, ctx.pr_number, &msg).await;
    status
}

/// `r? @user` — ask for a review, and report each half separately.
///
/// Two endpoints with two independent outcomes. The review *request* is what
/// GitHub shows under "Reviewers" and what a required-review rule counts; the
/// assignment is what appears in the sidebar. They fail independently — a user
/// with only read access can be assigned but not requested — and the old code
/// called only the assignment while the reply claimed the request.
///
/// Returns `ok` when every call that was attempted succeeded, `error` when none
/// did, and `partial` in between, so the log distinguishes "half of it worked"
/// from "none of it did".
async fn request_review(gh: &Client, ctx: &CommentContext, user: &str) -> String {
    let lang = ctx.lang;
    let users = [user.to_string()];
    let mut lines: Vec<String> = Vec::new();
    let mut good = 0usize;
    let mut total = 0usize;

    // An issue has no reviewers at all, so there is nothing to request and the
    // assignment is the whole action. Skipped rather than attempted: the
    // endpoint is under `/pulls/`, so on an issue it is a guaranteed 404.
    if ctx.is_pr {
        total += 1;
        match gh.request_reviewers(&ctx.repo, ctx.pr_number, &users).await {
            Ok(after) if contains_login(&after, user) => {
                good += 1;
                lines.push(t!(
                    lang,
                    "✅ Requested a review from @{user}.",
                    "✅ 已请求 @{user} 审查。"
                ));
            }
            Ok(_) => lines.push(t!(
                lang,
                "⚠️ GitHub accepted the request but @{user} is not listed as a reviewer.",
                "⚠️ GitHub 接受了请求,但 @{user} 未出现在 reviewer 列表中。"
            )),
            // 422 is GitHub's way of saying this user is not eligible, which is
            // an answer rather than a malfunction — so it gets the explanation,
            // not a raw error string.
            Err(GhError::Api { status: 422, .. }) => lines.push(t!(
                lang,
                "⚠️ @{user} can't be a reviewer on this PR — they need read access to the repo, and they can't have authored it.",
                "⚠️ @{user} 无法成为本 PR 的 reviewer —— 需要有仓库读权限,且不能是本 PR 作者。"
            )),
            Err(e) => {
                tracing::warn!(
                    "request review from @{user} on {}#{}: {e}",
                    ctx.repo,
                    ctx.pr_number
                );
                lines.push(t!(
                    lang,
                    "⚠️ Review request failed: `{e}`",
                    "⚠️ 请求审查失败: `{e}`"
                ));
            }
        }
    }

    total += 1;
    match gh.add_assignees(&ctx.repo, ctx.pr_number, &users).await {
        Ok(after) if contains_login(&after, user) => {
            good += 1;
            lines.push(if ctx.is_pr {
                t!(lang, "✅ Assigned @{user} 🙏", "✅ 已指派 @{user} 🙏")
            } else {
                t!(
                    lang,
                    "✅ Assigned @{user} — an issue has no reviewers, so this is an assignment 🙏",
                    "✅ 已指派 @{user} —— issue 没有 reviewer,这里只是指派 🙏"
                )
            });
        }
        Ok(_) => lines.push(t!(
            lang,
            "⚠️ GitHub ignored the assignment of @{user} — they need write access to the repo, org membership, or a prior comment here.",
            "⚠️ GitHub 忽略了对 @{user} 的指派 —— 用户需要有仓库写权限、或是组织成员、或曾在此留言。"
        )),
        Err(e) => {
            tracing::warn!("assign @{user} on {}#{}: {e}", ctx.repo, ctx.pr_number);
            lines.push(t!(
                lang,
                "⚠️ Assignment failed: `{e}`",
                "⚠️ 指派失败: `{e}`"
            ));
        }
    }

    let _ = gh
        .post_issue_comment(&ctx.repo, ctx.pr_number, &lines.join("\n"))
        .await;
    match good {
        0 => "error".into(),
        g if g == total => "ok".into(),
        _ => "partial".into(),
    }
}

/// ready/author/blocked: add one status label, remove its siblings.
async fn set_status_label(gh: &Client, cfg: &Config, ctx: &CommentContext, cmd: Command) -> String {
    let lang = ctx.lang;
    let (add, label_desc) = match cmd {
        Command::Ready => (
            &cfg.label_waiting_review,
            lang.pick("waiting for review", "等待审查"),
        ),
        Command::Author => (
            &cfg.label_waiting_author,
            lang.pick("waiting on the author", "等待作者"),
        ),
        Command::Blocked => (&cfg.label_blocked, lang.pick("blocked", "受阻")),
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
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                &t!(lang, "⚠️ Could not label: `{e}`", "⚠️ 打标签失败: `{e}`"),
            )
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
                &t!(
                    lang,
                    "Status updated: **{label_desc}** (`{add}`).",
                    "状态已更新: **{label_desc}**(`{add}`)。"
                ),
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
    let lang = ctx.lang;
    // 1. commenter must have write/maintain/admin
    let perm = match gh.collaborator_permission(&ctx.repo, &ctx.commenter).await {
        Ok(p) => p,
        Err(e) => {
            let _ = gh
                .post_issue_comment(
                    &ctx.repo,
                    ctx.pr_number,
                    &t!(
                        lang,
                        "⚠️ Could not check permissions: `{e}`",
                        "⚠️ 无法校验权限: `{e}`"
                    ),
                )
                .await;
            return format!("error: {e}");
        }
    };
    if !matches!(perm.as_str(), "admin" | "maintain" | "write") {
        let commenter = &ctx.commenter;
        let _ = gh
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                &t!(
                    lang,
                    "⚠️ `r+` from @{commenter} refused: write access or above is required (currently: {perm}).",
                    "⚠️ @{commenter} 的 `r+` 被拒绝:需要仓库 write 及以上权限(当前: {perm})。"
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
                &t!(
                    lang,
                    "⚠️ Cannot approve your own PR (`{credited}` authored it).",
                    "⚠️ 不能审批自己的 PR(`{credited}` 是本 PR 作者)。"
                ),
            )
            .await;
        return "self-approve".into();
    }

    // 3. post APPROVE review, crediting the human. Kept in English in both
    //    cases: this line is the audit trail for who approved what, and it is
    //    also what shows up in GitHub's review list.
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
                .post_issue_comment(
                    &ctx.repo,
                    ctx.pr_number,
                    &t!(
                        lang,
                        "⚠️ Relayed approval failed: `{e}`",
                        "⚠️ 代审批失败: `{e}`"
                    ),
                )
                .await;
            format!("error: {e}")
        }
    }
}

/// r-: withdraw — dismiss our own previous APPROVE reviews.
async fn handle_reject(gh: &Client, _cfg: &Config, ctx: &CommentContext) -> String {
    let lang = ctx.lang;
    let reviews = match gh.list_pr_reviews(&ctx.repo, ctx.pr_number).await {
        Ok(r) => r,
        Err(e) => {
            let _ = gh
                .post_issue_comment(
                    &ctx.repo,
                    ctx.pr_number,
                    &t!(
                        lang,
                        "⚠️ Could not list reviews: `{e}`",
                        "⚠️ 列出审查失败: `{e}`"
                    ),
                )
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
                lang.pick(
                    "⚠️ Cannot determine the bot's own identity, so the approval can't be withdrawn (check APP_SLUG / BOT_NAME).",
                    "⚠️ 无法确定 bot 自身身份,无法撤回审批(请检查 APP_SLUG / BOT_NAME)。",
                ),
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
                lang.pick(
                    "No bot approval to withdraw (there was no `r+` on this PR).",
                    "没有可撤回的 bot 审批(此前未在本 PR 上 `r+`)。",
                ),
            )
            .await;
        return "nothing-to-dismiss".into();
    }

    let found = mine.len();
    let mut dismissed = 0usize;
    let mut last_err: Option<String> = None;
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
            tracing::warn!("dismiss review {id} on {}: {e}", ctx.repo);
            last_err = Some(e.to_string());
        } else {
            dismissed += 1;
        }
    }

    let who = &ctx.commenter;
    // Every dismissal failing used to be reported as "withdrew 0 approval(s)"
    // with status `ok` — the approval was still standing and both the user and
    // the log said the command had worked.
    if dismissed == 0 {
        let e = last_err.unwrap_or_else(|| "unknown".into());
        let _ = gh
            .post_issue_comment(
                &ctx.repo,
                ctx.pr_number,
                &t!(
                    lang,
                    "❌ Could not withdraw the approval ({found} found, none dismissed): `{e}`. It is still standing.",
                    "❌ 撤回审批失败(找到 {found} 个,全部失败): `{e}`。审批仍然有效。"
                ),
            )
            .await;
        return format!("error: {e}");
    }

    let _ = gh
        .post_issue_comment(
            &ctx.repo,
            ctx.pr_number,
            &if dismissed == found {
                t!(
                    lang,
                    "Withdrew {dismissed} bot approval(s) (r- by @{who}).",
                    "已撤回 {dismissed} 个 bot 审批(r- by @{who})。"
                )
            } else {
                t!(
                    lang,
                    "⚠️ Withdrew {dismissed} of {found} bot approval(s) (r- by @{who}); the rest failed.",
                    "⚠️ 已撤回 {found} 个 bot 审批中的 {dismissed} 个(r- by @{who}),其余失败。"
                )
            },
        )
        .await;
    if dismissed == found {
        "ok".into()
    } else {
        "partial".into()
    }
}
