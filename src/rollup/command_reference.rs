//! Read-only command reference generated from the same vocabulary and policies as dispatch.
use super::{code, escape, page, WebState};
use crate::{
    admission,
    config::{
        cache::{RepositoryConfigCache, RepositoryKey},
        repository::{CommandId, Comments, ManualMode, RepositoryConfig, CONFIG_PATH},
        Config,
    },
    forgejo::ForgejoClient,
    github::Client,
    lang::Lang,
};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Provider {
    #[default]
    Github,
    Forgejo,
}
impl Provider {
    fn key(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Forgejo => "forgejo",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Forgejo => "Forgejo",
        }
    }
    fn bot(self, cfg: &Config) -> &str {
        match self {
            Self::Github => &cfg.bot_name,
            Self::Forgejo => &cfg.forgejo_bot_name,
        }
    }
}
#[derive(Default, Deserialize)]
pub(super) struct Options {
    lang: Option<String>,
    #[serde(default)]
    provider: Provider,
    project: Option<String>,
}
impl Options {
    fn lang(&self) -> Lang {
        if self.lang.as_deref() == Some("zh") {
            Lang::Zh
        } else {
            Lang::En
        }
    }
}

#[derive(Serialize)]
struct Source {
    path: &'static str,
    default_branch: String,
    commit: String,
    file_present: bool,
    cache_seconds: u64,
}
struct ProjectPolicy {
    config: RepositoryConfig,
    source: Source,
}
#[derive(Debug)]
enum LoadError {
    NotFound,
    Unavailable,
}
impl LoadError {
    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
    fn message(&self, lang: Lang) -> &'static str {
        match self {
            Self::NotFound => lang.pick("No public command policy is available for this project. Check the platform and owner/repository name.", "此项目没有可公开查询的指令策略，请检查平台和 owner/repository 名称。"),
            Self::Unavailable => lang.pick("The current project policy could not be verified. Retry later; built-in defaults are not this project's effective policy.", "暂时无法确认项目的当前配置，请稍后重试；内置默认值不能代表该项目的实际策略。"),
        }
    }
}

fn public_metadata(meta: &Value) -> Result<(), LoadError> {
    if meta["private"] != false || meta["archived"] != false {
        return Err(LoadError::NotFound);
    }
    Ok(())
}
async fn github_policy(
    cfg: &Config,
    gh: &Client,
    cache: &RepositoryConfigCache,
    key: RepositoryKey,
    name: &str,
) -> Result<ProjectPolicy, LoadError> {
    let meta =
        admission::github_repository(cfg, gh, key.installation_id, name, Some(key.repository_id))
            .await
            .map_err(|error| match error {
                crate::github::GhError::Api {
                    status: 401 | 403 | 404,
                    ..
                } => LoadError::NotFound,
                _ => LoadError::Unavailable,
            })?;
    public_metadata(&meta)?;
    let state = cache.load(gh, key, name).await;
    let snapshot = state.snapshot().map_err(|_| LoadError::Unavailable)?;
    snapshot
        .config
        .comments
        .as_ref()
        .map_err(|_| LoadError::Unavailable)?;
    Ok(ProjectPolicy {
        config: snapshot.config.clone(),
        source: Source {
            path: CONFIG_PATH,
            default_branch: snapshot.default_branch.clone(),
            commit: snapshot.commit_sha.clone(),
            file_present: snapshot.blob_sha.is_some(),
            cache_seconds: crate::config::cache::TTL_SECS,
        },
    })
}
async fn forgejo_policy(
    cfg: &Config,
    fj: &ForgejoClient,
    name: &str,
) -> Result<ProjectPolicy, LoadError> {
    let meta = admission::forgejo_repository(cfg, fj, name, None)
        .await
        .map_err(|error| match error {
            crate::forgejo::ForgejoError::Api {
                status: 401 | 403 | 404,
                ..
            } => LoadError::NotFound,
            _ => LoadError::Unavailable,
        })?;
    public_metadata(&meta)?;
    let snapshot = fj
        .repository_config_snapshot(name)
        .await
        .map_err(|_| LoadError::Unavailable)?;
    snapshot
        .config
        .comments
        .as_ref()
        .map_err(|_| LoadError::Unavailable)?;
    Ok(ProjectPolicy {
        config: snapshot.config,
        source: Source {
            path: CONFIG_PATH,
            default_branch: snapshot.default_branch,
            commit: snapshot.commit_sha,
            file_present: snapshot.file_present,
            cache_seconds: 0,
        },
    })
}
async fn load(cfg: &Config, provider: Provider, name: &str) -> Result<ProjectPolicy, LoadError> {
    let task = async {
        match provider {
            Provider::Github => {
                let (installation_id, repository_id) =
                    admission::github_identity(cfg, name).ok_or(LoadError::NotFound)?;
                let gh = Client::installation(cfg, installation_id, &cfg.app_slug)
                    .map_err(|_| LoadError::Unavailable)?;
                github_policy(
                    cfg,
                    &gh,
                    &RepositoryConfigCache::shared(),
                    RepositoryKey {
                        installation_id,
                        repository_id,
                    },
                    name,
                )
                .await
            }
            Provider::Forgejo => {
                if !admission::forgejo_scope(cfg, name, None) {
                    return Err(LoadError::NotFound);
                }
                let fj = ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)
                    .map_err(|_| LoadError::Unavailable)?;
                forgejo_policy(cfg, &fj, name).await
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), task)
        .await
        .map_err(|_| LoadError::Unavailable)?
}

#[derive(Serialize)]
struct Example {
    syntax: &'static str,
    invocation: String,
    available: Option<bool>,
    mention: &'static str,
    requirement: Option<&'static str>,
}
#[derive(Serialize)]
struct CommandReference {
    command: &'static str,
    syntax: String,
    aliases: &'static [&'static str],
    description: &'static str,
    requires_pr: bool,
    // Preserve the original API field while adding a stable TOML-compatible mode.
    trigger: String,
    mode: &'static str,
    mention: &'static str,
    github: bool,
    forgejo: bool,
    rollup_enabled: bool,
    available: Option<bool>,
    availability: &'static str,
    examples: Vec<Example>,
}
fn references(
    cfg: &Config,
    lang: Lang,
    project: Option<(&RepositoryConfig, Provider)>,
) -> Vec<CommandReference> {
    let defaults = Comments::default();
    let policy = project
        .map(|(p, _)| p.comments.as_ref().expect("verified comment domain"))
        .unwrap_or(&defaults);
    let provider = project.map(|(_, provider)| provider).unwrap_or_default();
    let rollup = cfg.rollup_enabled
        && project.is_some_and(|(p, _)| p.rollup.as_ref().is_ok_and(|p| p.enabled));
    CommandId::ALL
        .into_iter()
        .map(|id| {
            let mode = policy.mode(id);
            let queue_control = matches!(
                id,
                CommandId::Approve
                    | CommandId::Reject
                    | CommandId::Queue
                    | CommandId::Rollup
                    | CommandId::Try
                    | CommandId::Retry
            );
            let unavailable = if mode == ManualMode::Disabled {
                Some(lang.pick("Disabled by project", "项目已禁用"))
            } else if provider == Provider::Forgejo && !id.supports_forgejo() {
                Some(lang.pick("Unsupported on Forgejo", "Forgejo 暂不支持"))
            } else if cfg.rollup_enabled
                && queue_control
                && project.is_some_and(|(p, _)| p.rollup.is_err())
            {
                Some(lang.pick("Rollup policy invalid", "Rollup 配置无效"))
            } else if (id.needs_rollup()
                || (provider == Provider::Forgejo && id == CommandId::Queue))
                && !rollup
            {
                Some(lang.pick("Requires enabled Rollup", "需要开启 Rollup"))
            } else {
                None
            };
            let available = project.map(|_| unavailable.is_none());
            let availability = if project.is_some() {
                unavailable.unwrap_or(lang.pick("Available", "可用"))
            } else if id.needs_rollup() {
                lang.pick("Requires enabled Rollup", "需要项目开启 Rollup")
            } else if id == CommandId::Queue {
                lang.pick("Forgejo requires Rollup", "Forgejo 需要开启 Rollup")
            } else {
                lang.pick("Subject to project policy", "以项目配置为准")
            };
            let examples = id
                .examples()
                .iter()
                .map(|syntax| {
                    // Compound forms such as ?r @user cc @other are gated per
                    // parsed command, so a project's r?/cc override must also
                    // affect the displayed example, not just the ready row.
                    let parsed = crate::commands::parse_commands("bot", &format!("@bot {syntax}"));
                    let modes: Vec<_> = parsed
                        .commands
                        .iter()
                        .map(|c| policy.mode(c.id()))
                        .collect();
                    let example_mode = if modes.contains(&ManualMode::Disabled) {
                        ManualMode::Disabled
                    } else if modes.contains(&ManualMode::AlwaysMention) {
                        ManualMode::AlwaysMention
                    } else if modes.contains(&ManualMode::MentionOnce) {
                        ManualMode::MentionOnce
                    } else {
                        ManualMode::NoMention
                    };
                    let delegated = id == CommandId::Approve && syntax.contains('@');
                    let needs_rollup = id.needs_rollup()
                        || syntax.contains("rollup")
                        || (provider == Provider::Forgejo && id == CommandId::Queue);
                    let requirement = if example_mode == ManualMode::Disabled {
                        Some(lang.pick(
                            "A command in this example is disabled",
                            "此示例中的指令已禁用",
                        ))
                    } else if delegated && !cfg.r_plus_allow_on_behalf {
                        Some(lang.pick(
                            "On-behalf approval is disabled in this deployment",
                            "本部署未开启代他人审批",
                        ))
                    } else if needs_rollup
                        && (!cfg.rollup_enabled || (project.is_some() && !rollup))
                    {
                        Some(lang.pick("Requires enabled Rollup", "需要开启 Rollup"))
                    } else if needs_rollup && project.is_none() {
                        Some(lang.pick("Requires project Rollup", "需要项目开启 Rollup"))
                    } else {
                        None
                    };
                    Example {
                        syntax,
                        invocation: if example_mode == ManualMode::NoMention {
                            syntax.to_string()
                        } else {
                            format!("@{} {syntax}", provider.bot(cfg))
                        },
                        mention: example_mode.mention_label(lang),
                        available: if (delegated && !cfg.r_plus_allow_on_behalf)
                            || (needs_rollup && !cfg.rollup_enabled)
                        {
                            Some(false)
                        } else {
                            available.map(|a| a && requirement.is_none())
                        },
                        requirement,
                    }
                })
                .collect();
            CommandReference {
                command: id.name(),
                syntax: id.examples().join(" / "),
                aliases: id.aliases(),
                description: if project.is_some() && provider == Provider::Forgejo {
                    id.forgejo_description(lang)
                } else {
                    id.description(lang)
                },
                requires_pr: id.requires_pr(),
                trigger: format!("{mode:?}"),
                mode: mode.name(),
                mention: mode.mention_label(lang),
                github: true,
                forgejo: id.supports_forgejo(),
                rollup_enabled: if project.is_some() {
                    rollup
                } else {
                    cfg.rollup_enabled
                },
                available,
                availability,
                examples,
            }
        })
        .collect()
}
fn payload(cfg: &Config, lang: Lang, project: Option<(&str, Provider, &ProjectPolicy)>) -> Value {
    let policy = project.map(|(_, provider, p)| (&p.config, provider));
    json!({
        "version": env!("CARGO_PKG_VERSION"), "language": code(lang),
        "reference": if project.is_some() { "project" } else { "defaults" },
        "repository": project.map(|(name, _, _)| name),
        "provider": project.map(|(_, provider, _)| provider),
        "bot_name": project.map(|(_, provider, _)| provider.bot(cfg)),
        "source": project.map(|(_, _, p)| &p.source),
        "session_ttl_days": policy.map(|(p, _)| p.comments.as_ref().unwrap().ttl_days).unwrap_or(Comments::default().ttl_days),
        "commands": references(cfg, lang, policy),
    })
}

fn lookup(lang: Lang, name: &str, provider: Provider) -> String {
    let platforms = format!(
        r#"<option value="github"{}>GitHub</option><option value="forgejo"{}>Forgejo</option>"#,
        if provider == Provider::Github {
            " selected"
        } else {
            ""
        },
        if provider == Provider::Forgejo {
            " selected"
        } else {
            ""
        }
    );
    let locale = code(lang);
    let platform = lang.pick("Platform", "平台");
    let project = lang.pick("Project", "项目");
    let submit = lang.pick("View project commands", "查看项目指令");
    let name = escape(name);
    format!(
        r#"<form class="command-lookup" data-project-lookup data-current-project="{name}" action="/commands" method="get"><input type="hidden" name="lang" value="{locale}"><label>{platform}<select name="provider">{platforms}</select></label><label class="repository-input">{project}<select name="project" required disabled><option value="">{loading}</option></select></label><button type="submit" disabled>{submit}</button><button type="button" data-refresh disabled>{refresh}</button><p class="project-status" role="status" aria-live="polite">{loading}</p></form><details class="manual-project"><summary>{manual}</summary><form class="command-lookup" action="/commands" method="get"><input type="hidden" name="lang" value="{locale}"><label>{platform}<select name="provider">{platforms}</select></label><label class="repository-input">{project}<input name="project" value="{name}" placeholder="owner/repository" required maxlength="200"></label><button type="submit">{submit}</button></form></details><noscript><p>{no_script}</p></noscript><script src="/assets/projects.js" defer></script>"#,
        loading = lang.pick("Discovering projects…", "正在检测已接入的仓库…"),
        refresh = lang.pick("Refresh projects", "刷新仓库"),
        manual = lang.pick("Enter a project manually", "手动输入仓库"),
        no_script = lang.pick(
            "Enable JavaScript for automatic discovery, or expand the manual form above.",
            "启用 JavaScript 可自动检测仓库，也可以展开上方的手动输入。"
        ),
    )
}
fn instructions(cfg: &Config, lang: Lang, ttl: u16, provider: Option<Provider>) -> String {
    let example_bot = escape(provider.unwrap_or_default().bot(cfg));
    let bots = match provider {
        Some(p) => format!("{}: <code>@{}</code>", p.label(), escape(p.bot(cfg))),
        None => format!(
            "GitHub: <code>@{}</code> · Forgejo: <code>@{}</code>",
            escape(&cfg.bot_name),
            escape(&cfg.forgejo_bot_name)
        ),
    };
    format!("<section class=command-guide><p>{bots}</p><p>{}</p><p>{}</p><p>{}</p></section>",
        lang.pick("The @ rule means mentioning the bot. An @user argument (for example cc @user) mentions the recipient, not the bot. Bare commands must form a complete comment, separated by newlines or semicolons; every part of a compound command has its own policy.", "表中的 @ 指 @ 机器人；cc @user 等参数中的 @user 是目标用户。免 @ 指令须组成完整评论，可用换行或分号分隔；组合中的每条指令分别检查策略。"),
        lang.pick(&format!("For mention_once, first send an eligible explicit command whose effective mode is no_mention or mention_once and that passes applicability and authorization checks (for example <code>@{example_bot} help</code> under the default policy). Later comments by the same user in the same project, Issue/PR and platform can omit @ for {ttl} days. Only eligible explicit mentions open or renew a session. Commands in always_mention mode, such as <code>@{example_bot} r+</code> under the defaults, never open or renew a session, even when successful. On Forgejo, queue, review and codeql also never open or renew a session. This page cannot determine your session; use help in the thread."), &format!("mention_once 须先发送可用于唤醒的显式 @ 指令，其生效模式须为 no_mention 或 mention_once，并通过适用性与权限检查（例如默认策略下的 <code>@{example_bot} help</code>）。同一平台、项目、Issue/PR 内，同一用户的后续评论在 {ttl} 天会话有效期内免 @。只有符合唤醒条件的显式 @ 才会建立或续期会话。always_mention 模式的指令（如默认策略下的 <code>@{example_bot} r+</code>）即使执行成功，也不会建立或续期会话。Forgejo 的 queue、review 和 codeql 同样不会建立或续期会话。此页不判断个人会话状态，可在线程中使用 help 查询。")),
        lang.pick("Availability describes configured commands. Repository permissions, PR state and execution checks still apply; mentioning the bot grants no permissions.", "可用状态表示配置允许调用；执行时仍会检查仓库权限、PR 状态等条件，@ 不会授予权限。"))
}
fn table(cfg: &Config, lang: Lang, policy: Option<(&RepositoryConfig, Provider)>) -> String {
    let rows = references(cfg, lang, policy);
    let mut out = format!("<p>{}: {}</p><div class=table-wrap><table class=commands><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>",
        lang.pick("Registered commands", "注册指令"), rows.len(), lang.pick("Command / aliases", "指令 / 别名"),
        lang.pick("Action / examples", "作用 / 调用示例"), lang.pick("@ bot requirement", "是否需要 @ 机器人"),
        lang.pick("Scope", "适用范围"), lang.pick("Availability / platform", "可用状态 / 平台"));
    for row in rows {
        out.push_str(&format!("<tr data-command=\"{}\"><td>{}</td><td><p>{}</p><details><summary>{}</summary><ul class=command-examples>",
            escape(row.command), row.aliases.iter().map(|s| format!("<code>{}</code>", escape(s))).collect::<Vec<_>>().join("<br>"), escape(row.description), lang.pick("All syntax examples", "全部调用示例")));
        for example in &row.examples {
            out.push_str(&format!(
                "<li><code>{}</code><br><span class=muted>{}</span>{}</li>",
                escape(&example.invocation),
                example.mention,
                example
                    .requirement
                    .map(|r| format!(" <span class=muted>({})</span>", escape(r)))
                    .unwrap_or_default()
            ));
        }
        let platform = if policy.is_some() {
            ""
        } else if row.forgejo {
            "GitHub · Forgejo"
        } else {
            "GitHub"
        };
        out.push_str(&format!("</ul></details></td><td><span class=\"mention {}\">{}</span><br><code>{}</code></td><td>{}</td><td><span class=\"{}\">{}</span><br>{}</td></tr>",
            row.mode, row.mention, row.mode, if row.requires_pr { lang.pick("PR only", "仅 PR") } else { "Issue / PR" },
            if row.available == Some(false) { "unavailable" } else { "" }, row.availability, platform));
    }
    out.push_str("</tbody></table></div>");
    out
}
fn valid_name(name: &str) -> bool {
    let parts: Vec<_> = name.split('/').collect();
    parts.len() == 2
        && name.len() <= 200
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}
fn project_path(name: &str, provider: Provider) -> String {
    format!(
        "/commands/{}?provider={}",
        name.split('/')
            .map(crate::github::enc_seg)
            .collect::<Vec<_>>()
            .join("/"),
        provider.key()
    )
}
pub(super) async fn commands(State(s): State<WebState>, Query(q): Query<Options>) -> Response {
    let lang = q.lang();
    if let Some(name) = q
        .project
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if valid_name(name) {
            return Redirect::to(&format!(
                "{}&lang={}",
                project_path(name, q.provider),
                code(lang)
            ))
            .into_response();
        }
        return (
            StatusCode::BAD_REQUEST,
            page(
                lang.pick("Invalid project", "项目名称无效"),
                &format!(
                    "<h1>{}</h1><p>{}</p>{}",
                    lang.pick("Invalid project", "项目名称无效"),
                    lang.pick(
                        "Enter a project as owner/repository.",
                        "请按 owner/repository 格式输入项目。"
                    ),
                    lookup(lang, name, q.provider)
                ),
                "/commands",
                lang,
            ),
        )
            .into_response();
    }
    let mut body = format!("<h1>{}</h1><p>{}</p>", lang.pick("Command reference", "指令参考"), lang.pick("Complete built-in defaults from the running bot's command registry. Select a project to see its effective configuration.", "由运行版本的统一指令注册表生成完整默认参考。选择项目可查看实际生效的配置。"));
    body.push_str(&lookup(lang, "", q.provider));
    body.push_str(&instructions(
        &s.cfg,
        lang,
        Comments::default().ttl_days,
        None,
    ));
    body.push_str(&table(&s.cfg, lang, None));
    page(
        lang.pick("Commands", "指令"),
        &body,
        &format!("/commands?provider={}", q.provider.key()),
        lang,
    )
    .into_response()
}
pub(super) async fn command_data(
    State(s): State<WebState>,
    Query(q): Query<Options>,
) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(payload(&s.cfg, q.lang(), None)),
    )
}
fn project_body(
    cfg: &Config,
    name: &str,
    provider: Provider,
    p: &ProjectPolicy,
    lang: Lang,
) -> String {
    let policy = p.config.comments.as_ref().unwrap();
    let mut body = format!(
        "<h1>{} · {}</h1><p>{} · {}</p>",
        lang.pick("Project commands", "项目指令"),
        escape(name),
        provider.label(),
        lang.pick("Effective project configuration", "项目当前生效配置")
    );
    body.push_str(&lookup(lang, name, provider));
    if provider == Provider::Github {
        body.push_str(&format!(
            "<p><a href=\"/queue/{}?lang={}\">{}</a></p>",
            name.split('/')
                .map(crate::github::enc_seg)
                .collect::<Vec<_>>()
                .join("/"),
            code(lang),
            lang.pick("View project queue", "查看项目队列")
        ));
    }
    body.push_str(&format!(
        "<p class=muted>{}: <code>{}</code> · {}: <code>{}</code> · {}: <code>{}</code></p>",
        lang.pick("Source", "配置来源"),
        CONFIG_PATH,
        lang.pick("Default branch", "默认分支"),
        escape(&p.source.default_branch),
        lang.pick("Commit", "提交"),
        escape(&p.source.commit)
    ));
    if !p.source.file_present {
        body.push_str(&format!(
            "<p>{}</p>",
            lang.pick(
                "Configuration file confirmed absent: built-in defaults apply.",
                "已确认配置文件不存在，使用内置默认值。"
            )
        ));
    }
    if p.source.cache_seconds > 0 {
        body.push_str(&format!(
            "<p class=muted>{}</p>",
            lang.pick(
                "Configuration cache: up to 60 seconds. Reload this page to refresh.",
                "配置缓存最长 60 秒，重新加载页面可刷新。"
            )
        ));
    }
    body.push_str(&instructions(cfg, lang, policy.ttl_days, Some(provider)));
    body.push_str(&table(cfg, lang, Some((&p.config, provider))));
    body
}
pub(super) async fn project_commands(
    State(s): State<WebState>,
    Path((owner, repo)): Path<(String, String)>,
    Query(q): Query<Options>,
) -> Response {
    let name = format!("{owner}/{repo}");
    let lang = q.lang();
    let result = if valid_name(&name) {
        load(&s.cfg, q.provider, &name).await
    } else {
        Err(LoadError::NotFound)
    };
    let path = project_path(&name, q.provider);
    match result {
        Ok(p) => page(
            lang.pick("Project commands", "项目指令"),
            &project_body(&s.cfg, &name, q.provider, &p, lang),
            &path,
            lang,
        )
        .into_response(),
        Err(e) => (
            e.status(),
            page(
                lang.pick("Project commands", "项目指令"),
                &format!(
                    "<h1>{}</h1><p role=alert>{}</p>{}",
                    lang.pick("Project commands", "项目指令"),
                    e.message(lang),
                    lookup(lang, &name, q.provider)
                ),
                &path,
                lang,
            ),
        )
            .into_response(),
    }
}
pub(super) async fn project_command_data(
    State(s): State<WebState>,
    Path((owner, repo)): Path<(String, String)>,
    Query(q): Query<Options>,
) -> Response {
    let name = format!("{owner}/{repo}");
    let result = if valid_name(&name) {
        load(&s.cfg, q.provider, &name).await
    } else {
        Err(LoadError::NotFound)
    };
    let (status, data) = match result {
        Ok(p) => (
            StatusCode::OK,
            payload(&s.cfg, q.lang(), Some((&name, q.provider, &p))),
        ),
        Err(e) => (e.status(), json!({"error": e.message(q.lang())})),
    };
    (status, [(header::CACHE_CONTROL, "no-store")], Json(data)).into_response()
}

#[cfg(test)]
#[path = "command_reference_tests.rs"]
mod tests;
