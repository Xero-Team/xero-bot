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

mod authorization;
pub(crate) use authorization::authorize;

/// The trailing note under the command table. With the merge queue on, it
/// documents r+'s second effect; without it, only the needs-rebase note —
/// one function rather than per-row flags, so the two deployments' helps
/// can't drift apart row by row.
fn queue_note(merge_queue_enabled: bool, lang: Lang) -> String {
    match lang {
        Lang::En => {
            if merge_queue_enabled {
                "A successful `r+` (or a web Approve by a write+ reviewer) **queues the PR for \
automatic merge** — batches are tested on the `staging` branch, then advanced to `main`. \
`r-` withdraws from the queue; `queue` shows it. Conflicted PRs are labelled \
`needs-rebase` automatically."
                    .to_string()
            } else {
                "_Conflicted PRs are labelled `needs-rebase` automatically, with a reminder._"
                    .to_string()
            }
        }
        Lang::Zh => {
            if merge_queue_enabled {
                "成功的 `r+`(或网页上 write+ 审阅者的 Approve)会**将 PR 加入自动合并队列** \
—— 批次先在 `staging` 分支上测试,全绿后推进 `main`。`r-` 撤回出队;`queue` 查看队列。\
冲突的 PR 会被自动打上 `needs-rebase` 标签。"
                    .to_string()
            } else {
                "_冲突的 PR 会被自动打上 `needs-rebase` 标签并提醒。_".to_string()
            }
        }
    }
}

mod help;
pub use help::help_text;
pub(crate) use help::{repository_help, HelpSession};

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
    handle_comment_with_help(gh, cfg, ctx, commands, diagnostics, None).await
}

/// Dispatch supplies help rendered from its verified repository/session evidence.
pub(crate) async fn handle_comment_with_help(
    gh: &Client,
    cfg: &Config,
    ctx: &CommentContext,
    commands: Vec<Command>,
    diagnostics: Vec<String>,
    help: Option<&str>,
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
        let r = crate::trigger_state::runtime::command(
            gh,
            &cmd,
            handle_one(gh, cfg, ctx, cmd.clone(), help),
        )
        .await;
        results.push(r);
    }
    results
}

async fn handle_one(
    gh: &Client,
    cfg: &Config,
    ctx: &CommentContext,
    cmd: Command,
    help: Option<&str>,
) -> String {
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
        Command::Help => {
            let fallback;
            let body = match help {
                Some(body) => body,
                None => {
                    fallback = help_text(
                        &cfg.bot_name,
                        cfg.merge_queue_enabled,
                        lang,
                        cfg.r_plus_allow_on_behalf,
                    );
                    &fallback
                }
            };
            labeled(
                "help reply",
                gh.post_issue_comment(&ctx.repo, ctx.pr_number, body).await,
            )
        }
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
        Command::Queue => {
            let status = crate::merge_queue::render_queue_status(gh, cfg, &ctx.repo, lang).await;
            labeled(
                "queue status reply",
                gh.post_issue_comment(&ctx.repo, ctx.pr_number, &status)
                    .await,
            )
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
            // GitHub authenticates the App, not the commenter. Never let a
            // general label command bypass approval/withdrawal authorization.
            if let Err(refusal) = authorize(
                gh,
                cfg,
                ctx,
                &Command::Label {
                    add: add.clone(),
                    remove: remove.clone(),
                },
            )
            .await
            {
                let _ = gh
                    .post_issue_comment(&ctx.repo, ctx.pr_number, &refusal.message)
                    .await;
                return refusal.status.into();
            }
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
        .remove_assignees(&ctx.repo, ctx.pr_number, std::slice::from_ref(who))
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
///
/// `ready` on a PR additionally notifies whoever is expected to review — see
/// [`notify_ready_reviewers`]; a label alone reaches nobody.
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
        .add_labels(&ctx.repo, ctx.pr_number, std::slice::from_ref(add))
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
        if let Command::Ready = cmd {
            // On a PR the label is half the job: it says "waiting for
            // review" but notifies nobody. The reviewers endpoint is what
            // pings a human, so find who that should be and request them.
            if ctx.is_pr {
                notify_ready_reviewers(gh, ctx).await;
            }
        }
        "ok".into()
    } else {
        "error".into()
    }
}

/// A bare `?r`/`ready` has to reach a reviewer somehow.
///
/// The order of attempts is who is already attached to the PR, because the
/// question "who should look at this?" has usually already been answered:
///
/// 1. **open review requests** — the PR's Reviewers field, whether filled by
///    this bot's `r?` or by hand in the GitHub UI. Re-requesting a reviewer
///    who is already listed re-pings them; this is the path that makes a
///    hand-picked reviewer actually receive the ready nudge.
/// 2. **past human reviewers** — anyone who left a CHANGES_REQUESTED or
///    APPROVED review. They asked for changes; "ready" is the moment they
///    asked for.
///
/// If neither names anyone, the reply says so and asks for `?r @user` — a
/// PR with no reviewer history has no one to guess.
async fn notify_ready_reviewers(gh: &Client, ctx: &CommentContext) {
    let lang = ctx.lang;
    let who = match gh.requested_reviewers(&ctx.repo, ctx.pr_number).await {
        Ok(users) if !users.is_empty() => users,
        Ok(_) => match gh.list_pr_reviews(&ctx.repo, ctx.pr_number).await {
            Ok(reviews) => {
                let mut seen: Vec<String> = Vec::new();
                for r in &reviews {
                    let state = r.get("state").and_then(|s| s.as_str()).unwrap_or("");
                    if state != "CHANGES_REQUESTED" && state != "APPROVED" {
                        continue;
                    }
                    let login = r
                        .pointer("/user/login")
                        .and_then(|l| l.as_str())
                        .unwrap_or("");
                    // The bot's own approval (r+) is not a reviewer to ping,
                    // and neither is the PR author or the person typing ?r.
                    if login.is_empty()
                        || crate::github::normalize_login(login).is_empty()
                        || login.ends_with("[bot]")
                        || crate::github::normalize_login(login)
                            == crate::github::normalize_login(&ctx.pr_author)
                        || login.eq_ignore_ascii_case(&ctx.commenter)
                        || seen.iter().any(|s| s.eq_ignore_ascii_case(login))
                    {
                        continue;
                    }
                    seen.push(login.to_string());
                }
                seen
            }
            Err(e) => {
                tracing::warn!(
                    "ready: past reviewers of {}#{}: {e}",
                    ctx.repo,
                    ctx.pr_number
                );
                Vec::new()
            }
        },
        Err(e) => {
            tracing::warn!(
                "ready: requested reviewers of {}#{}: {e}",
                ctx.repo,
                ctx.pr_number
            );
            Vec::new()
        }
    };

    if who.is_empty() {
        let body = t!(
            lang,
            "ℹ️ No reviewer is attached to this PR yet — say `?r @user` (or `r? @user`) \
to pick one and they'll be notified.",
            "ℹ️ 本 PR 还没有挂 reviewer —— 用 `?r @用户`(或 `r? @用户`)指定一个,对方会收到通知。"
        );
        let _ = gh.post_issue_comment(&ctx.repo, ctx.pr_number, &body).await;
        return;
    }

    // Re-requesting an already-listed reviewer is how GitHub nudges them, so
    // this POST is the notification, not a bookkeeping step. A failure here is
    // not fatal — the label was still set — but the reader deserves the truth
    // rather than a silent skip.
    match gh.request_reviewers(&ctx.repo, ctx.pr_number, &who).await {
        Ok(_) => {
            let mentions = who
                .iter()
                .map(|u| format!("@{u}"))
                .collect::<Vec<_>>()
                .join(" ");
            let body = t!(
                lang,
                "🔔 {mentions} — the PR is marked ready for review.",
                "🔔 {mentions} —— 本 PR 已标记为等待审查。"
            );
            let _ = gh.post_issue_comment(&ctx.repo, ctx.pr_number, &body).await;
        }
        Err(e) => {
            tracing::warn!(
                "ready: re-request of {who:?} on {}#{}: {e}",
                ctx.repo,
                ctx.pr_number
            );
            let body = t!(
                lang,
                "⚠️ The PR is marked ready, but notifying the reviewer(s) failed: `{e}`",
                "⚠️ 已标记为等待审查,但通知 reviewer 失败: `{e}`"
            );
            let _ = gh.post_issue_comment(&ctx.repo, ctx.pr_number, &body).await;
        }
    }
}

/// r+: permission-gated approval relay (merge-queue style).
///
/// The gates run cheapest-first, and that order is part of the guarantee: a
/// request that is refused on configuration or on who asked never reaches the
/// API at all, so a refusal cannot be turned into an information leak (whether
/// a login exists, what permission it holds) or into rate-limit pressure.
async fn handle_approve(
    gh: &Client,
    cfg: &Config,
    ctx: &CommentContext,
    on_behalf_of: Option<String>,
) -> String {
    let lang = ctx.lang;
    if let Err(refusal) = authorize(
        gh,
        cfg,
        ctx,
        &Command::Approve {
            on_behalf_of: on_behalf_of.clone(),
        },
    )
    .await
    {
        let _ = gh
            .post_issue_comment(&ctx.repo, ctx.pr_number, &refusal.message)
            .await;
        return refusal.status.into();
    }
    let credited = on_behalf_of.unwrap_or_else(|| ctx.commenter.clone());

    // 5. post APPROVE review, crediting the human. Kept in English in both
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
            // An approval is one vote and one vote only — the merge queue is
            // what turns it into a merge. Enqueuing after a successful relay
            // keeps r+ meaningful with the queue on; with it off, this is a
            // no-op. Failures are logged, not fatal: the approval itself went
            // through, and the queue will pick the PR up on the next r+.
            if cfg.merge_queue_enabled && ctx.is_pr {
                let outcome = crate::merge_queue::enqueue(gh, cfg, &ctx.repo, ctx.pr_number).await;
                tracing::info!(
                    "merge queue enqueue after r+ on {}#{}: {outcome}",
                    ctx.repo,
                    ctx.pr_number
                );
            }
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
async fn handle_reject(gh: &Client, cfg: &Config, ctx: &CommentContext) -> String {
    let lang = ctx.lang;
    if let Err(refusal) = authorize(gh, cfg, ctx, &Command::Reject).await {
        let _ = gh
            .post_issue_comment(&ctx.repo, ctx.pr_number, &refusal.message)
            .await;
        return refusal.status.into();
    }
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
    // Withdrawing the approval withdraws the merge request. A partial
    // dismissal leaves at least one APPROVE standing, so the queue keeps the
    // PR — the caller can retry r-.
    if cfg.merge_queue_enabled && dismissed == found && ctx.is_pr {
        let note = t!(
            lang,
            "➖ Removed from the merge queue (r- by @{who}).",
            "➖ 已移出合并队列(r- by @{who})。"
        );
        let status = crate::merge_queue::dequeue_pr(gh, cfg, &ctx.repo, ctx.pr_number, &note).await;
        tracing::info!(
            "merge queue dequeue after r- on {}#{}: {status}",
            ctx.repo,
            ctx.pr_number
        );
    }
    if dismissed == found {
        "ok".into()
    } else {
        "partial".into()
    }
}
