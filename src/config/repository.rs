//! Repository policy contract. Parsing never performs actions or grants permissions.
//! Structural errors reject the document; semantic errors retain their domain/rule.
use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use crate::commands::Command;
use crate::idle_workflows::config::{self as idle, Rules};

pub const CONFIG_PATH: &str = ".github/xero-bot.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CommandId {
    Claim,
    Unclaim,
    Cc,
    RequestReview,
    Ready,
    Approve,
    Reject,
    Review,
    Codeql,
    Label,
    Assign,
    Author,
    Blocked,
    Ping,
    Help,
    Queue,
}

/// Single vocabulary shared with the parser. Sigils are tokenized separately.
pub const WORD_ALIASES: &[&str] = &[
    // Preserve parser suggestion priority when edit distances tie.
    "ping",
    "help",
    "commands",
    "review",
    "codeql",
    "ready",
    "reviewer",
    "author",
    "blocked",
    "claim",
    "take",
    "unclaim",
    "untake",
    "release",
    "release-assignment",
    "cc",
    "label",
    "relabel",
    "assign",
    "queue",
];

impl CommandId {
    pub const ALL: [Self; 16] = [
        Self::Claim,
        Self::Unclaim,
        Self::Cc,
        Self::RequestReview,
        Self::Ready,
        Self::Approve,
        Self::Reject,
        Self::Review,
        Self::Codeql,
        Self::Label,
        Self::Assign,
        Self::Author,
        Self::Blocked,
        Self::Ping,
        Self::Help,
        Self::Queue,
    ];

    /// Return the canonical TOML key used for diagnostics and policy lookup.
    pub fn name(self) -> &'static str {
        self.aliases()[0]
    }

    /// List every accepted spelling, with the canonical ID first.
    /// All spellings share one policy entry; arguments are not part of an alias.
    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Claim => &["claim", "take"],
            Self::Unclaim => &["unclaim", "untake", "release", "release-assignment"],
            Self::Cc => &["cc"],
            Self::RequestReview => &["r?"],
            Self::Ready => &["ready", "?r", "reviewer"],
            Self::Approve => &["r+", "r="],
            Self::Reject => &["r-"],
            Self::Review => &["review"],
            Self::Codeql => &["codeql"],
            Self::Label => &["label", "relabel"],
            Self::Assign => &["assign"],
            Self::Author => &["author"],
            Self::Blocked => &["blocked"],
            Self::Ping => &["ping"],
            Self::Help => &["help", "commands"],
            Self::Queue => &["queue"],
        }
    }

    /// Resolve an exact TOML command key; reject unknown spellings rather than defaulting.
    /// The comment lexer performs its own case normalization before calling this lookup.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|id| id.aliases().contains(&name))
    }

    /// Return the built-in manual trigger contract; no command defaults to disabled.
    /// Runtime mention/session routing is connected by the Issue 14 dispatch path.
    pub fn default_mode(self) -> ManualMode {
        match self {
            Self::Claim | Self::Unclaim | Self::Cc | Self::RequestReview | Self::Ready => {
                ManualMode::NoMention
            }
            Self::Approve | Self::Reject => ManualMode::AlwaysMention,
            _ => ManualMode::MentionOnce,
        }
    }

    /// Identify commands whose execution requires a PR diff or review endpoint.
    pub fn requires_pr(self) -> bool {
        matches!(
            self,
            Self::Review | Self::Codeql | Self::Approve | Self::Reject
        )
    }
}

impl Command {
    /// Map a parsed command and its arguments to the canonical policy identity.
    pub fn id(&self) -> CommandId {
        match self {
            Self::Claim => CommandId::Claim,
            Self::Unclaim => CommandId::Unclaim,
            Self::Cc { .. } => CommandId::Cc,
            Self::RequestReview { .. } => CommandId::RequestReview,
            Self::Ready => CommandId::Ready,
            Self::Approve { .. } => CommandId::Approve,
            Self::Reject => CommandId::Reject,
            Self::Review => CommandId::Review,
            Self::Codeql => CommandId::Codeql,
            Self::Label { .. } => CommandId::Label,
            Self::Assign { .. } => CommandId::Assign,
            Self::Author => CommandId::Author,
            Self::Blocked => CommandId::Blocked,
            Self::Ping => CommandId::Ping,
            Self::Help => CommandId::Help,
            Self::Queue => CommandId::Queue,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualMode {
    Disabled,
    NoMention,
    MentionOnce,
    AlwaysMention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonCode {
    InvalidDocument,
    InvalidComments,
    InvalidIdle,
    InvalidRule,
    DuplicateRuleId,
    InvalidPathSettings,
    Unsupported,
    Disabled,
    RequiresPr,
    MentionRequired,
    SessionRequired,
    SessionUnavailable,
    PermissionDenied,
    PermissionUnknown,
    RelayDisabled,
    SelfApproval,
    InvalidCredited,
    RepositoryUnavailable,
    BranchUnavailable,
    Forbidden,
    RateLimited,
    ApiFailure,
    Transport,
    InvalidResponse,
    Invalidated,
}

/// Safe, stable reasons for logs, help/ping and the durable delivery layer (#13).
/// No TOML source, workflow inputs, HTTP body or credentials are interpolated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code:?}: {detail}")]
pub struct Problem {
    pub code: ReasonCode,
    pub detail: &'static str,
}
impl Problem {
    /// Build a diagnostic from a stable reason code and source-free static explanation.
    pub fn new(code: ReasonCode, detail: &'static str) -> Self {
        Self { code, detail }
    }
    /// Render a short localized refusal without including repository configuration values.
    pub fn message(&self, lang: crate::lang::Lang) -> String {
        match lang {
            crate::lang::Lang::En => format!("Repository configuration blocked this action ({:?}): {}. Check `{CONFIG_PATH}` on the default branch.", self.code, self.detail),
            crate::lang::Lang::Zh => format!("仓库配置阻止了此操作（{:?}）：{}。请检查默认分支上的 `{CONFIG_PATH}`。", self.code, self.zh_detail()),
        }
    }
    /// Translate reason categories without exposing the underlying TOML or HTTP body.
    fn zh_detail(&self) -> &'static str {
        match self.code {
            ReasonCode::Disabled => "该指令及其别名已禁用",
            ReasonCode::Unsupported => "该功能尚未支持",
            ReasonCode::MentionRequired => "需要显式 @ 机器人",
            ReasonCode::SessionRequired => "需要有效的显式 @ 会话",
            ReasonCode::SessionUnavailable => "会话存储不可用，请稍后重试",
            ReasonCode::PermissionDenied => "当前用户没有执行此审批操作所需的仓库权限",
            ReasonCode::PermissionUnknown => "无法确认当前用户的仓库权限",
            ReasonCode::RelayDisabled => "部署未开启代审批",
            ReasonCode::SelfApproval => "不允许自我审批",
            ReasonCode::InvalidCredited => "代审批目标不是合法的 GitHub 用户名",
            ReasonCode::RequiresPr => "该指令仅适用于 PR",
            ReasonCode::Forbidden => "GitHub 拒绝读取配置",
            ReasonCode::RateLimited => "GitHub API 已限流，请稍后重试",
            ReasonCode::Transport | ReasonCode::ApiFailure => "配置读取失败，请稍后重试",
            ReasonCode::RepositoryUnavailable => "无法确认目标仓库可读",
            ReasonCode::BranchUnavailable => "无法确认默认分支可读",
            ReasonCode::Invalidated => "配置读取期间默认分支发生变化，请重试",
            _ => "配置格式或规则无效",
        }
    }
}

pub type Domain<T> = Result<T, Problem>;

#[derive(Debug, Clone)]
pub struct Comments {
    modes: BTreeMap<CommandId, ManualMode>,
    pub ttl_days: u16,
}
impl Default for Comments {
    /// Merge all sixteen built-in modes with the 30-day session TTL contract.
    fn default() -> Self {
        Self {
            modes: CommandId::ALL
                .into_iter()
                .map(|id| (id, id.default_mode()))
                .collect(),
            ttl_days: 30,
        }
    }
}
impl Comments {
    /// Return the merged mode for a canonical command, including an explicit disabled mode.
    pub fn mode(&self, id: CommandId) -> ManualMode {
        self.modes[&id]
    }

    /// Call before applicability/session/authorization/inbox checks. This does
    /// not authorize execution; callers must still perform repository checks.
    pub fn enabled(&self, id: CommandId) -> Domain<ManualMode> {
        match self.mode(id) {
            ManualMode::Disabled => Err(Problem::new(
                ReasonCode::Disabled,
                "command and all aliases are disabled",
            )),
            mode => Ok(mode),
        }
    }

    /// Check disabled, PR applicability, then the manual mention/session requirement.
    /// The caller supplies mention and session evidence and must still check repository
    /// permissions before execution; this pure contract does not create or renew sessions.
    pub fn gate(&self, id: CommandId, is_pr: bool, explicit: bool, session: bool) -> Domain<()> {
        let mode = self.enabled(id)?;
        if id.requires_pr() && !is_pr {
            return Err(Problem::new(
                ReasonCode::RequiresPr,
                "command requires a pull request",
            ));
        }
        match mode {
            ManualMode::AlwaysMention if !explicit => Err(Problem::new(
                ReasonCode::MentionRequired,
                "an explicit bot mention is required",
            )),
            ManualMode::MentionOnce if !explicit && !session => Err(Problem::new(
                ReasonCode::SessionRequired,
                "an explicit-mention session is required",
            )),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    PullRequestOpened,
    PullRequestSynchronize,
    IssueOpened,
}
impl Event {
    /// Recognize supported event names without guessing a default for unknown events.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "pull_request.opened" => Some(Self::PullRequestOpened),
            "pull_request.synchronize" => Some(Self::PullRequestSynchronize),
            "issues.opened" => Some(Self::IssueOpened),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EventAction {
    Review,
    Codeql,
    AddLabels(Vec<String>),
}
impl EventAction {
    /// Deployment control labels are never automatic data labels, even when
    /// the corresponding legacy feature is temporarily switched off.
    pub fn validate_deployment(&self, cfg: &crate::config::Config) -> Domain<()> {
        if let Self::AddLabels(labels) = self {
            if labels.iter().any(|label| {
                [
                    &cfg.label_merge_queue_queued,
                    &cfg.label_merge_queue_testing,
                    &cfg.codeql_label,
                ]
                .iter()
                .any(|reserved| {
                    !reserved.is_empty() && label.to_lowercase() == reserved.to_lowercase()
                })
            }) {
                return Err(Problem::new(
                    ReasonCode::InvalidRule,
                    "automatic labels contain a deployment control label",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone)]
pub struct EventRule {
    pub id: String,
    pub event: Event,
    pub action: EventAction,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PathRule {
    pub id: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub labels: Vec<String>,
    pub cc: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct Rule<T> {
    pub id: String,
    pub value: Domain<T>,
}
#[derive(Debug, Clone)]
pub struct Paths {
    pub events: Vec<Event>,
    pub max_cc_users_per_pr: u8,
    pub rules: Vec<Rule<PathRule>>,
}

impl PathRule {
    /// Deployment control labels are never safe as path-derived labels.
    pub fn validate_deployment(&self, cfg: &crate::config::Config) -> Domain<()> {
        if self.labels.iter().any(|label| {
            [
                &cfg.label_merge_queue_queued,
                &cfg.label_merge_queue_testing,
                &cfg.codeql_label,
            ]
            .iter()
            .any(|reserved| !reserved.is_empty() && label.to_lowercase() == reserved.to_lowercase())
        }) {
            return Err(Problem::new(
                ReasonCode::InvalidRule,
                "path labels contain a deployment control label",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone)]
pub struct RepositoryConfig {
    pub comments: Domain<Comments>,
    pub events: Domain<Vec<Rule<EventRule>>>,
    pub paths: Domain<Paths>,
    pub idle: Domain<Option<Rules>>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Document {
    command_triggers: BTreeMap<String, Trigger>,
    command_sessions: Sessions,
    event_triggers: Vec<RawEventRule>,
    path_triggers: RawPaths,
    idle_workflows: Option<Rules>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Trigger {
    mode: String,
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Sessions {
    ttl_days: i64,
}
impl Default for Sessions {
    /// Use the contract TTL when command_sessions is omitted or empty.
    fn default() -> Self {
        Self { ttl_days: 30 }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventRule {
    id: String,
    event: String,
    command: String,
    #[serde(default)]
    add: Vec<String>,
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawPaths {
    events: Vec<String>,
    max_cc_users_per_pr: i64,
    rules: Vec<RawPathRule>,
}
impl Default for RawPaths {
    /// Subscribe future path rules to PR opened/synchronize with a ten-user ceiling; rules remain empty.
    fn default() -> Self {
        Self {
            events: vec![
                "pull_request.opened".into(),
                "pull_request.synchronize".into(),
            ],
            max_cc_users_per_pr: 10,
            rules: vec![],
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPathRule {
    id: String,
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
}

impl RepositoryConfig {
    /// Semantic failures remain visible even when another domain is usable.
    /// In particular, future event/path rules must never be silently accepted.
    pub fn problems(&self) -> Vec<&Problem> {
        let mut problems = Vec::new();
        if let Err(e) = &self.comments {
            problems.push(e);
        }
        if let Err(e) = &self.idle {
            problems.push(e);
        }
        match &self.events {
            Err(e) => problems.push(e),
            Ok(rules) => problems.extend(rules.iter().filter_map(|r| r.value.as_ref().err())),
        }
        match &self.paths {
            Err(e) => problems.push(e),
            Ok(paths) => problems.extend(paths.rules.iter().filter_map(|r| r.value.as_ref().err())),
        }
        problems
    }

    /// Deserialize strict TOML, then validate independent semantic domains.
    /// Syntax/shape errors reject the document; domain and rule errors remain inspectable
    /// through `problems()` and never receive fallback defaults.
    pub fn parse(text: &str, repository: &str) -> Domain<Self> {
        let doc: Document = toml::from_str(text).map_err(|_| {
            Problem::new(
                ReasonCode::InvalidDocument,
                "invalid TOML syntax, field or structure",
            )
        })?;
        let comments = comments(doc.command_triggers, doc.command_sessions);
        let events = events(doc.event_triggers, &comments);
        let paths = paths(doc.path_triggers);
        let idle = idle::validate(doc.idle_workflows, repository)
            .map_err(|_| Problem::new(ReasonCode::InvalidIdle, "invalid idle workflow settings"));
        Ok(Self {
            comments,
            events,
            paths,
            idle,
        })
    }
}

/// Merge manual overrides only after checking TTL, command names and alias uniqueness.
/// A semantic failure makes the entire comment domain unavailable.
fn comments(triggers: BTreeMap<String, Trigger>, sessions: Sessions) -> Domain<Comments> {
    let bad = || {
        Problem::new(
            ReasonCode::InvalidComments,
            "unknown command, duplicate alias, invalid mode or session TTL (1..365 days)",
        )
    };
    if !(1..=365).contains(&sessions.ttl_days) {
        return Err(bad());
    }
    let mut comments = Comments {
        ttl_days: sessions.ttl_days as u16,
        ..Comments::default()
    };
    let mut seen = HashSet::new();
    for (name, trigger) in triggers {
        let id = CommandId::from_name(&name).ok_or_else(bad)?;
        if !seen.insert(id) {
            return Err(bad());
        }
        let mode = match trigger.mode.as_str() {
            "disabled" => ManualMode::Disabled,
            "no_mention" => ManualMode::NoMention,
            "mention_once" => ManualMode::MentionOnce,
            "always_mention" => ManualMode::AlwaysMention,
            _ => return Err(bad()),
        };
        comments.modes.insert(id, mode);
    }
    Ok(comments)
}

/// Reject duplicate IDs before individual rule validation to avoid ambiguous ownership.
fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Domain<()> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(Problem::new(
                ReasonCode::DuplicateRuleId,
                "duplicate rule IDs disable this domain",
            ));
        }
    }
    Ok(())
}
/// Validate IDs at domain scope and preserve each event rule's independent result.
fn events(raw: Vec<RawEventRule>, comments: &Domain<Comments>) -> Domain<Vec<Rule<EventRule>>> {
    if raw.len() > 32 {
        return Err(Problem::new(
            ReasonCode::InvalidRule,
            "at most 32 event rules are allowed",
        ));
    }
    unique_ids(raw.iter().map(|r| r.id.as_str()))?;
    Ok(raw
        .into_iter()
        .map(|r| {
            let id = r.id.clone();
            let value = event_rule(r, comments);
            Rule { id, value }
        })
        .collect())
}
/// Check the canonical command and construct only a whitelisted automatic action.
/// A disabled command cannot be enabled through an automatic subscription.
fn event_rule(r: RawEventRule, comments: &Domain<Comments>) -> Domain<EventRule> {
    let bad = || {
        Problem::new(
            ReasonCode::InvalidRule,
            "invalid event rule or action outside the automatic whitelist",
        )
    };
    if r.id.trim().is_empty() {
        return Err(bad());
    }
    let id = CommandId::from_name(&r.command).ok_or_else(bad)?;
    comments.as_ref().map_err(Clone::clone)?.enabled(id)?;
    let event = Event::parse(&r.event).ok_or_else(bad)?;
    let action = match (id, event) {
        (CommandId::Review, Event::PullRequestOpened) if r.add.is_empty() => EventAction::Review,
        (CommandId::Codeql, Event::PullRequestOpened) if r.add.is_empty() => EventAction::Codeql,
        (CommandId::Label, Event::PullRequestOpened | Event::IssueOpened)
            if !r.add.is_empty() && r.add.iter().all(|s| !s.trim().is_empty()) =>
        {
            let mut labels: Vec<_> = r.add.into_iter().map(|s| s.to_lowercase()).collect();
            labels.sort();
            labels.dedup();
            EventAction::AddLabels(labels)
        }
        _ => return Err(bad()),
    };
    Ok(EventRule {
        id: r.id,
        event,
        action,
    })
}
/// Validate shared path settings, then retain independent rule diagnostics.
/// Static path actions do not inherit the similarly named comment command modes.
fn paths(raw: RawPaths) -> Domain<Paths> {
    if raw.rules.len() > 32 {
        return Err(Problem::new(
            ReasonCode::InvalidPathSettings,
            "at most 32 path rules are allowed",
        ));
    }
    unique_ids(raw.rules.iter().map(|r| r.id.as_str()))?;
    let bad = || {
        Problem::new(
            ReasonCode::InvalidPathSettings,
            "path events must be PR opened/synchronize; CC budget must be 0..10",
        )
    };
    if !(0..=10).contains(&raw.max_cc_users_per_pr) {
        return Err(bad());
    }
    let events: Vec<_> = raw
        .events
        .iter()
        .map(|e| match Event::parse(e) {
            Some(e @ (Event::PullRequestOpened | Event::PullRequestSynchronize)) => Ok(e),
            _ => Err(bad()),
        })
        .collect::<Domain<_>>()?;
    if events.is_empty() && !raw.rules.is_empty() {
        return Err(bad());
    }
    let rules = raw
        .rules
        .into_iter()
        .map(|r| {
            let id = r.id.clone();
            let value = if r.id.trim().is_empty()
                || r.include.is_empty()
                || r.include.len().saturating_add(r.exclude.len()) > 64
                || r.include
                    .iter()
                    .chain(&r.exclude)
                    .any(|s| s.trim().is_empty())
                || (r.labels.is_empty() && r.cc.is_empty())
                || r.labels.len() > 20
                || r.labels.iter().any(|s| s.trim().is_empty())
                || r.cc.iter().any(|s| !crate::commands::is_valid_login(s))
                || r.include
                    .iter()
                    .chain(&r.exclude)
                    .any(|s| !crate::path_triggers::valid_glob(s))
            {
                Err(Problem::new(
                    ReasonCode::InvalidRule,
                    "invalid path rule, labels or explicit personal logins",
                ))
            } else if !r.cc.is_empty() {
                Err(Problem::new(
                    ReasonCode::Unsupported,
                    "path notifications are not implemented yet (#17)",
                ))
            } else {
                Ok(PathRule {
                    id: r.id,
                    include: r.include,
                    exclude: r.exclude,
                    labels: r.labels,
                    cc: r.cc,
                })
            };
            Rule { id, value }
        })
        .collect();
    Ok(Paths {
        events,
        max_cc_users_per_pr: raw.max_cc_users_per_pr as u8,
        rules,
    })
}
