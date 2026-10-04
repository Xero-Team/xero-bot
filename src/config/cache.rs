//! Single-instance, installation/repository-scoped configuration snapshots.
//! A failed revalidation never makes a stale snapshot executable.
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::Instant;

use base64::Engine;
use serde_json::Value;

use super::repository::{Problem, ReasonCode, RepositoryConfig, CONFIG_PATH};
use crate::github::repository_config::FetchError;
use crate::github::{enc_seg, Client};

pub const TTL_SECS: u64 = 60;
pub trait Clock: Send + Sync {
    /// Return monotonically increasing seconds for snapshot age and retry deadlines.
    fn now(&self) -> u64;
}
struct MonotonicClock(Instant);
impl Clock for MonotonicClock {
    /// Measure elapsed process time so wall-clock adjustments cannot revive old policy.
    fn now(&self) -> u64 {
        self.0.elapsed().as_secs()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RepositoryKey {
    pub installation_id: i64,
    pub repository_id: i64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub key: RepositoryKey,
    pub default_branch: String,
    pub commit_sha: String,
    /// None means confirmed absent, not a read failure.
    pub blob_sha: Option<String>,
    pub config: RepositoryConfig,
    pub verified_at: u64,
    etag: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ConfigState {
    Ready(Arc<Snapshot>),
    Unavailable {
        problem: Problem,
        retry_after_secs: u64,
        stale_reference: Option<Arc<Snapshot>>,
    },
}
impl ConfigState {
    /// Expose executable policy only for Ready; stale references never pass this accessor.
    pub fn snapshot(&self) -> Result<&Snapshot, &Problem> {
        match self {
            Self::Ready(s) => Ok(s),
            Self::Unavailable { problem, .. } => Err(problem),
        }
    }
    /// This path cannot run a command, inspect a session, or create inbox work.
    pub fn diagnostic(&self, lang: crate::lang::Lang) -> Option<String> {
        match self {
            Self::Ready(_) => None,
            Self::Unavailable {
                problem,
                stale_reference,
                ..
            } => {
                let mut message = problem.message(lang);
                if stale_reference.is_some() {
                    message.push_str(match lang {
                        crate::lang::Lang::En => {
                            " Previous configuration is an expired reference only; it cannot authorize actions."
                        }
                        crate::lang::Lang::Zh => " 旧配置仅作过期参考，不能据此执行动作。",
                    });
                }
                Some(message)
            }
        }
    }
}

#[derive(Default)]
struct Cached {
    snapshot: Option<Arc<Snapshot>>,
    generation: u64,
    failure: Option<(FetchError, u64)>,
}
#[derive(Default)]
struct Entry {
    generation: AtomicU64,
    branch: Mutex<Option<String>>,
    refresh: tokio::sync::Mutex<Cached>,
}

pub struct RepositoryConfigCache {
    entries: Mutex<HashMap<RepositoryKey, Arc<Entry>>>,
    clock: Arc<dyn Clock>,
    diagnostics: Mutex<HashMap<(RepositoryKey, i64, ReasonCode), u64>>,
}
impl Default for RepositoryConfigCache {
    /// Create an isolated cache using a monotonic clock and no persisted snapshots.
    fn default() -> Self {
        Self::with_clock(Arc::new(MonotonicClock(Instant::now())))
    }
}
impl RepositoryConfigCache {
    /// Create an isolated cache with an injectable monotonic clock for deterministic tests.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            clock,
            diagnostics: Mutex::new(HashMap::new()),
        }
    }
    /// Return the process-wide cache shared by comments, CodeQL, webhooks and idle scheduling.
    pub fn shared() -> Arc<Self> {
        static CACHE: OnceLock<Arc<RepositoryConfigCache>> = OnceLock::new();
        Arc::clone(CACHE.get_or_init(|| Arc::new(Self::default())))
    }
    /// Per-process rate limit for new source comments. Durable write receipts
    /// separately prevent replay of the same diagnostic across restart.
    /// Reserve before sending: an unknown write result must not cause retries.
    pub fn claim_diagnostic(&self, key: RepositoryKey, issue: i64, reason: ReasonCode) -> bool {
        let now = self.clock.now();
        let mut sent = self.diagnostics.lock().unwrap();
        sent.retain(|_, at| now.saturating_sub(*at) < 600);
        if sent.contains_key(&(key, issue, reason)) {
            return false;
        }
        sent.insert((key, issue, reason), now);
        true
    }
    /// Obtain one repository's refresh lock without holding the registry lock during I/O.
    fn entry(&self, key: RepositoryKey) -> Arc<Entry> {
        Arc::clone(self.entries.lock().unwrap().entry(key).or_default())
    }
    /// Fence existing and in-flight snapshots without clearing a GitHub retry deadline.
    /// No entry is allocated when the repository has never requested configuration.
    pub fn invalidate(&self, key: RepositoryKey) {
        if let Some(entry) = self.entries.lock().unwrap().get(&key) {
            entry.generation.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Invalidate a known repository when a verified observation changes its default branch.
    pub fn observe_default_branch(&self, key: RepositoryKey, branch: &str) {
        if let Some(entry) = self.entries.lock().unwrap().get(&key) {
            let mut known = entry.branch.lock().unwrap();
            if known.as_deref() != Some(branch) {
                *known = Some(branch.into());
                entry.generation.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    /// Verified webhooks only. Does not allocate entries or perform remote I/O
    /// for ordinary chat. Invalidations also fence refreshes already in flight.
    pub fn observe_webhook(&self, event: &str, payload: &Value) {
        let (Some(installation_id), Some(repository_id)) = (
            payload["installation"]["id"].as_i64(),
            payload["repository"]["id"].as_i64(),
        ) else {
            return;
        };
        let key = RepositoryKey {
            installation_id,
            repository_id,
        };
        if let Some(branch) = payload["repository"]["default_branch"].as_str() {
            self.observe_default_branch(key, branch);
        }
        if event == "push" {
            if let Some(entry) = self.entries.lock().unwrap().get(&key) {
                let branch = entry.branch.lock().unwrap();
                if branch.as_ref().is_some_and(|b| {
                    payload["ref"].as_str() == Some(format!("refs/heads/{b}").as_str())
                }) {
                    entry.generation.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }

    /// Return a fresh snapshot or a typed failure, coalescing concurrent repository refreshes.
    /// Expired or invalidated snapshots remain diagnostic references only. Failed refreshes
    /// respect their retry deadline, even if a new invalidation arrives during the request.
    pub async fn load(&self, gh: &Client, key: RepositoryKey, repository: &str) -> ConfigState {
        let entry = self.entry(key);
        // All waiters re-check validity/backoff after taking the per-repo lock.
        let mut cached = entry.refresh.lock().await;
        let now = self.clock.now();
        let generation = entry.generation.load(Ordering::SeqCst);
        if cached.generation == generation {
            if let Some(s) = &cached.snapshot {
                if now.saturating_sub(s.verified_at) < TTL_SECS && cached.failure.is_none() {
                    return ConfigState::Ready(Arc::clone(s));
                }
            }
        }
        if let Some((failure, until)) = &cached.failure {
            if now < *until {
                tracing::warn!(?key, reason = ?failure.problem.code, "configuration blocked during retry backoff");
                return ConfigState::Unavailable {
                    problem: failure.problem.clone(),
                    retry_after_secs: until - now,
                    stale_reference: cached.snapshot.clone(),
                };
            }
        }
        let result = fetch(gh, key, repository, cached.snapshot.as_deref()).await;
        let completed = self.clock.now();
        // Lock the observed branch while committing to close the rename race.
        let mut branch = entry.branch.lock().unwrap();
        let result = if result.is_ok() && entry.generation.load(Ordering::SeqCst) != generation {
            Err(FetchError {
                problem: Problem::new(
                    ReasonCode::Invalidated,
                    "default branch changed during configuration refresh",
                ),
                retry_after_secs: 0,
            })
        } else {
            result
        };
        match result {
            Ok(mut snapshot) => {
                for problem in snapshot.config.problems() {
                    tracing::warn!(?key, reason = ?problem.code, detail = problem.detail, "repository configuration domain/rule is unavailable");
                }
                snapshot.verified_at = completed;
                *branch = Some(snapshot.default_branch.clone());
                let snapshot = Arc::new(snapshot);
                cached.snapshot = Some(Arc::clone(&snapshot));
                cached.generation = generation;
                cached.failure = None;
                ConfigState::Ready(snapshot)
            }
            Err(failure) => {
                tracing::warn!(?key, reason = ?failure.problem.code, "repository configuration unavailable");
                cached.failure = Some((
                    failure.clone(),
                    completed.saturating_add(failure.retry_after_secs),
                ));
                ConfigState::Unavailable {
                    problem: failure.problem,
                    retry_after_secs: failure.retry_after_secs,
                    stale_reference: cached.snapshot.clone(),
                }
            }
        }
    }
}

/// Read a required nonempty identity field without including response content in errors.
fn required<'a>(v: &'a Value, key: &str) -> Result<&'a str, FetchError> {
    v[key].as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
        FetchError::new(
            ReasonCode::InvalidResponse,
            "missing configuration snapshot identity",
        )
    })
}

/// Resolve target metadata and the default ref before reading config at an immutable commit.
/// Only a file-path 404 after both reads produces defaults. A 304 may reuse only the
/// same installation/repository, branch, commit and previously cached representation.
async fn fetch(
    gh: &Client,
    key: RepositoryKey,
    repository: &str,
    previous: Option<&Snapshot>,
) -> Result<Snapshot, FetchError> {
    if key.repository_id <= 0
        || key.installation_id <= 0
        || crate::idle_workflows::config::repository_name(repository).is_err()
    {
        return Err(FetchError::new(
            ReasonCode::RepositoryUnavailable,
            "missing or invalid target repository identity",
        ));
    }
    let metadata = gh.config_get(&format!("/repos/{repository}"), None).await?;
    if metadata.status == 404 {
        return Err(FetchError::new(
            ReasonCode::RepositoryUnavailable,
            "target repository is not readable",
        ));
    }
    if metadata.status != 200 || metadata.value["id"].as_i64() != Some(key.repository_id) {
        return Err(FetchError::new(
            ReasonCode::InvalidResponse,
            "target repository identity does not match",
        ));
    }
    let default_branch = required(&metadata.value, "default_branch")?.to_owned();
    let head = gh
        .config_get(
            &format!(
                "/repos/{repository}/git/ref/heads/{}",
                enc_seg(&default_branch)
            ),
            None,
        )
        .await?;
    if head.status == 404 {
        return Err(FetchError::new(
            ReasonCode::BranchUnavailable,
            "default branch is not readable",
        ));
    }
    if head.status != 200 {
        return Err(FetchError::new(
            ReasonCode::InvalidResponse,
            "invalid default branch response",
        ));
    }
    let commit_sha = required(&head.value["object"], "sha")?.to_owned();
    // Immutable ref: a branch move between these requests cannot mix snapshots.
    let previous = previous.filter(|s| {
        s.commit_sha == commit_sha && s.default_branch == default_branch && s.key == key
    });
    let response = gh
        .config_get(
            &format!(
                "/repos/{repository}/contents/{CONFIG_PATH}?ref={}",
                enc_seg(&commit_sha)
            ),
            previous.and_then(|s| s.etag.as_deref()),
        )
        .await?;
    let (config, blob_sha, etag) = match response.status {
        304 => {
            let old = previous.filter(|s| s.etag.is_some()).ok_or_else(|| {
                FetchError::new(
                    ReasonCode::InvalidResponse,
                    "304 without a matching configuration representation",
                )
            })?;
            (
                old.config.clone(),
                old.blob_sha.clone(),
                response.etag.or_else(|| old.etag.clone()),
            )
        }
        404 => {
            // The repository AND default branch were successfully read above.
            (
                RepositoryConfig::parse("", repository).expect("empty document has valid defaults"),
                None,
                None,
            )
        }
        200 => {
            let v = response.value;
            if v["type"] != "file" || v["encoding"] != "base64" || !v["submodule_git_url"].is_null()
            {
                return Err(FetchError::new(
                    ReasonCode::InvalidResponse,
                    "configuration must be a readable regular UTF-8 file",
                ));
            }
            let blob_sha = required(&v, "sha")?.to_owned();
            let content = v["content"].as_str().ok_or_else(|| {
                FetchError::new(ReasonCode::InvalidResponse, "configuration content missing")
            })?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content.replace(['\r', '\n'], ""))
                .map_err(|_| {
                    FetchError::new(ReasonCode::InvalidResponse, "invalid configuration base64")
                })?;
            let text = String::from_utf8(bytes).map_err(|_| {
                FetchError::new(ReasonCode::InvalidResponse, "configuration is not UTF-8")
            })?;
            let config =
                RepositoryConfig::parse(&text, repository).map_err(|problem| FetchError {
                    problem,
                    retry_after_secs: 15,
                })?;
            (config, Some(blob_sha), response.etag)
        }
        _ => unreachable!(),
    };
    Ok(Snapshot {
        key,
        default_branch,
        commit_sha,
        blob_sha,
        config,
        etag,
        verified_at: 0,
    })
}
