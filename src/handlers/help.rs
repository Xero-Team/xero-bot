//! Render help from the same verified policy used by dispatch. No network reads,
//! raw configuration dump, recipient mentions, or permission grants live here.
use crate::config::cache::Snapshot;
use crate::config::repository::{
    CommandId, Comments, Event, EventAction, ManualMode, RepositoryConfig,
};
use crate::config::Config;
use crate::lang::Lang;
use crate::trigger_state::SessionWake;

pub(crate) enum HelpSession {
    Active {
        expires_at: i64,
        remaining_secs: i64,
    },
    Inactive,
    Unavailable,
}
impl HelpSession {
    /// Use source time and the effective TTL, never webhook arrival time.
    pub(crate) fn from_wake(wake: Option<&SessionWake>, ttl_days: u16, now: i64) -> Self {
        let Some(wake) = wake else {
            return Self::Inactive;
        };
        let expires_at = wake
            .source_at
            .saturating_add(i64::from(ttl_days) * 86_400_000);
        if wake.source_at > now || expires_at <= now {
            Self::Inactive
        } else {
            Self::Active {
                expires_at,
                remaining_secs: (expires_at - now) / 1000,
            }
        }
    }
}

/// Escape and bound repository-controlled display values. In particular, help
/// must not notify the configured CC list or interpret globs as Markdown/HTML.
fn value(text: &str) -> String {
    let mut escaped = String::from("<code>");
    for ch in text.chars().take(48) {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '|' => escaped.push_str("&#124;"),
            '`' => escaped.push_str("&#96;"),
            '*' => escaped.push_str("&#42;"),
            '_' => escaped.push_str("&#95;"),
            '[' => escaped.push_str("&#91;"),
            ']' => escaped.push_str("&#93;"),
            '\\' => escaped.push_str("&#92;"),
            '@' => escaped.push('＠'),
            ch if ch.is_control() => escaped.push(' '),
            ch => escaped.push(ch),
        }
    }
    if text.chars().count() > 48 {
        escaped.push('…');
    }
    escaped.push_str("</code>");
    escaped
}
/// Preview at most two escaped globs and count the remaining entries.
fn patterns(items: &[String]) -> String {
    let mut shown = items
        .iter()
        .take(2)
        .map(|s| value(s))
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > 2 {
        shown.push_str(&format!(" … (+{})", items.len() - 2));
    }
    if shown.is_empty() {
        shown.push('—');
    }
    shown
}
/// Use the exact TOML event spelling so help can be compared with configuration.
fn event_name(event: Event) -> &'static str {
    match event {
        Event::PullRequestOpened => "pull_request.opened",
        Event::PullRequestSynchronize => "pull_request.synchronize",
        Event::IssueOpened => "issues.opened",
    }
}
/// Keep policy names untranslated to match the accepted TOML values.
fn mode_name(mode: ManualMode) -> &'static str {
    match mode {
        ManualMode::Disabled => "disabled",
        ManualMode::NoMention => "no_mention",
        ManualMode::MentionOnce => "mention_once",
        ManualMode::AlwaysMention => "always_mention",
    }
}
/// Describe the action and its key restrictions independently of its trigger mode.
fn description(id: CommandId, lang: Lang) -> &'static str {
    let (en, zh) = match id {
        CommandId::Claim => ("Assign yourself", "认领，指派给自己"),
        CommandId::Unclaim => ("Release your assignment", "释放自己的指派"),
        CommandId::Cc => ("`cc @user…`: notify users", "`cc @user…`：通知用户"),
        CommandId::RequestReview => (
            "`r? @user`: request a PR review; assign on an Issue",
            "`r? @user`：PR 请求审阅；Issue 指派",
        ),
        CommandId::Ready => (
            "Waiting for review; on PRs also re-request/ping reviewers",
            "等待审阅；PR 上还会重新请求/通知审阅者",
        ),
        CommandId::Approve => (
            "Relay APPROVE; commenter needs write+, no self-approval",
            "提交 APPROVE；评论者须有 write+ 权限，禁止自我审批",
        ),
        CommandId::Reject => (
            "Withdraw bot approval / queue entry; requires write+",
            "撤回 bot 审批/出队；须有 write+ 权限",
        ),
        CommandId::Review => (
            "Incremental AI review (COMMENT only)",
            "增量 AI 审查（仅 COMMENT）",
        ),
        CommandId::Codeql => (
            "Report existing CodeQL alerts for this change",
            "报告与本次改动相关的现有 CodeQL 告警",
        ),
        CommandId::Label => (
            "`label +a -b`: add/remove labels",
            "`label +a -b`：添加/移除标签",
        ),
        CommandId::Assign => ("`assign @user`: assign a user", "`assign @user`：指派用户"),
        CommandId::Author => ("Waiting on author", "等待作者"),
        CommandId::Blocked => ("Mark blocked", "标记受阻"),
        CommandId::Ping => ("Health check", "健康检查"),
        CommandId::Help => (
            "Show effective policy and session status",
            "显示有效策略与会话状态",
        ),
        CommandId::Queue => ("Show merge queue", "查看合并队列"),
    };
    lang.pick(en, zh)
}
/// Render every command from the supplied policy and source-checked session evidence.
fn manual(
    policy: &Comments,
    bot: &str,
    lang: Lang,
    on_behalf: bool,
    session: &HelpSession,
) -> String {
    let mut text = format!(
        "### {}\n\n",
        lang.pick("xero-bot commands", "xero-bot 命令参考")
    );
    text.push_str(lang.pick(
        "`disabled`: denied through every entry point, including aliases and automatic rules. `no_mention`: no mention needed. `mention_once`: explicit mention or an earlier unexpired session. `always_mention`: mention on every call. **Mentioning the bot never grants repository permissions.**\n\n",
        "`disabled`：所有入口永久拒绝，包括别名和自动规则。`no_mention`：无需 @。`mention_once`：显式 @ 或更早且未过期的会话。`always_mention`：每次调用都须 @。**@ 机器人不会授予仓库权限。**\n\n"));
    text.push_str(&format!("{} `@{bot} help` {}\n\n", lang.pick("Use", "使用"), lang.pick("with your deployment's bot name. Bare words require a complete command block (`take; cc @alice`); prose such as `claim 是什么意思？` or `review 一下` does not run. Each part of a combined command has its own gate.", "时请使用本部署的机器人名。裸单词要求完整指令块（`take; cc @alice`）；`claim 是什么意思？`、`review 一下` 等散文不会执行。组合中的每个子指令单独检查门槛。")));
    text.push_str(lang.pick(
        "| Command / aliases | Manual mode | Scope | Action |\n|---|---|---|---|\n",
        "| 命令 / 别名 | 手动模式 | 范围 | 动作 |\n|---|---|---|---|\n",
    ));
    for id in CommandId::ALL {
        let aliases = id
            .aliases()
            .iter()
            .map(|alias| format!("`{alias}`"))
            .collect::<Vec<_>>()
            .join(" / ");
        text.push_str(&format!(
            "| {aliases} | `{}` | {} | {} |\n",
            mode_name(policy.mode(id)),
            if id.requires_pr() {
                lang.pick("PR only", "仅 PR")
            } else {
                "Issue / PR"
            },
            description(id, lang)
        ));
    }
    text.push('\n');
    text.push_str(if on_behalf {
        lang.pick("`r= @user`, `r+ as @user` and `r+ @user` credit another user; that user also needs write+. All use the `r+` mode above.\n\n", "`r= @user`、`r+ as @user`、`r+ @user` 以他人名义审批，该用户也须有 write+ 权限。三者共用上表的 `r+` 模式。\n\n")
    } else {
        lang.pick("`r= @user`, `r+ as @user` and `r+ @user` are **disabled in this deployment** (`R_PLUS_ALLOW_ON_BEHALF=true` enables them); plain `r+` still follows the mode above.\n\n", "`r= @user`、`r+ as @user`、`r+ @user` **在本部署中关闭**（`R_PLUS_ALLOW_ON_BEHALF=true` 可开启）；普通 `r+` 仍遵守上表模式。\n\n")
    });
    text.push_str(&format!(
        "#### {}\n\n",
        lang.pick("Your session", "你的会话")
    ));
    match session {
        HelpSession::Active { expires_at, remaining_secs } => {
            let expires = chrono::DateTime::from_timestamp_millis(*expires_at).expect("validated GitHub date").format("%Y-%m-%d %H:%M:%S UTC");
            text.push_str(&crate::t!(lang, "Active for later comments; {remaining_secs} seconds remaining, expires {expires}.\n\n", "对后续评论有效；剩余 {remaining_secs} 秒，到期时间 {expires}。\n\n"));
        }
        HelpSession::Inactive => text.push_str(lang.pick("No applicable unexpired session was confirmed. A new valid explicit mention is required for `mention_once`.\n\n", "未确认适用且未过期的会话。`mention_once` 需要新发有效的显式 @。\n\n")),
        HelpSession::Unavailable => text.push_str(lang.pick("Session storage is unavailable; session validity cannot be confirmed.\n\n", "会话存储不可用，无法确认会话有效性。\n\n")),
    }
    let ttl = policy.ttl_days;
    text.push_str(&crate::t!(lang,
        "Scope: same installation, repository, Issue/PR thread and GitHub user. TTL: {ttl} days from the source time of a valid explicit mention that passed applicability/authorization checks; only explicit mentions renew it. Bare calls use evidence strictly earlier than their source comment, including within combined comments. Edits do not wake; deletion does not revoke; closing/reopening does not renew. Upgrades do not import old mentions.\n\n",
        "范围：同一 installation、仓库、Issue/PR 线程及 GitHub 用户。TTL：通过适用性/授权预检的有效显式 @ 源时间起 {ttl} 天，仅显式 @ 续期。裸调用只使用源评论之前的证据，组合评论也不例外。编辑不唤醒，删除不撤销，关闭/重开不续期；升级不导入旧唤醒。\n\n"));
    text
}

/// Direct handler callers lack repository/session evidence. Label defaults as a
/// reference rather than presenting them as this repository's effective policy.
pub fn help_text(bot: &str, queue: bool, lang: Lang, on_behalf: bool) -> String {
    let mut text = lang
        .pick(
            "Built-in defaults — reference only, repository policy not verified here.\n\n",
            "内置默认值——仅供参考，此处未确认仓库策略。\n\n",
        )
        .to_owned();
    text.push_str(&manual(
        &Comments::default(),
        bot,
        lang,
        on_behalf,
        &HelpSession::Unavailable,
    ));
    text.push_str(&super::queue_note(queue, lang));
    text
}

/// Only a Ready snapshot with a valid comment domain reaches this renderer.
pub(crate) fn repository_help(
    snapshot: &Snapshot,
    cfg: &Config,
    lang: Lang,
    session: &HelpSession,
) -> String {
    let policy = snapshot
        .config
        .comments
        .as_ref()
        .expect("dispatch checked comment policy");
    let branch = value(&snapshot.default_branch);
    let commit = value(&snapshot.commit_sha);
    let mut text = crate::t!(lang,
        "Verified configuration from default branch {branch} at {commit} (cache window: 60 seconds). Source: `.github/xero-bot.toml`.\n\n",
        "已验证默认分支 {branch} 的配置，提交 {commit}（缓存窗口：60 秒）。来源：`.github/xero-bot.toml`。\n\n");
    if snapshot.blob_sha.is_none() {
        text.push_str(lang.pick(
            "File absent: built-in defaults apply; no automatic rules.\n\n",
            "文件不存在：使用内置默认值，无自动规则。\n\n",
        ));
    }
    text.push_str(&manual(
        policy,
        &cfg.bot_name,
        lang,
        cfg.r_plus_allow_on_behalf,
        session,
    ));
    text.push_str(&automatic(&snapshot.config, cfg, lang));
    text.push_str(&super::queue_note(cfg.merge_queue_enabled, lang));
    text
}
/// Summarize independent subscriptions with deployment vetoes and bounded previews.
/// Recipient counts describe configuration, never delivered notifications or free slots.
fn automatic(config: &RepositoryConfig, cfg: &Config, lang: Lang) -> String {
    let mut text = format!(
        "#### {}\n\n",
        lang.pick("Automatic creation rules", "自动创建规则")
    );
    text.push_str(lang.pick("Independent opt-in subscriptions; no session or manual permission change. AI review publishes COMMENT, never APPROVE or queue entry. Runtime checks still apply.\n\n", "独立的显式订阅，不建立会话或改变手动权限。AI review 只发布 COMMENT，不批准或入队；执行时仍须通过检查。\n\n"));
    match &config.events {
        Err(problem) => text.push_str(&format!("{}\n\n", problem.message(lang))),
        Ok(rules) if rules.is_empty() => {
            text.push_str(lang.pick("None configured.\n\n", "未配置规则。\n\n"))
        }
        Ok(rules) => {
            for (index, rule) in rules.iter().enumerate() {
                match rule
                    .value
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|r| r.action.validate_deployment(cfg).map(|()| r))
                {
                    Ok(rule) => {
                        let action = match &rule.action {
                            EventAction::Review => "review (COMMENT)".into(),
                            EventAction::Codeql => "codeql".into(),
                            EventAction::AddLabels(labels) => {
                                format!("label.add ({})", labels.len())
                            }
                        };
                        text.push_str(&format!(
                            "- {}: `{}` → {action}\n",
                            value(&rule.id),
                            event_name(rule.event)
                        ));
                    }
                    Err(problem) => {
                        text.push_str(&format!("- #{}: {}\n", index + 1, problem.message(lang)))
                    }
                }
            }
        }
    }
    text.push_str(&format!(
        "\n#### {}\n\n",
        lang.pick("PR path rules", "PR 路径规则")
    ));
    text.push_str(lang.pick("PR only (including drafts); complete current base/head diff, case-sensitive root-relative globs. Renames check both paths; deletions check the old path; generated files participate unless excluded. Incomplete or >3000-file diffs cause no partial actions. Existing labels only; queue/CodeQL control labels are forbidden.\n\n", "仅 PR（含草稿）；完整当前 base/head diff，glob 从仓库根匹配且区分大小写。改名检查新旧路径，删除检查旧路径，生成文件参与匹配，除非显式排除。diff 不完整或超过 3000 文件时不执行部分动作。只加已有标签，禁止队列/CodeQL 控制标签。\n\n"));
    match &config.paths {
        Err(problem) => text.push_str(&format!("{}\n\n", problem.message(lang))),
        Ok(paths) => {
            let events = paths
                .events
                .iter()
                .map(|e| event_name(*e))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|event| format!("`{event}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let budget = paths.max_cc_users_per_pr;
            text.push_str(&crate::t!(lang, "Events: {events}. Lifetime CC budget: **{budget}** distinct people per installation/repository/PR (0 disables notifications).\n\n", "事件：{events}。每个 installation/repository/PR 生命周期 CC 总预算：**{budget}** 位不同用户（0 关闭通知）。\n\n"));
            if paths.rules.is_empty() {
                text.push_str(lang.pick("None configured.\n\n", "未配置规则。\n\n"));
            }
            for (index, rule) in paths.rules.iter().enumerate() {
                match &rule.value {
                    Ok(rule) if rule.validate_deployment(cfg).is_ok() => text.push_str(&format!(
                        "- {}: include {}; exclude {}; label.add: {}; cc: {}\n",
                        value(&rule.id),
                        patterns(&rule.include),
                        patterns(&rule.exclude),
                        rule.labels.len(),
                        rule.cc.len()
                    )),
                    result => {
                        let problem = match result {
                            Err(p) => p.clone(),
                            Ok(r) => r.validate_deployment(cfg).unwrap_err(),
                        };
                        text.push_str(&format!("- #{}: {}\n", index + 1, problem.message(lang)));
                    }
                }
            }
        }
    }
    text.push_str(lang.pick("\nCC lists are explicit personal logins. All rules share the lifetime budget; recipients are deduplicated and selected in login order, aggregated in one comment. Excess recipients are only counted, never mentioned. Pushes, configuration changes, deleted comments and restarts do not reset it. Counts above describe configuration, not remaining budget or delivery success. Unknown writes pause for operator reconciliation.\n\n", "\nCC 仅允许显式个人 login。所有规则共享终身预算，名单去重后按 login 顺序选择，聚合为一条评论；超额者只计数，不再提及。push、配置变化、删评论和重启均不重置预算。上方人数表示配置名单数量，不代表剩余额度或投递成功。不确定写入暂停，等待人工核对。\n\n"));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Remaining validity follows current TTL without reviving expired or future evidence.
    #[test]
    fn session_remaining_uses_effective_ttl_and_does_not_revive_expired_or_future_wakes() {
        let wake = SessionWake {
            installation_id: 1,
            repository_id: 2,
            thread_number: 3,
            user_id: 4,
            comment_id: 5,
            source_at: 1_000_000,
        };
        let now = wake.source_at + 86_400_000;
        assert!(matches!(
            HelpSession::from_wake(Some(&wake), 30, now),
            HelpSession::Active {
                remaining_secs: 2_505_600,
                ..
            }
        ));
        assert!(matches!(
            HelpSession::from_wake(Some(&wake), 1, now),
            HelpSession::Inactive
        ));
        assert!(matches!(
            HelpSession::from_wake(Some(&wake), 30, wake.source_at - 1),
            HelpSession::Inactive
        ));
        assert!(matches!(
            HelpSession::from_wake(None, 30, now),
            HelpSession::Inactive
        ));
    }

    /// Both translations must describe the same effective policy and deployment restrictions.
    #[test]
    fn both_languages_share_all_aliases_modes_scopes_and_relay_switch() {
        let policy = RepositoryConfig::parse("[command_triggers]\ntake={mode='disabled'}\ncc={mode='always_mention'}\nhelp={mode='no_mention'}\n[command_sessions]\nttl_days=7", "example/project").unwrap().comments.unwrap();
        for lang in [Lang::En, Lang::Zh] {
            let text = manual(&policy, "bot", lang, false, &HelpSession::Inactive);
            for id in CommandId::ALL {
                let row = text
                    .lines()
                    .find(|line| line.starts_with(&format!("| `{}`", id.name())))
                    .unwrap();
                assert!(row.contains(mode_name(policy.mode(id))));
                for alias in id.aliases() {
                    assert!(row.contains(&format!("`{alias}`")));
                }
                assert_eq!(row.contains("Issue / PR"), !id.requires_pr());
            }
            assert!(text.contains("7"));
            assert!(text.contains("R_PLUS_ALLOW_ON_BEHALF=true"));
            assert!(
                !manual(&policy, "bot", lang, true, &HelpSession::Unavailable)
                    .contains("R_PLUS_ALLOW_ON_BEHALF")
            );
        }
    }

    /// Repository-controlled text cannot inject mentions, markup or an oversized help reply.
    #[test]
    fn automatic_help_never_echoes_private_inputs_or_mentions_recipients_and_bounds_display() {
        let mut cfg = Config::from_env();
        cfg.codeql_label = "codeql".into();
        let text = r#"
[[event_triggers]]
id = "@outsider <b>|`"
event = "issues.opened"
command = "label"
add = ["codeql"]
[[path_triggers.rules]]
id = "@outsider <b>|`"
include = ["src/@outsider*.rs"]
cc = ["private-recipient"]
[ idle_workflows ]
enabled = false
"#;
        let config = RepositoryConfig::parse(text, "example/project").unwrap();
        for lang in [Lang::En, Lang::Zh] {
            let rendered = automatic(&config, &cfg, lang);
            assert!(rendered.contains("InvalidRule"));
            assert!(!rendered.contains('@'));
            assert!(!rendered.contains("<b>"));
            assert!(!rendered.contains("private-recipient"));
            assert!(rendered.contains("＠outsider"));
        }
        let rules = (0..32).map(|i| format!("[[path_triggers.rules]]\nid='{i}{}'\ninclude=['{}','{}','{}']\ncc=['private-recipient']\n", "|".repeat(300), "&".repeat(500), "&".repeat(500), "&".repeat(500))).collect::<String>();
        let rules = format!(
            "[path_triggers]\nevents={}\n{rules}",
            serde_json::to_string(&vec!["pull_request.opened"; 2000]).unwrap()
        );
        let config = RepositoryConfig::parse(&rules, "example/project").unwrap();
        let rendered = automatic(&config, &cfg, Lang::En);
        assert!(rendered.len() < 60_000);
        assert_eq!(rendered.matches("`pull_request.opened`").count(), 1);
    }
}
