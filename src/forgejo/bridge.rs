//! Two-way Issue/PR metadata and comment bridge between GitHub and Forgejo.
//!
//! Explicit ordinary-repository bindings may opt into conservative Git ref
//! transport before PR updates. Object identity lives in a durable scoped
//! mapping; every relayed body
//! also carries an HMAC-authenticated hidden marker. Mappings are authoritative:
//! editable markers never authorize a new mapping. Uncertain creates/comments
//! are reserved durably and require an operator-verified receipt before replay.
//!
//! Only `opened` creates a peer. Later edits and comments require a valid mapping
//! in SQLite. No provider's webhook-supplied repository ID is trusted as
//! a destination identity; ordinary repositories require bilateral pinned IDs,
//! while legacy mirrors use their Git origin and the App installation inventory.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use hmac::Mac;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{identity::Identities, ForgejoClient, ForgejoError, ForgejoEvent, ForgejoLabel};
use crate::config::cache::{ConfigState, RepositoryConfigCache, RepositoryKey};
use crate::config::repository::ForgejoBridgeConfig;
use crate::config::Config;
use crate::github::{Client, GhError};
use crate::trigger_state::Store;

pub mod inbox;

const MARKER_PREFIX: &str = "<!-- xero-bridge";
const COMMENT_PREFIX: &str = "<!-- xero-bridge-comment:";
const MAX_BODY: usize = 65_000;
const BINDING_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    GitHub,
    Forgejo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Issue,
    PullRequest,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pull",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Marker {
    peer: i64,
    kind: Kind,
    revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Label {
    name: String,
    color: String,
    description: String,
}

#[derive(Debug, Clone)]
struct Item {
    number: i64,
    is_pr: bool,
    action: String,
    title: String,
    body: String,
    state: String,
    labels: Vec<Label>,
    head: String,
    base: String,
    peer_repo: String,
}

#[derive(Debug, Clone)]
struct Comment {
    id: i64,
    number: i64,
    is_pr: bool,
    body: String,
    author: String,
}

#[derive(Debug, Clone)]
struct Binding {
    github_repo: String,
    installation_id: i64,
    repository_id: i64,
    forgejo_repository_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeOutcome {
    ProviderDisabled,
    BridgeDisabled,
    InvalidConfiguration,
    PolicyUnavailable,
    RetryAfter(u64),
    MissingDestination,
    Ignored(&'static str),
    Synced,
    AlreadyCurrent,
    SelfAuthored,
    Rejected(u16),
    Transport(String),
    InvalidPayload(String),
    NeedsReconciliation,
}

impl std::fmt::Display for BridgeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsReconciliation => f.write_str("needs-reconciliation"),
            Self::ProviderDisabled => f.write_str("forgejo-provider-disabled"),
            Self::BridgeDisabled => f.write_str("bridge-disabled"),
            Self::InvalidConfiguration => f.write_str("invalid-configuration"),
            Self::PolicyUnavailable => f.write_str("policy-unavailable"),
            Self::RetryAfter(seconds) => write!(f, "read-retry-after-{seconds}s"),
            Self::MissingDestination => f.write_str("missing-destination"),
            Self::Ignored(reason) => write!(f, "ignored-{reason}"),
            Self::Synced => f.write_str("synced"),
            Self::AlreadyCurrent => f.write_str("already-current"),
            Self::SelfAuthored => f.write_str("self-authored"),
            Self::Rejected(status) => write!(f, "rejected-{status}"),
            Self::Transport(message) => {
                write!(f, "transport-error: {}", crate::redact::scrub(message))
            }
            Self::InvalidPayload(message) => {
                write!(f, "invalid-payload: {}", crate::redact::scrub(message))
            }
        }
    }
}

impl BridgeOutcome {
    /// Whether this is a stable expected skip rather than a failure.
    pub fn is_skip(&self) -> bool {
        matches!(
            self,
            Self::BridgeDisabled
                | Self::ProviderDisabled
                | Self::Ignored(_)
                | Self::SelfAuthored
                | Self::AlreadyCurrent
        )
    }
}

fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn revision(item: &Item) -> String {
    // Membership is synchronized, while existing repository label colors and
    // descriptions remain local. Ignore provider ordering and normalization.
    let mut labels = item
        .labels
        .iter()
        .map(|label| label.name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    let labels = labels.join("\u{1e}");
    digest(&[
        &item.title,
        &strip_any_marker(&item.body),
        &item.state,
        &labels,
        &item.head,
        &item.base,
    ])
}

fn bridge_scope(
    client: &ForgejoClient,
    key: RepositoryKey,
    forgejo_id: i64,
    github_repo: &str,
    forgejo_repo: &str,
) -> String {
    json!([
        client.api_base.as_str(),
        key.installation_id,
        key.repository_id,
        forgejo_id,
        github_repo.to_ascii_lowercase(),
        forgejo_repo.to_ascii_lowercase()
    ])
    .to_string()
}

// Both directions share one lock, including creation, live reads and comments.
// Weak entries are pruned so inactive repositories do not accumulate forever.
fn scope_lock(scope: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(scope).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(scope.into(), Arc::downgrade(&lock));
    lock
}

fn scope_digest(scope: &str) -> String {
    digest(&[scope])
}

fn sign(secret: &str, payload: &str) -> String {
    let mut mac = <crate::forgejo::HmacSha256 as Mac>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn marker_payload(scope: &str, peer: i64, kind: Kind, revision: &str) -> String {
    format!(
        "v1:{}:{peer}:{revision}:{}",
        kind.as_str(),
        scope_digest(scope)
    )
}

fn marker(secret: &str, scope: &str, peer: i64, kind: Kind, revision: &str) -> String {
    let payload = marker_payload(scope, peer, kind, revision);
    format!("{MARKER_PREFIX}:{payload}:{} -->", sign(secret, &payload))
}

fn with_marker(
    secret: &str,
    scope: &str,
    body: &str,
    peer: i64,
    kind: Kind,
    revision: &str,
) -> String {
    let source = strip_any_marker(body);
    let marker = marker(secret, scope, peer, kind, revision);
    if source.is_empty() {
        marker
    } else {
        format!("{source}\n\n{marker}")
    }
}

/// Remove bridge-owned HTML comments while retaining human text verbatim.
fn strip_any_marker(body: &str) -> String {
    body.lines()
        .filter(|line| {
            let line = line.trim();
            !(line.starts_with(MARKER_PREFIX) && line.ends_with(" -->"))
                && !line.starts_with(COMMENT_PREFIX)
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn parse_marker(body: &str, secret: &str, scope: &str) -> Option<Marker> {
    body.lines().find_map(|line| {
        let line = line.trim();
        let inner = line
            .strip_prefix(MARKER_PREFIX)?
            .strip_suffix(" -->")?
            .strip_prefix(':')?;
        let (payload, signature) = inner.rsplit_once(':')?;
        let mut fields = payload.split(':');
        if fields.next()? != "v1" {
            return None;
        }
        let kind = match fields.next()? {
            "issue" => Kind::Issue,
            "pull" => Kind::PullRequest,
            _ => return None,
        };
        let peer = fields.next()?.parse::<i64>().ok().filter(|id| *id > 0)?;
        let revision = fields.next()?;
        let scope_tag = fields.next()?;
        if revision.len() != 64
            || scope_tag != scope_digest(scope)
            || fields.next().is_some()
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let expected = hex::decode(signature).ok()?;
        let mut mac =
            <crate::forgejo::HmacSha256 as Mac>::new_from_slice(secret.as_bytes()).ok()?;
        mac.update(payload.as_bytes());
        if mac.verify_slice(&expected).is_err() {
            return None;
        }
        Some(Marker {
            peer,
            kind,
            revision: revision.to_ascii_lowercase(),
        })
    })
}

fn comment_marker(secret: &str, scope: &str, side: &str, id: i64) -> String {
    let payload = format!("v1:{side}:{id}:{}", scope_digest(scope));
    format!("{COMMENT_PREFIX}{payload}:{} -->", sign(secret, &payload))
}

fn is_bridge_comment(body: &str, secret: &str, scope: &str) -> bool {
    body.lines().any(|line| {
        let line = line.trim();
        let Some(inner) = line
            .strip_prefix(COMMENT_PREFIX)
            .and_then(|rest| rest.strip_suffix(" -->"))
        else {
            return false;
        };
        let Some((payload, signature)) = inner.rsplit_once(':') else {
            return false;
        };
        let expected_scope = scope_digest(scope);
        let mut fields = payload.split(':');
        matches!(fields.next(), Some("v1"))
            && matches!(fields.next(), Some("github" | "forgejo"))
            && fields
                .next()
                .and_then(|id| id.parse::<i64>().ok())
                .is_some_and(|id| id > 0)
            && fields.next() == Some(expected_scope.as_str())
            && fields.next().is_none()
            && hex::decode(signature).ok().is_some_and(|expected| {
                let mut mac =
                    <crate::forgejo::HmacSha256 as Mac>::new_from_slice(secret.as_bytes())
                        .expect("HMAC key");
                mac.update(payload.as_bytes());
                mac.verify_slice(&expected).is_ok()
            })
    })
}

fn canonical_repo(value: &str) -> Option<String> {
    crate::idle_workflows::config::repository_name(value).ok()
}

fn gate<'a>(cfg: &'a Config, policy: &'a ForgejoBridgeConfig) -> Result<&'a str, BridgeOutcome> {
    if !cfg.forgejo_ready() {
        return Err(BridgeOutcome::ProviderDisabled);
    }
    if !policy.enabled {
        return Err(BridgeOutcome::BridgeDisabled);
    }
    policy
        .destination
        .as_deref()
        .filter(|destination| canonical_repo(destination).is_some())
        .ok_or(BridgeOutcome::MissingDestination)
}

fn forgejo_error(error: ForgejoError) -> BridgeOutcome {
    match error {
        ForgejoError::Api { status, message } => {
            tracing::warn!(status, detail = %crate::redact::scrub(&message), "Forgejo bridge API request failed");
            BridgeOutcome::Rejected(status)
        }
        other => BridgeOutcome::Transport(other.to_string()),
    }
}

fn github_error(error: GhError) -> BridgeOutcome {
    match error {
        GhError::Api { status, message } => {
            tracing::warn!(status, detail = %crate::redact::scrub(&message), "GitHub bridge API request failed");
            BridgeOutcome::Rejected(status)
        }
        other => BridgeOutcome::Transport(other.to_string()),
    }
}

fn octocrab_error(error: octocrab::Error) -> BridgeOutcome {
    match error {
        octocrab::Error::GitHub { source, .. } => {
            tracing::warn!(status = source.status_code.as_u16(), detail = %crate::redact::scrub(&source.message), "GitHub bridge API request failed");
            BridgeOutcome::Rejected(source.status_code.as_u16())
        }
        other => BridgeOutcome::Transport(other.to_string()),
    }
}

// Only GET/identity lookups use these classifiers. Write failures retain their
// existing blocked/unknown treatment and must never enter automatic retries.
fn retry_read(outcome: BridgeOutcome) -> BridgeOutcome {
    match outcome {
        BridgeOutcome::Rejected(408 | 429 | 500..=599) | BridgeOutcome::Transport(_) => {
            tracing::info!(result = %outcome, "temporary bridge read failure");
            BridgeOutcome::RetryAfter(60)
        }
        other => other,
    }
}

fn forgejo_read_error(error: ForgejoError) -> BridgeOutcome {
    match error {
        ForgejoError::Config(_) | ForgejoError::UnknownWriteOutcome(_) => forgejo_error(error),
        other => retry_read(forgejo_error(other)),
    }
}

fn github_read_error(error: GhError) -> BridgeOutcome {
    match error {
        GhError::Api {
            status: 403,
            ref message,
        } if message.to_ascii_lowercase().contains("rate limit") => BridgeOutcome::RetryAfter(60),
        other => retry_read(github_error(other)),
    }
}

fn octocrab_read_error(error: octocrab::Error) -> BridgeOutcome {
    if let octocrab::Error::GitHub { source, .. } = &error {
        if source.status_code.as_u16() == 403
            && source.message.to_ascii_lowercase().contains("rate limit")
        {
            return BridgeOutcome::RetryAfter(60);
        }
    }
    retry_read(octocrab_error(error))
}

/// Relay a verified GitHub `issues` or `pull_request` webhook. Policy is read
/// through the immutable-commit cache and gates all side effects.
pub async fn from_github(
    gh: &Client,
    cfg: &Config,
    store: &Store,
    repo: &str,
    key: RepositoryKey,
    event: &str,
    payload: &Value,
) -> BridgeOutcome {
    if !cfg.forgejo_ready() {
        return BridgeOutcome::ProviderDisabled;
    }
    if canonical_repo(repo).is_none() {
        return BridgeOutcome::Ignored("invalid-repository");
    }

    if github_self(gh, &payload["sender"]) {
        return BridgeOutcome::SelfAuthored;
    }
    if payload["pull_request"]["head"]["ref"]
        .as_str()
        .is_some_and(crate::rollup::internal_branch)
    {
        return BridgeOutcome::Ignored("internal-rollup");
    }
    let Some(item) = github_item(event, payload) else {
        return BridgeOutcome::Ignored("unsupported-github-event");
    };
    let state = RepositoryConfigCache::shared().load(gh, key, repo).await;
    let snapshot = match state.snapshot() {
        Ok(snapshot) => snapshot,
        Err(_) => return policy_wait(&state),
    };
    let policy = match &snapshot.config.forgejo_bridge {
        Ok(policy) => policy,
        Err(_) => return BridgeOutcome::InvalidConfiguration,
    };
    let destination = match gate(cfg, policy) {
        Ok(destination) => destination,
        Err(outcome) => return outcome,
    };
    if (item.is_pr && !policy.pull_requests) || (!item.is_pr && !policy.issues) {
        return BridgeOutcome::Ignored("kind-disabled");
    }
    match gh.repo_info(repo).await {
        Ok(value) if value["id"].as_i64() == Some(key.repository_id) => {}
        Ok(_) => return BridgeOutcome::Ignored("github-repository-identity-mismatch"),
        Err(error) => return github_read_error(error),
    }
    let client = match ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token) {
        Ok(client) => client,
        Err(error) => return forgejo_read_error(error),
    };
    let (forgejo_id, remote_policy) =
        match authorize_forgejo(&client, cfg, destination, repo, key.repository_id, policy).await {
            Ok(value) => value,
            Err(outcome) => return outcome,
        };
    if (item.is_pr && !remote_policy.pull_requests) || (!item.is_pr && !remote_policy.issues) {
        return BridgeOutcome::Ignored("peer-kind-disabled");
    }
    if item.is_pr && policy.sync_refs && remote_policy.sync_refs {
        if let Err(outcome) =
            prepare_refs(gh, &client, cfg, key, repo, destination, item.number, false).await
        {
            return outcome;
        }
    }
    let scope = bridge_scope(&client, key, forgejo_id, repo, destination);
    let Ok(identities) = Identities::new(cfg, &gh.app_slug) else {
        return BridgeOutcome::InvalidConfiguration;
    };
    match forward_item(
        &gh.crab,
        &client,
        store,
        &cfg.forgejo_webhook_secret,
        Target::Forgejo,
        destination,
        repo,
        &item,
        &scope,
        &[
            &cfg.label_merge_queue_queued,
            &cfg.label_merge_queue_testing,
        ],
        &identities,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

/// Relay a verified GitHub comment. Comments require a durable object mapping;
/// a body marker alone is not enough to choose a peer issue.
pub async fn comment_from_github(
    gh: &Client,
    cfg: &Config,
    store: &Store,
    repo: &str,
    key: RepositoryKey,
    payload: &Value,
) -> BridgeOutcome {
    if !cfg.forgejo_ready() {
        return BridgeOutcome::ProviderDisabled;
    }
    if canonical_repo(repo).is_none() {
        return BridgeOutcome::Ignored("invalid-repository");
    }

    if github_self(gh, &payload["comment"]["user"]) {
        return BridgeOutcome::SelfAuthored;
    }
    let Some(comment) = github_comment(payload) else {
        return BridgeOutcome::Ignored("invalid-github-comment");
    };
    let state = RepositoryConfigCache::shared().load(gh, key, repo).await;
    let snapshot = match state.snapshot() {
        Ok(snapshot) => snapshot,
        Err(_) => return policy_wait(&state),
    };
    let policy = match &snapshot.config.forgejo_bridge {
        Ok(policy) => policy,
        Err(_) => return BridgeOutcome::InvalidConfiguration,
    };
    let destination = match gate(cfg, policy) {
        Ok(destination) => destination,
        Err(outcome) => return outcome,
    };
    if !policy.comments {
        return BridgeOutcome::Ignored("comments-disabled");
    }
    match gh.repo_info(repo).await {
        Ok(value) if value["id"].as_i64() == Some(key.repository_id) => {}
        Ok(_) => return BridgeOutcome::Ignored("github-repository-identity-mismatch"),
        Err(error) => return github_read_error(error),
    }
    let client = match ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token) {
        Ok(client) => client,
        Err(error) => return forgejo_read_error(error),
    };
    let (forgejo_id, remote_policy) =
        match authorize_forgejo(&client, cfg, destination, repo, key.repository_id, policy).await {
            Ok(value) => value,
            Err(outcome) => return outcome,
        };
    if !remote_policy.comments {
        return BridgeOutcome::Ignored("peer-comments-disabled");
    }
    let scope = bridge_scope(&client, key, forgejo_id, repo, destination);
    if is_bridge_comment(&comment.body, &cfg.forgejo_webhook_secret, &scope) {
        return BridgeOutcome::SelfAuthored;
    }
    let peer = match store.bridge_peer(
        &scope,
        kind(comment.is_pr).as_str(),
        "github",
        comment.number,
    ) {
        Ok(Some(peer)) => peer,
        Ok(None) => return BridgeOutcome::Ignored("unmapped-comment"),
        Err(error) => return storage_error(error),
    };
    let Ok(identities) = Identities::new(cfg, &gh.app_slug) else {
        return BridgeOutcome::InvalidConfiguration;
    };
    match forward_comment_to_forgejo(
        &client,
        store,
        &cfg.forgejo_webhook_secret,
        destination,
        &scope,
        peer,
        &comment,
        &identities,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

/// Relay a verified Forgejo `issues` or `pull_request` webhook.
pub async fn from_forgejo(
    cfg: &Config,
    store: &Store,
    forgejo_repo: &str,
    event: &ForgejoEvent,
) -> BridgeOutcome {
    if !cfg.forgejo_ready() {
        return BridgeOutcome::ProviderDisabled;
    }
    if canonical_repo(forgejo_repo).is_none() {
        return BridgeOutcome::Ignored("invalid-repository");
    }

    let Some(item) = forgejo_item(event) else {
        return BridgeOutcome::Ignored("unsupported-forgejo-event");
    };
    if item.is_pr && crate::rollup::internal_branch(&item.head) {
        return BridgeOutcome::Ignored("internal-rollup");
    }
    let client = match ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token) {
        Ok(client) => client,
        Err(error) => return forgejo_read_error(error),
    };
    let sender = match event {
        ForgejoEvent::Issue { sender, .. } | ForgejoEvent::PullRequest { sender, .. } => sender,
        _ => return BridgeOutcome::Ignored("unsupported-forgejo-event"),
    };
    if sender.is_empty() {
        return BridgeOutcome::Ignored("missing-sender");
    }
    let bot = match client.current_user().await {
        Ok(bot) => bot,
        Err(error) => return forgejo_read_error(error),
    };
    let Some(login) = bot["login"].as_str().filter(|s| !s.is_empty()) else {
        return BridgeOutcome::PolicyUnavailable;
    };
    if sender.eq_ignore_ascii_case(login) {
        return BridgeOutcome::SelfAuthored;
    }
    let repository = match client.repository_config(forgejo_repo).await {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(forgejo_repo, "Forgejo bridge policy unavailable: {error}");
            return BridgeOutcome::PolicyUnavailable;
        }
    };
    let policy = match &repository.forgejo_bridge {
        Ok(policy) => policy,
        Err(_) => return BridgeOutcome::InvalidConfiguration,
    };
    let configured = match gate(cfg, policy) {
        Ok(configured) => configured,
        Err(outcome) => return outcome,
    };
    if !configured.eq_ignore_ascii_case(forgejo_repo) {
        return BridgeOutcome::Ignored("destination-mismatch");
    }
    if (item.is_pr && !policy.pull_requests) || (!item.is_pr && !policy.issues) {
        return BridgeOutcome::Ignored("kind-disabled");
    }
    let binding = match resolve_binding(&client, cfg, forgejo_repo, policy).await {
        Ok(binding) => binding,
        Err(outcome) => return outcome,
    };
    let source_repository = match event {
        ForgejoEvent::Issue { repository_id, .. }
        | ForgejoEvent::PullRequest { repository_id, .. }
        | ForgejoEvent::IssueComment { repository_id, .. } => *repository_id,
        _ => None,
    };
    if source_repository != Some(binding.forgejo_repository_id) {
        return BridgeOutcome::Ignored("forgejo-repository-identity-mismatch");
    }
    let gh = match Client::installation_resolved(cfg, binding.installation_id).await {
        Ok(client) => client,
        Err(error) => return github_read_error(error),
    };
    let metadata = match crate::admission::github_repository(
        cfg,
        &gh,
        binding.installation_id,
        &binding.github_repo,
        Some(binding.repository_id),
    )
    .await
    {
        Ok(metadata) => metadata,
        Err(error) => return github_read_error(error),
    };
    if metadata["id"].as_i64() != Some(binding.repository_id) {
        return BridgeOutcome::Ignored("github-repository-identity-mismatch");
    }
    let key = RepositoryKey {
        installation_id: binding.installation_id,
        repository_id: binding.repository_id,
    };
    let remote_policy = match github_policy(&gh, cfg, &binding.github_repo, key, forgejo_repo).await
    {
        Ok(policy) => policy,
        Err(outcome) => return outcome,
    };
    if (item.is_pr && !remote_policy.pull_requests) || (!item.is_pr && !remote_policy.issues) {
        return BridgeOutcome::Ignored("peer-kind-disabled");
    }
    if let Err(outcome) = matching_identities(
        policy,
        &remote_policy,
        &binding.github_repo,
        binding.repository_id,
        binding.forgejo_repository_id,
    ) {
        return outcome;
    }
    let scope = bridge_scope(
        &client,
        key,
        binding.forgejo_repository_id,
        &binding.github_repo,
        forgejo_repo,
    );
    if item.is_pr && policy.sync_refs && remote_policy.sync_refs {
        if let Err(outcome) = prepare_refs(
            &gh,
            &client,
            cfg,
            key,
            &binding.github_repo,
            forgejo_repo,
            item.number,
            true,
        )
        .await
        {
            return outcome;
        }
    }
    let Ok(identities) = Identities::new(cfg, &gh.app_slug) else {
        return BridgeOutcome::InvalidConfiguration;
    };
    match forward_item(
        &gh.crab,
        &client,
        store,
        &cfg.forgejo_webhook_secret,
        Target::GitHub,
        &binding.github_repo,
        forgejo_repo,
        &item,
        &scope,
        &[
            &cfg.label_merge_queue_queued,
            &cfg.label_merge_queue_testing,
        ],
        &identities,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

/// Relay one verified Forgejo comment to its mapped GitHub issue/PR.
pub async fn comment_from_forgejo(
    cfg: &Config,
    store: &Store,
    forgejo_repo: &str,
    event: &ForgejoEvent,
) -> BridgeOutcome {
    if !cfg.forgejo_ready() {
        return BridgeOutcome::ProviderDisabled;
    }
    if canonical_repo(forgejo_repo).is_none() {
        return BridgeOutcome::Ignored("invalid-repository");
    }

    let Some(comment) = forgejo_comment(event) else {
        return BridgeOutcome::Ignored("not-a-comment");
    };
    let client = match ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token) {
        Ok(client) => client,
        Err(error) => return forgejo_read_error(error),
    };
    let bot = match client.current_user().await {
        Ok(bot) => bot,
        Err(error) => return forgejo_read_error(error),
    };
    let Some(login) = bot["login"].as_str().filter(|s| !s.is_empty()) else {
        return BridgeOutcome::PolicyUnavailable;
    };
    if comment.author.eq_ignore_ascii_case(login) {
        return BridgeOutcome::SelfAuthored;
    }
    let repository = match client.repository_config(forgejo_repo).await {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(
                forgejo_repo,
                "Forgejo bridge comment policy unavailable: {error}"
            );
            return BridgeOutcome::PolicyUnavailable;
        }
    };
    let policy = match &repository.forgejo_bridge {
        Ok(policy) => policy,
        Err(_) => return BridgeOutcome::InvalidConfiguration,
    };
    let configured = match gate(cfg, policy) {
        Ok(configured) => configured,
        Err(outcome) => return outcome,
    };
    if !configured.eq_ignore_ascii_case(forgejo_repo) || !policy.comments {
        return BridgeOutcome::Ignored("comments-disabled");
    }
    let binding = match resolve_binding(&client, cfg, forgejo_repo, policy).await {
        Ok(binding) => binding,
        Err(outcome) => return outcome,
    };
    let source_repository = match event {
        ForgejoEvent::Issue { repository_id, .. }
        | ForgejoEvent::PullRequest { repository_id, .. }
        | ForgejoEvent::IssueComment { repository_id, .. } => *repository_id,
        _ => None,
    };
    if source_repository != Some(binding.forgejo_repository_id) {
        return BridgeOutcome::Ignored("forgejo-repository-identity-mismatch");
    }
    let gh = match Client::installation_resolved(cfg, binding.installation_id).await {
        Ok(client) => client,
        Err(error) => return github_read_error(error),
    };
    let metadata = match crate::admission::github_repository(
        cfg,
        &gh,
        binding.installation_id,
        &binding.github_repo,
        Some(binding.repository_id),
    )
    .await
    {
        Ok(metadata) => metadata,
        Err(error) => return github_read_error(error),
    };
    if metadata["id"].as_i64() != Some(binding.repository_id) {
        return BridgeOutcome::Ignored("github-repository-identity-mismatch");
    }
    let key = RepositoryKey {
        installation_id: binding.installation_id,
        repository_id: binding.repository_id,
    };
    let remote_policy = match github_policy(&gh, cfg, &binding.github_repo, key, forgejo_repo).await
    {
        Ok(policy) => policy,
        Err(outcome) => return outcome,
    };
    if !remote_policy.comments {
        return BridgeOutcome::Ignored("peer-kind-disabled");
    }
    if let Err(outcome) = matching_identities(
        policy,
        &remote_policy,
        &binding.github_repo,
        binding.repository_id,
        binding.forgejo_repository_id,
    ) {
        return outcome;
    }
    let scope = bridge_scope(
        &client,
        key,
        binding.forgejo_repository_id,
        &binding.github_repo,
        forgejo_repo,
    );
    if is_bridge_comment(&comment.body, &cfg.forgejo_webhook_secret, &scope) {
        return BridgeOutcome::SelfAuthored;
    }
    let kind = kind(comment.is_pr);
    let peer = match store.bridge_peer(&scope, kind.as_str(), "forgejo", comment.number) {
        Ok(Some(peer)) => peer,
        Ok(None) => return BridgeOutcome::Ignored("unmapped-comment"),
        Err(error) => return BridgeOutcome::Transport(error.to_string()),
    };
    let Ok(identities) = Identities::new(cfg, &gh.app_slug) else {
        return BridgeOutcome::InvalidConfiguration;
    };
    match forward_comment_to_github(
        &gh,
        store,
        &cfg.forgejo_webhook_secret,
        &binding.github_repo,
        &scope,
        peer,
        &comment,
        &identities,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_refs(
    gh: &Client,
    forgejo: &ForgejoClient,
    cfg: &Config,
    key: RepositoryKey,
    github_repo: &str,
    forgejo_repo: &str,
    number: i64,
    from_forgejo: bool,
) -> Result<(), BridgeOutcome> {
    let source_repo = if from_forgejo {
        forgejo_repo
    } else {
        github_repo
    };
    let route = format!("/repos/{source_repo}/pulls/{number}");
    let live = if from_forgejo {
        forgejo.get(&route).await.map_err(forgejo_read_error)?
    } else {
        gh.get(&route).await.map_err(github_read_error)?
    };
    if live["state"].as_str() == Some("closed") {
        return Ok(());
    }
    if !live["head"]["repo"]["full_name"]
        .as_str()
        .is_some_and(|r| r.eq_ignore_ascii_case(source_repo))
    {
        return Err(BridgeOutcome::Ignored("fork-pr-not-mirrored"));
    }
    let head = live["head"]["ref"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(BridgeOutcome::Ignored("missing-pr-branches"))?;
    let base = live["base"]["ref"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(BridgeOutcome::Ignored("missing-pr-branches"))?;
    super::refs::sync_pr(
        gh,
        cfg,
        key.installation_id,
        github_repo,
        forgejo_repo,
        head,
        base,
        from_forgejo,
    )
    .await
    .map_err(BridgeOutcome::Transport)
}

fn github_self(gh: &Client, user: &Value) -> bool {
    user["login"].as_str().is_some_and(|login| {
        !gh.app_slug.is_empty()
            && login.eq_ignore_ascii_case(&format!(
                "{}[bot]",
                crate::github::normalize_login(&gh.app_slug)
            ))
    })
}

async fn github_policy(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    key: RepositoryKey,
    forgejo_repo: &str,
) -> Result<ForgejoBridgeConfig, BridgeOutcome> {
    let state = RepositoryConfigCache::shared().load(gh, key, repo).await;
    let snapshot = state.snapshot().map_err(|_| policy_wait(&state))?;
    let policy = snapshot
        .config
        .forgejo_bridge
        .as_ref()
        .map_err(|_| BridgeOutcome::InvalidConfiguration)?;
    if !gate(cfg, policy)?.eq_ignore_ascii_case(forgejo_repo) {
        return Err(BridgeOutcome::Ignored("destination-mismatch"));
    }
    Ok(policy.clone())
}

fn policy_wait(state: &ConfigState) -> BridgeOutcome {
    match state {
        ConfigState::Unavailable {
            retry_after_secs, ..
        } => BridgeOutcome::RetryAfter((*retry_after_secs).max(15)),
        ConfigState::Ready(_) => BridgeOutcome::PolicyUnavailable,
    }
}

pub(crate) async fn authorize_forgejo(
    client: &ForgejoClient,
    cfg: &Config,
    repo: &str,
    github_repo: &str,
    github_id: i64,
    local_policy: &ForgejoBridgeConfig,
) -> Result<(i64, ForgejoBridgeConfig), BridgeOutcome> {
    let metadata = crate::admission::forgejo_repository(cfg, client, repo, None)
        .await
        .map_err(forgejo_read_error)?;
    let id = metadata["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or_else(|| BridgeOutcome::InvalidPayload("Forgejo repository has no id".into()))?;
    let policy = client
        .repository_config(repo)
        .await
        .map_err(|_| BridgeOutcome::PolicyUnavailable)?
        .forgejo_bridge
        .map_err(|_| BridgeOutcome::InvalidConfiguration)?;
    if !gate(cfg, &policy)?.eq_ignore_ascii_case(repo) {
        return Err(BridgeOutcome::Ignored("destination-mismatch"));
    }
    validate_source(&metadata, &policy)?;
    if source_repository(&metadata, &policy)?.as_str() != github_repo.to_ascii_lowercase() {
        return Err(BridgeOutcome::Ignored("mirror-origin-mismatch"));
    }
    matching_identities(local_policy, &policy, github_repo, github_id, id)?;
    Ok((id, policy))
}

/// Resolve an explicitly pinned pair or a legacy mirror origin to one App installation and
/// repository ID. Results are cached briefly to keep comment relays cheap.
async fn resolve_binding(
    forgejo: &ForgejoClient,
    cfg: &Config,
    forgejo_repo: &str,
    policy: &ForgejoBridgeConfig,
) -> Result<Binding, BridgeOutcome> {
    static CACHE: OnceLock<tokio::sync::Mutex<HashMap<String, (Instant, Binding)>>> =
        OnceLock::new();
    let metadata = crate::admission::forgejo_repository(cfg, forgejo, forgejo_repo, None)
        .await
        .map_err(forgejo_read_error)?;
    validate_source(&metadata, policy)?;
    let forgejo_repository_id = metadata["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or_else(|| BridgeOutcome::InvalidPayload("Forgejo repository has no id".into()))?;
    let github_repo = source_repository(&metadata, policy)?;
    let key = format!(
        "{}|{}|{}|{}|{:?}",
        forgejo.api_base,
        cfg.app_id,
        forgejo_repository_id,
        github_repo.to_ascii_lowercase(),
        policy.github_repository_id
    );
    let cache = CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    {
        let cache = cache.lock().await;
        if let Some((at, binding)) = cache.get(&key) {
            if at.elapsed() < BINDING_TTL
                && crate::admission::github_scope(
                    cfg,
                    binding.installation_id,
                    &binding.github_repo,
                    Some(binding.repository_id),
                )
            {
                return Ok(binding.clone());
            }
        }
    }

    let app = Client::app_client(cfg).map_err(github_read_error)?;
    let installations = crate::github::paginate(&app, "/app/installations?per_page=100")
        .await
        .map_err(github_read_error)?;
    let mut found = None;
    let mut lookup_error = None;
    for installation in installations {
        let Some(installation_id) = installation["id"].as_i64().filter(|id| *id > 0) else {
            continue;
        };
        if crate::admission::github_installation(cfg, installation_id).is_err() {
            continue;
        }
        let client = match Client::installation_resolved(cfg, installation_id).await {
            Ok(client) => client,
            Err(error) => {
                lookup_error = Some(github_read_error(error));
                continue;
            }
        };
        let repos =
            match crate::github::paginate(&client.crab, "/installation/repositories?per_page=100")
                .await
            {
                Ok(repos) => repos,
                Err(error) => {
                    lookup_error = Some(github_read_error(error));
                    continue;
                }
            };
        if let Some(repository) = repos.into_iter().find(|repo| {
            crate::admission::github_metadata(cfg, installation_id, repo)
                && repo["full_name"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&github_repo))
        }) {
            let Some(repository_id) = repository["id"].as_i64().filter(|id| *id > 0) else {
                continue;
            };
            if policy
                .github_repository_id
                .is_some_and(|id| id != repository_id)
            {
                return Err(BridgeOutcome::Ignored(
                    "github-repository-identity-mismatch",
                ));
            }
            found = Some(Binding {
                github_repo: repository["full_name"]
                    .as_str()
                    .unwrap_or(&github_repo)
                    .to_string(),
                installation_id,
                repository_id,
                forgejo_repository_id,
            });
            break;
        }
    }
    let binding = found.ok_or_else(|| {
        lookup_error.unwrap_or(BridgeOutcome::Ignored("github-installation-not-found"))
    })?;
    let mut cache = cache.lock().await;
    cache.retain(|_, (at, _)| at.elapsed() < BINDING_TTL);
    cache.insert(key, (Instant::now(), binding.clone()));
    Ok(binding)
}

fn validate_source(metadata: &Value, policy: &ForgejoBridgeConfig) -> Result<(), BridgeOutcome> {
    if metadata["mirror"].as_bool().is_none() {
        return Err(BridgeOutcome::InvalidPayload(
            "Forgejo repository type missing".into(),
        ));
    }
    if policy
        .forgejo_repository_id
        .is_some_and(|id| metadata["id"].as_i64() != Some(id))
    {
        return Err(BridgeOutcome::Ignored(
            "forgejo-repository-identity-mismatch",
        ));
    }
    if metadata["mirror"].as_bool() != Some(true) && policy.github_repository.is_none() {
        return Err(BridgeOutcome::Ignored(
            "ordinary-repository-needs-explicit-binding",
        ));
    }
    Ok(())
}

fn source_repository(
    metadata: &Value,
    policy: &ForgejoBridgeConfig,
) -> Result<String, BridgeOutcome> {
    if let Some(repo) = &policy.github_repository {
        return Ok(repo.to_ascii_lowercase());
    }
    metadata["original_url"]
        .as_str()
        .and_then(parse_github_origin)
        .ok_or(BridgeOutcome::Ignored("mirror-origin-not-github"))
}

fn matching_identities(
    a: &ForgejoBridgeConfig,
    b: &ForgejoBridgeConfig,
    github_repo: &str,
    github_id: i64,
    forgejo_id: i64,
) -> Result<(), BridgeOutcome> {
    if a.github_repository.is_none() && b.github_repository.is_none() {
        return Ok(());
    }
    for policy in [a, b] {
        if !policy
            .github_repository
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case(github_repo))
            || policy.github_repository_id != Some(github_id)
            || policy.forgejo_repository_id != Some(forgejo_id)
        {
            return Err(BridgeOutcome::Ignored("explicit-binding-mismatch"));
        }
    }
    Ok(())
}

fn parse_github_origin(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.host_str(), Some("github.com" | "www.github.com"))
    {
        return None;
    }
    let pieces = url
        .path_segments()?
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<_>>();
    if pieces.len() != 2 {
        return None;
    }
    let repo = pieces[1].strip_suffix(".git").unwrap_or(pieces[1]);
    canonical_repo(&format!("{}/{}", pieces[0], repo))
}

pub(super) fn supported_action(action: &str) -> bool {
    matches!(
        action,
        "opened"
            | "edited"
            | "closed"
            | "reopened"
            | "labeled"
            | "unlabeled"
            | "label_updated"
            | "label_cleared"
            | "synchronize"
            | "synchronized"
    )
}

fn github_item(event: &str, payload: &Value) -> Option<Item> {
    let (source, is_pr) = match event {
        "issues" => (&payload["issue"], false),
        "pull_request" => (&payload["pull_request"], true),
        _ => return None,
    };
    let number = source["number"].as_i64().filter(|number| *number > 0)?;
    let action = payload["action"].as_str().unwrap_or("");
    // Ref updates may be transported by the opt-in ordinary-repository mode.
    // Review requests do not map to
    // Forgejo without an account directory and would create noisy edits.
    if !supported_action(action) || (!is_pr && matches!(action, "synchronize" | "synchronized")) {
        return None;
    }
    let (head, base, peer_repo) = if is_pr {
        (
            source["head"]["ref"].as_str().unwrap_or("").to_string(),
            source["base"]["ref"].as_str().unwrap_or("").to_string(),
            source["head"]["repo"]["full_name"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        )
    } else {
        (String::new(), String::new(), String::new())
    };
    Some(Item {
        number,
        is_pr,
        action: action.to_string(),
        title: source["title"].as_str().unwrap_or("").to_string(),
        body: source["body"].as_str().unwrap_or("").to_string(),
        state: source["state"].as_str().unwrap_or("").to_string(),
        labels: github_labels(source),
        head,
        base,
        peer_repo,
    })
}

fn github_labels(source: &Value) -> Vec<Label> {
    source["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|label| {
            Some(Label {
                name: label["name"].as_str()?.to_string(),
                color: label["color"]
                    .as_str()
                    .unwrap_or("ededed")
                    .trim_start_matches('#')
                    .to_string(),
                description: label["description"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
}

fn forgejo_item(event: &ForgejoEvent) -> Option<Item> {
    let item = match event {
        ForgejoEvent::Issue {
            number,
            action,
            title,
            body,
            state,
            labels,
            ..
        } => Some(Item {
            number: *number,
            is_pr: false,
            action: action.clone(),
            title: title.clone(),
            body: body.clone(),
            state: state.clone(),
            labels: forgejo_labels(labels),
            head: String::new(),
            base: String::new(),
            peer_repo: String::new(),
        }),
        ForgejoEvent::PullRequest {
            number,
            action,
            title,
            body,
            state,
            labels,
            head,
            base,
            head_repo,
            base_repo,
            ..
        } => {
            if !head_repo.is_empty()
                && !base_repo.is_empty()
                && !head_repo.eq_ignore_ascii_case(base_repo)
            {
                return None;
            }
            Some(Item {
                number: *number,
                is_pr: true,
                action: action.clone(),
                title: title.clone(),
                body: body.clone(),
                state: state.clone(),
                labels: forgejo_labels(labels),
                head: head.clone(),
                base: base.clone(),
                peer_repo: head_repo.clone(),
            })
        }
        _ => None,
    }?;
    (item.number > 0 && supported_action(&item.action)).then_some(item)
}

fn forgejo_labels(labels: &[ForgejoLabel]) -> Vec<Label> {
    labels
        .iter()
        .map(|label| Label {
            name: label.name.clone(),
            color: label.color.trim_start_matches('#').to_string(),
            description: label.description.clone(),
        })
        .collect()
}

fn github_comment(payload: &Value) -> Option<Comment> {
    if payload["action"].as_str() != Some("created") {
        return None;
    }
    let id = payload["comment"]["id"].as_i64().filter(|id| *id > 0)?;
    let number = payload["issue"]["number"]
        .as_i64()
        .filter(|number| *number > 0)?;
    let body = payload["comment"]["body"].as_str()?.to_string();
    let author = payload["comment"]["user"]["login"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    Some(Comment {
        id,
        number,
        is_pr: payload["issue"]
            .get("pull_request")
            .is_some_and(|value| !value.is_null()),
        body,
        author,
    })
}

fn forgejo_comment(event: &ForgejoEvent) -> Option<Comment> {
    match event {
        ForgejoEvent::IssueComment {
            number,
            comment_id,
            is_pr,
            body,
            commenter,
            ..
        } => Some(Comment {
            id: (*comment_id).filter(|id| *id > 0)?,
            number: *number,
            is_pr: *is_pr,
            body: body.clone(),
            author: commenter.clone(),
        }),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_item(
    github: &octocrab::Octocrab,
    forgejo: &ForgejoClient,
    store: &Store,
    secret: &str,
    target: Target,
    destination: &str,
    source_repo: &str,
    event_item: &Item,
    scope: &str,
    reserved_labels: &[&str],
    identities: &Identities,
) -> Result<BridgeOutcome, BridgeOutcome> {
    let lock = scope_lock(scope);
    let _guard = lock.lock().await;
    // An old webhook must never restore an old title, label set or open state.
    let route = object_route(source_repo, event_item.number, event_item.is_pr);
    let live: Value = match target {
        Target::Forgejo => github
            .get(&route, None::<&()>)
            .await
            .map_err(octocrab_read_error)?,
        Target::GitHub => forgejo.get(&route).await.map_err(forgejo_read_error)?,
    };
    if !event_item.is_pr && live.get("pull_request").is_some_and(|pr| !pr.is_null()) {
        return Ok(BridgeOutcome::Ignored("source-kind-mismatch"));
    }
    let event = if event_item.is_pr {
        "pull_request"
    } else {
        "issues"
    };
    let field = if event_item.is_pr {
        "pull_request"
    } else {
        "issue"
    };
    let mut item = github_item(event, &json!({"action":event_item.action, field:live}))
        .filter(|item| item.number == event_item.number)
        .ok_or_else(|| BridgeOutcome::InvalidPayload("source object identity missing".into()))?;
    if item.title.trim().is_empty() || !matches!(item.state.as_str(), "open" | "closed") {
        return Ok(BridgeOutcome::Ignored("invalid-object"));
    }
    if item.body.len() > MAX_BODY || item.title.len() > 512 {
        return Ok(BridgeOutcome::Ignored("object-too-large"));
    }
    item.labels.retain(|label| {
        !reserved_labels
            .iter()
            .any(|reserved| label.name.eq_ignore_ascii_case(reserved))
    });
    let kind = kind(item.is_pr);
    let source_side = peer_side(target);
    let target_side = side(target);
    let current_revision = revision(&item);
    let mapped = store
        .bridge_peer(scope, kind.as_str(), source_side, item.number)
        .map_err(storage_error)?;
    let Some(peer) = mapped else {
        // A signed marker is only echo evidence. A user can copy a valid marker
        // to another issue, so it must never create a mapping or select a target.
        if parse_marker(&item.body, secret, scope).is_some() {
            return Ok(BridgeOutcome::Ignored("unmapped-marked-object"));
        }
        return if item.action == "opened" {
            create_peer(
                github,
                forgejo,
                store,
                secret,
                target,
                destination,
                source_repo,
                &item,
                kind,
                scope,
                reserved_labels,
                identities,
            )
            .await
        } else {
            Ok(BridgeOutcome::Ignored("unmapped-edit"))
        };
    };
    if store
        .bridge_peer(scope, kind.as_str(), target_side, peer)
        .map_err(storage_error)?
        != Some(item.number)
    {
        return Ok(BridgeOutcome::Ignored("mapping-mismatch"));
    }
    if store
        .bridge_revision(scope, kind.as_str(), source_side, item.number)
        .map_err(storage_error)?
        .as_deref()
        == Some(&current_revision)
    {
        return Ok(BridgeOutcome::AlreadyCurrent);
    }
    update_peer(
        github,
        forgejo,
        store,
        secret,
        target,
        destination,
        peer,
        &item,
        kind,
        &current_revision,
        scope,
        reserved_labels,
        identities,
    )
    .await
}

fn storage_error(error: impl std::fmt::Display) -> BridgeOutcome {
    BridgeOutcome::Transport(crate::redact::scrub(&error.to_string()))
}

#[allow(clippy::too_many_arguments)]
async fn create_peer(
    github: &octocrab::Octocrab,
    forgejo: &ForgejoClient,
    store: &Store,
    secret: &str,
    target: Target,
    destination: &str,
    source_repo: &str,
    item: &Item,
    kind: Kind,
    scope: &str,
    reserved_labels: &[&str],
    identities: &Identities,
) -> Result<BridgeOutcome, BridgeOutcome> {
    if item.is_pr && !item.peer_repo.eq_ignore_ascii_case(source_repo) {
        return Ok(BridgeOutcome::Ignored("fork-pr-not-mirrored"));
    }
    if item.is_pr && (item.head.is_empty() || item.base.is_empty()) {
        return Ok(BridgeOutcome::Ignored("missing-pr-branches"));
    }
    if item.body.len() > MAX_BODY || item.title.len() > 512 {
        return Ok(BridgeOutcome::Ignored("object-too-large"));
    }
    let current_revision = revision(item);
    let body = with_marker(
        secret,
        scope,
        &identities.markdown(&item.body, target),
        item.number,
        kind,
        &current_revision,
    );
    if body.len() > MAX_BODY {
        return Ok(BridgeOutcome::Ignored("rendered-object-too-large"));
    }
    if !store
        .bridge_create_begin(scope, kind.as_str(), peer_side(target), item.number)
        .map_err(storage_error)?
    {
        return Err(BridgeOutcome::NeedsReconciliation);
    }
    let created = create_remote(github, forgejo, target, destination, item, &body).await?;
    let peer = number_of(&created).ok_or_else(|| {
        BridgeOutcome::InvalidPayload(
            "create response has no number; reconcile before retrying".into(),
        )
    })?;
    // Persist the identity before any follow-up write. Empty revision leaves a
    // partial metadata update resumable without creating another remote object.
    store
        .bridge_link(
            scope,
            kind.as_str(),
            peer_side(target),
            item.number,
            side(target),
            peer,
            "",
            crate::github::chrono_now_secs(),
        )
        .map_err(storage_error)?;
    update_peer(
        github,
        forgejo,
        store,
        secret,
        target,
        destination,
        peer,
        item,
        kind,
        &current_revision,
        scope,
        reserved_labels,
        identities,
    )
    .await?;
    Ok(BridgeOutcome::Synced)
}

#[allow(clippy::too_many_arguments)]
async fn update_peer(
    github: &octocrab::Octocrab,
    forgejo: &ForgejoClient,
    store: &Store,
    secret: &str,
    target: Target,
    destination: &str,
    peer: i64,
    item: &Item,
    kind: Kind,
    revision: &str,
    scope: &str,
    reserved_labels: &[&str],
    identities: &Identities,
) -> Result<BridgeOutcome, BridgeOutcome> {
    let body = with_marker(
        secret,
        scope,
        &identities.markdown(&item.body, target),
        item.number,
        kind,
        revision,
    );
    if body.len() > MAX_BODY {
        return Ok(BridgeOutcome::Ignored("rendered-object-too-large"));
    }
    let mut labeled = item.clone();
    if !reserved_labels.is_empty() {
        let route = format!("/repos/{destination}/issues/{peer}/labels");
        let labels = match target {
            Target::Forgejo => forgejo
                .get_all(&route, 50)
                .await
                .map_err(forgejo_read_error)?,
            Target::GitHub => crate::github::paginate(github, &format!("{route}?per_page=100"))
                .await
                .map_err(github_read_error)?,
        };
        labeled
            .labels
            .extend(
                github_labels(&json!({"labels":labels}))
                    .into_iter()
                    .filter(|label| {
                        reserved_labels
                            .iter()
                            .any(|reserved| label.name.eq_ignore_ascii_case(reserved))
                    }),
            );
    }
    let mut payload = json!({"title": item.title, "body": body, "state": item.state});
    if item.is_pr && target == Target::Forgejo && item.state == "open" {
        payload["base"] = json!(item.base);
    }
    match target {
        Target::Forgejo => {
            forgejo
                .patch(
                    &object_route(destination, peer, item.is_pr),
                    payload.clone(),
                )
                .await
                .map_err(forgejo_error)?;
            apply_forgejo_labels(forgejo, destination, peer, &labeled).await?;
        }
        Target::GitHub => {
            github_patch(
                github,
                &object_route(destination, peer, item.is_pr),
                payload,
            )
            .await?;
            // GitHub rejects any base parameter while a PR is closed, even
            // when unchanged. Reopen first; only then synchronize its base.
            if item.is_pr && item.state == "open" {
                github_patch(
                    github,
                    &object_route(destination, peer, true),
                    json!({"base":item.base}),
                )
                .await?;
            }
            apply_github_labels(github, destination, peer, &labeled).await?;
        }
    }
    store
        .bridge_link(
            scope,
            kind.as_str(),
            peer_side(target),
            item.number,
            side(target),
            peer,
            revision,
            crate::github::chrono_now_secs(),
        )
        .map_err(storage_error)?;
    Ok(BridgeOutcome::Synced)
}

async fn create_remote(
    github: &octocrab::Octocrab,
    forgejo: &ForgejoClient,
    target: Target,
    destination: &str,
    item: &Item,
    body: &str,
) -> Result<Value, BridgeOutcome> {
    let route = if item.is_pr {
        format!("/repos/{destination}/pulls")
    } else {
        format!("/repos/{destination}/issues")
    };
    let payload = if item.is_pr {
        json!({"title": item.title, "body": body, "head": item.head, "base": item.base})
    } else {
        json!({"title": item.title, "body": body})
    };
    match target {
        Target::Forgejo => forgejo.post(&route, payload).await.map_err(forgejo_error),
        Target::GitHub => github
            .post(&route, Some(&payload))
            .await
            .map_err(octocrab_error),
    }
}

async fn github_patch(
    github: &octocrab::Octocrab,
    route: &str,
    payload: Value,
) -> Result<(), BridgeOutcome> {
    github
        .patch::<Value, _, _>(route, Some(&payload))
        .await
        .map(|_| ())
        .map_err(octocrab_error)
}

async fn apply_forgejo_labels(
    client: &ForgejoClient,
    repo: &str,
    number: i64,
    item: &Item,
) -> Result<(), BridgeOutcome> {
    let labels = ensure_forgejo_labels(client, repo, &item.labels).await?;
    client
        .put(
            &format!("/repos/{repo}/issues/{number}/labels"),
            json!({"labels": labels}),
        )
        .await
        .map_err(forgejo_error)?;
    Ok(())
}

async fn ensure_forgejo_labels(
    client: &ForgejoClient,
    repo: &str,
    labels: &[Label],
) -> Result<Vec<i64>, BridgeOutcome> {
    if labels.is_empty() {
        return Ok(Vec::new());
    }
    let mut existing = client
        .get_all(&format!("/repos/{repo}/labels"), 50)
        .await
        .map_err(forgejo_read_error)?;
    let mut ids = Vec::new();
    for label in labels {
        let found = existing.iter().find(|current| {
            current["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(&label.name))
        });
        let value = match found {
            Some(value) => value.clone(),
            None => {
                let created = client.post(&format!("/repos/{repo}/labels"), json!({"name":label.name, "color":format!("#{}",safe_color(&label.color)), "description":label.description})).await.map_err(forgejo_error)?;
                existing.push(created.clone());
                created
            }
        };
        ids.push(
            value["id"]
                .as_i64()
                .filter(|id| *id > 0)
                .ok_or_else(|| BridgeOutcome::InvalidPayload("label has no id".into()))?,
        );
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

async fn apply_github_labels(
    github: &octocrab::Octocrab,
    repo: &str,
    number: i64,
    item: &Item,
) -> Result<(), BridgeOutcome> {
    ensure_github_labels(github, repo, &item.labels).await?;
    github
        .put::<Value, _, _>(
            &format!("/repos/{repo}/issues/{number}/labels"),
            Some(
                &json!({"labels": item.labels.iter().map(|label| &label.name).collect::<Vec<_>>()}),
            ),
        )
        .await
        .map_err(octocrab_error)?;
    Ok(())
}

async fn ensure_github_labels(
    github: &octocrab::Octocrab,
    repo: &str,
    labels: &[Label],
) -> Result<(), BridgeOutcome> {
    if labels.is_empty() {
        return Ok(());
    }
    let existing = crate::github::paginate(github, &format!("/repos/{repo}/labels?per_page=100"))
        .await
        .map_err(github_read_error)?;
    for label in labels {
        if existing.iter().any(|current| {
            current["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(&label.name))
        }) {
            continue;
        }
        github
            .post::<Value, Value>(
                &format!("/repos/{repo}/labels"),
                Some(&json!({
                    "name": label.name,
                    "color": safe_color(&label.color),
                    "description": label.description,
                })),
            )
            .await
            .map_err(octocrab_error)?;
    }
    Ok(())
}

fn safe_color(color: &str) -> String {
    let color = color.trim_start_matches('#');
    if color.len() == 6 && color.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        color.to_ascii_lowercase()
    } else {
        "ededed".into()
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_comment_to_forgejo(
    client: &ForgejoClient,
    store: &Store,
    secret: &str,
    destination: &str,
    scope: &str,
    peer: i64,
    comment: &Comment,
    identities: &Identities,
) -> Result<BridgeOutcome, BridgeOutcome> {
    let lock = scope_lock(scope);
    let _guard = lock.lock().await;
    if peer <= 0 || comment.body.trim().is_empty() || comment.body.len() > MAX_BODY {
        return Ok(BridgeOutcome::Ignored("invalid-comment"));
    }
    let body = format!(
        "{}:\n\n{}\n\n{}",
        identities.author(&comment.author, Target::Forgejo),
        identities.markdown(&crate::redact::scrub(&comment.body), Target::Forgejo),
        comment_marker(secret, scope, "github", comment.id)
    );
    if body.len() > MAX_BODY {
        return Ok(BridgeOutcome::Ignored("rendered-comment-too-large"));
    }
    let inserted = store
        .bridge_comment_begin(
            scope,
            "github",
            comment.id,
            peer,
            crate::github::chrono_now_secs(),
        )
        .map_err(|error| BridgeOutcome::Transport(error.to_string()))?;
    if !inserted {
        return if store
            .bridge_comment_status(scope, "github", comment.id)
            .map_err(storage_error)?
            .flatten()
            .is_some()
        {
            Ok(BridgeOutcome::AlreadyCurrent)
        } else {
            Err(BridgeOutcome::NeedsReconciliation)
        };
    }
    let created = match client
        .post(
            &format!("/repos/{destination}/issues/{peer}/comments"),
            json!({"body": body}),
        )
        .await
    {
        Ok(created) => created,
        Err(error) => return Err(forgejo_error(error)),
    };
    let id = created["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or(BridgeOutcome::NeedsReconciliation)?;
    {
        store
            .bridge_comment_finish(
                scope,
                "github",
                comment.id,
                id,
                crate::github::chrono_now_secs(),
            )
            .map_err(|error| BridgeOutcome::Transport(error.to_string()))?;
    }
    Ok(BridgeOutcome::Synced)
}

#[allow(clippy::too_many_arguments)]
async fn forward_comment_to_github(
    gh: &Client,
    store: &Store,
    secret: &str,
    destination: &str,
    scope: &str,
    peer: i64,
    comment: &Comment,
    identities: &Identities,
) -> Result<BridgeOutcome, BridgeOutcome> {
    let lock = scope_lock(scope);
    let _guard = lock.lock().await;
    if peer <= 0 || comment.body.trim().is_empty() || comment.body.len() > MAX_BODY {
        return Ok(BridgeOutcome::Ignored("invalid-comment"));
    }
    let body = format!(
        "{}:\n\n{}\n\n{}",
        identities.author(&comment.author, Target::GitHub),
        identities.markdown(&crate::redact::scrub(&comment.body), Target::GitHub),
        comment_marker(secret, scope, "forgejo", comment.id)
    );
    if body.len() > MAX_BODY {
        return Ok(BridgeOutcome::Ignored("rendered-comment-too-large"));
    }
    let inserted = store
        .bridge_comment_begin(
            scope,
            "forgejo",
            comment.id,
            peer,
            crate::github::chrono_now_secs(),
        )
        .map_err(|error| BridgeOutcome::Transport(error.to_string()))?;
    if !inserted {
        return if store
            .bridge_comment_status(scope, "forgejo", comment.id)
            .map_err(storage_error)?
            .flatten()
            .is_some()
        {
            Ok(BridgeOutcome::AlreadyCurrent)
        } else {
            Err(BridgeOutcome::NeedsReconciliation)
        };
    }
    let created = match gh
        .post(
            &format!("/repos/{destination}/issues/{peer}/comments"),
            Some(json!({"body": body})),
        )
        .await
    {
        Ok(created) => created,
        Err(error) => return Err(github_error(error)),
    };
    let id = created["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or(BridgeOutcome::NeedsReconciliation)?;
    {
        store
            .bridge_comment_finish(
                scope,
                "forgejo",
                comment.id,
                id,
                crate::github::chrono_now_secs(),
            )
            .map_err(|error| BridgeOutcome::Transport(error.to_string()))?;
    }
    Ok(BridgeOutcome::Synced)
}

fn side(target: Target) -> &'static str {
    match target {
        Target::GitHub => "github",
        Target::Forgejo => "forgejo",
    }
}

fn peer_side(target: Target) -> &'static str {
    match target {
        Target::GitHub => "forgejo",
        Target::Forgejo => "github",
    }
}

fn kind(is_pr: bool) -> Kind {
    if is_pr {
        Kind::PullRequest
    } else {
        Kind::Issue
    }
}

fn number_of(value: &Value) -> Option<i64> {
    value["number"]
        .as_i64()
        .or_else(|| value["index"].as_i64())
        .filter(|number| *number > 0)
}

fn object_route(repo: &str, number: i64, is_pr: bool) -> String {
    if is_pr {
        format!("/repos/{repo}/pulls/{number}")
    } else {
        format!("/repos/{repo}/issues/{number}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Item {
        Item {
            number: 4,
            is_pr: false,
            action: "edited".into(),
            title: "Title".into(),
            body: "Body".into(),
            state: "open".into(),
            labels: vec![Label {
                name: "bug".into(),
                color: "ff0000".into(),
                description: String::new(),
            }],
            head: String::new(),
            base: String::new(),
            peer_repo: String::new(),
        }
    }

    #[test]
    fn signed_markers_round_trip_and_reject_forgery() {
        let scope = "test-instance|xero-team/xero-bot".to_string();
        let revision = "a".repeat(64);
        let value = marker("secret-key", &scope, 7, Kind::PullRequest, &revision);
        assert_eq!(
            parse_marker(&format!("hello\n{value}"), "secret-key", &scope)
                .map(|marker| marker.peer),
            Some(7)
        );
        assert!(parse_marker(&value, "wrong-key", &scope).is_none());
        assert!(parse_marker("<!-- xero-bridge:pull:7:short -->", "secret-key", &scope).is_none());
        assert!(parse_marker(
            &marker("secret-key", &scope, 0, Kind::Issue, &revision),
            "secret-key",
            &scope
        )
        .is_none());
        assert_eq!(strip_any_marker(&format!("hello\n{value}")), "hello");
    }

    #[test]
    fn revision_ignores_marker_text_and_changes_with_metadata() {
        let scope = "test-instance|xero-team/xero-bot".to_string();
        let base = item();
        let first = revision(&base);
        assert_eq!(first, revision(&base));
        let marker = marker("secret-key", &scope, 9, Kind::Issue, &first);
        let mut echoed = base.clone();
        echoed.body = format!("Body\n\n{marker}");
        assert_eq!(first, revision(&echoed));
        echoed.labels[0].name = "feature".into();
        assert_ne!(first, revision(&echoed));
    }

    #[test]
    fn github_origin_accepts_https_git_urls_only() {
        assert_eq!(
            parse_github_origin("https://github.com/Xero-Team/xero-bot.git").as_deref(),
            Some("xero-team/xero-bot")
        );
        assert!(parse_github_origin("https://gitlab.com/a/b.git").is_none());
        assert!(parse_github_origin("https://github.com/a/b/c").is_none());
    }

    #[test]
    fn comment_markers_are_scoped_and_forgery_is_rejected() {
        let scope = "test-instance|xero-team/xero-bot".to_string();
        let marker = comment_marker("secret-key", &scope, "github", 7);
        assert!(is_bridge_comment(
            &format!("text\n{marker}"),
            "secret-key",
            &scope
        ));
        assert!(!is_bridge_comment(&marker, "wrong-key", &scope));
        assert!(!is_bridge_comment(
            &marker,
            "secret-key",
            "other-instance|xero-team/xero-bot"
        ));
        assert!(!is_bridge_comment(
            "<!-- xero-bridge:pull:7:abc -->",
            "secret-key",
            &scope
        ));
        assert_eq!(strip_any_marker(&format!("human\n{marker}")), "human");
    }
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod relay_tests;

/// Reuse the bridge's bilateral, pinned repository authorization for queue ingress.
pub(crate) async fn rollup_binding(
    forgejo: &ForgejoClient,
    cfg: &Config,
    store: &Store,
    repo: &str,
    number: i64,
) -> Result<(Client, String, i64, i64, Option<i64>), String> {
    crate::admission::forgejo_repository(cfg, forgejo, repo, None)
        .await
        .map_err(|e| e.to_string())?;
    let local = forgejo
        .repository_config(repo)
        .await?
        .forgejo_bridge
        .map_err(|e| e.to_string())?;
    gate(cfg, &local).map_err(|e| e.to_string())?;
    let binding = resolve_binding(forgejo, cfg, repo, &local)
        .await
        .map_err(|e| e.to_string())?;
    let gh = Client::installation_resolved(cfg, binding.installation_id)
        .await
        .map_err(|e| e.to_string())?;
    crate::admission::github_repository(
        cfg,
        &gh,
        binding.installation_id,
        &binding.github_repo,
        Some(binding.repository_id),
    )
    .await
    .map_err(|e| e.to_string())?;

    let key = RepositoryKey {
        installation_id: binding.installation_id,
        repository_id: binding.repository_id,
    };
    let other = github_policy(&gh, cfg, &binding.github_repo, key, repo)
        .await
        .map_err(|e| e.to_string())?;
    matching_identities(
        &local,
        &other,
        &binding.github_repo,
        binding.repository_id,
        binding.forgejo_repository_id,
    )
    .map_err(|e| e.to_string())?;
    let scope = bridge_scope(
        forgejo,
        key,
        binding.forgejo_repository_id,
        &binding.github_repo,
        repo,
    );
    let peer = store
        .bridge_peer(&scope, "pull", "forgejo", number)
        .map_err(|e| e.to_string())?;
    Ok((
        gh,
        binding.github_repo,
        binding.repository_id,
        binding.installation_id,
        peer,
    ))
}

pub(crate) async fn rollup_peer(
    gh: &Client,
    cfg: &Config,
    store: &Store,
    repo: &str,
    key: RepositoryKey,
    number: i64,
) -> Result<Option<(ForgejoClient, String, i64)>, String> {
    let state = RepositoryConfigCache::shared().load(gh, key, repo).await;
    let snapshot = state.snapshot().map_err(|e| e.to_string())?;
    let policy = snapshot
        .config
        .forgejo_bridge
        .as_ref()
        .map_err(|e| e.to_string())?;
    if !policy.enabled {
        return Ok(None);
    }
    let destination = gate(cfg, policy).map_err(|e| e.to_string())?;
    let forgejo =
        ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token).map_err(|e| e.to_string())?;
    let (id, _) = authorize_forgejo(&forgejo, cfg, destination, repo, key.repository_id, policy)
        .await
        .map_err(|e| e.to_string())?;
    let scope = bridge_scope(&forgejo, key, id, repo, destination);
    let peer = store
        .bridge_peer(&scope, "pull", "github", number)
        .map_err(|e| e.to_string())?
        .ok_or("waiting for the paired Forgejo PR")?;
    Ok(Some((forgejo, destination.to_string(), peer)))
}
