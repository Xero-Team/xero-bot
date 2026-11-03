//! GitHub Push driven one-way Forgejo mirror sync.
//!
//! This module only reads GitHub configuration and asks an existing Forgejo
//! pull mirror to fetch. It never creates a repo, stores GitHub credentials in
//! Forgejo, or writes a Git ref to GitHub.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde_json::Value;

use super::{ForgejoClient, ForgejoError};
use crate::config::cache::{RepositoryConfigCache, RepositoryKey};
use crate::config::repository::ForgejoMirrorConfig;
use crate::config::Config;
use crate::github::Client;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    ProviderDisabled,
    MirrorDisabled,
    MissingDestination,
    InvalidConfiguration,
    Synced,
    Rejected(u16),
    Transport(String),
}

impl std::fmt::Display for SyncOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderDisabled => f.write_str("forgejo-provider-disabled"),
            Self::MirrorDisabled => f.write_str("mirror-disabled"),
            Self::MissingDestination => f.write_str("missing-destination"),
            Self::InvalidConfiguration => f.write_str("invalid-configuration"),
            Self::Synced => f.write_str("synced"),
            Self::Rejected(status) => write!(f, "rejected-{status}"),
            Self::Transport(message) => write!(f, "transport-error: {message}"),
        }
    }
}

/// Load one repository's policy and trigger its declared mirror. Every push ref
/// reaches this path, including branches, tags, and deletions.
pub async fn sync_repository(
    gh: &Client,
    cfg: &Config,
    repo: &str,
    key: RepositoryKey,
) -> SyncOutcome {
    if !cfg.forgejo_ready() {
        return SyncOutcome::ProviderDisabled;
    }
    let state = RepositoryConfigCache::shared().load(gh, key, repo).await;
    let snapshot = match state.snapshot() {
        Ok(snapshot) => snapshot,
        Err(problem) => {
            tracing::warn!(
                repo,
                reason = ?problem.code,
                "Forgejo mirror sync blocked by unavailable repository configuration"
            );
            return SyncOutcome::InvalidConfiguration;
        }
    };
    let mirror = match &snapshot.config.forgejo {
        Ok(mirror) => mirror,
        Err(problem) => {
            tracing::warn!(
                repo,
                reason = ?problem.code,
                "Forgejo mirror sync blocked by invalid repository configuration"
            );
            return SyncOutcome::InvalidConfiguration;
        }
    };
    let destination = match mirror_destination(mirror) {
        Ok(destination) => destination,
        Err(outcome) => {
            if outcome == SyncOutcome::MissingDestination {
                tracing::warn!(repo, "Forgejo mirror sync enabled without a destination");
            }
            return outcome;
        }
    };
    sync_destination(cfg, destination).await
}

fn mirror_destination(mirror: &ForgejoMirrorConfig) -> Result<&str, SyncOutcome> {
    if !mirror.enabled {
        return Err(SyncOutcome::MirrorDisabled);
    }
    mirror
        .destination
        .as_deref()
        .filter(|destination| valid_destination(destination))
        .ok_or(SyncOutcome::MissingDestination)
}

async fn sync_destination(cfg: &Config, destination: &str) -> SyncOutcome {
    sync_destination_with(&cfg.forgejo_base_url, &cfg.forgejo_token, destination).await
}

async fn sync_destination_with(base_url: &str, token: &str, destination: &str) -> SyncOutcome {
    if !valid_destination(destination) {
        return SyncOutcome::InvalidConfiguration;
    }
    let client = match ForgejoClient::new(base_url, token) {
        Ok(client) => client,
        Err(error) => return SyncOutcome::Transport(error.to_string()),
    };
    let lock_key = format!("{}|{destination}", base_url.trim_end_matches('/'));
    let lock = target_lock(&lock_key);
    let _guard = lock.lock().await;
    match client.trigger_mirror_sync(destination).await {
        Ok(()) => SyncOutcome::Synced,
        Err(ForgejoError::Api { status, .. }) => SyncOutcome::Rejected(status),
        Err(error) => SyncOutcome::Transport(error.to_string()),
    }
}

/// One lock per Forgejo base URL + destination. Weak references let idle
/// targets disappear without retaining process-lifetime state.
fn target_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = match locks.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key.to_string(), Arc::downgrade(&lock));
    lock
}

fn valid_destination(destination: &str) -> bool {
    let Some((owner, repo)) = destination.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !repo.is_empty()
        && !repo.contains('/')
        && !destination.contains("..")
        && [owner, repo].into_iter().all(|part| {
            !part
                .chars()
                .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
        })
}

/// Periodic reconciliation. The webhook path is primary; this covers a missed
/// delivery or a process restart by re-reading every opted-in repository.
pub async fn sweep(cfg: &Config) -> String {
    if !cfg.forgejo_ready() {
        return "forgejo provider disabled".into();
    }
    let app = match Client::app_client(cfg) {
        Ok(app) => app,
        Err(error) => return format!("app-client-error: {error}"),
    };
    let installations: Vec<Value> =
        match crate::github::paginate(&app, "/app/installations?per_page=100").await {
            Ok(items) => items,
            Err(error) => return format!("installations-error: {error}"),
        };
    let mut synced = 0usize;
    let mut skipped = 0usize;
    let mut errors = 0usize;
    for installation in installations {
        let Some(installation_id) = installation["id"].as_i64().filter(|id| *id > 0) else {
            errors += 1;
            continue;
        };
        let gh = match Client::installation_resolved(cfg, installation_id).await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(installation_id, "Forgejo sweep client failed: {error}");
                errors += 1;
                continue;
            }
        };
        let repos = match Client::installation_repositories_via(&gh).await {
            Ok(repos) => repos,
            Err(error) => {
                tracing::warn!(
                    installation_id,
                    "Forgejo sweep repository list failed: {error}"
                );
                errors += 1;
                continue;
            }
        };
        for repo in repos {
            let metadata = match gh.repo_info(&repo).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(repo, "Forgejo sweep metadata failed: {error}");
                    errors += 1;
                    continue;
                }
            };
            let Some(repository_id) = metadata["id"].as_i64().filter(|id| *id > 0) else {
                errors += 1;
                continue;
            };
            let outcome = sync_repository(
                &gh,
                cfg,
                &repo,
                RepositoryKey {
                    installation_id,
                    repository_id,
                },
            )
            .await;
            match outcome {
                SyncOutcome::Synced => synced += 1,
                SyncOutcome::ProviderDisabled | SyncOutcome::MirrorDisabled => skipped += 1,
                _ => {
                    tracing::warn!(repo, "Forgejo sweep sync failed: {outcome}");
                    errors += 1;
                }
            }
        }
    }
    format!("{synced} synced, {skipped} skipped, {errors} errors")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn disabled_and_missing_mirror_plans_never_produce_a_target() {
        let disabled = ForgejoMirrorConfig::disabled();
        assert_eq!(
            mirror_destination(&disabled),
            Err(SyncOutcome::MirrorDisabled)
        );
        let enabled_without_target = ForgejoMirrorConfig {
            enabled: true,
            destination: None,
        };
        assert_eq!(
            mirror_destination(&enabled_without_target),
            Err(SyncOutcome::MissingDestination)
        );
        let enabled = ForgejoMirrorConfig {
            enabled: true,
            destination: Some("team/repo".into()),
        };
        assert_eq!(mirror_destination(&enabled), Ok("team/repo"));
    }

    #[test]
    fn destination_validation_matches_repository_policy() {
        for good in ["owner/repo", "team/xero-bot", "a_b/c.d"] {
            assert!(valid_destination(good), "{good}");
        }
        for bad in [
            "",
            "owner",
            "owner/",
            "/repo",
            "owner/repo/extra",
            "owner/../repo",
            "owner/repo?ref=main",
            "https://git.example.com/owner/repo",
            "token@owner/repo",
            "ownér/repo",
        ] {
            assert!(!valid_destination(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn enabled_destination_calls_only_the_mirror_sync_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/mirror-team/xero-bot/mirror-sync"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let outcome = sync_destination_with(&server.uri(), "token", "mirror-team/xero-bot").await;
        assert_eq!(outcome, SyncOutcome::Synced);
    }

    #[tokio::test]
    async fn rejected_and_missing_targets_are_classified_without_retry() {
        let server = MockServer::start().await;
        for status in [401, 403, 404, 409] {
            let target = format!("mirror-team/status-{status}");
            Mock::given(method("POST"))
                .and(path(format!("/api/v1/repos/{target}/mirror-sync")))
                .respond_with(
                    ResponseTemplate::new(status).set_body_json(serde_json::json!({
                        "message": "sync rejected"
                    })),
                )
                .expect(1)
                .mount(&server)
                .await;
            assert_eq!(
                sync_destination_with(&server.uri(), "token", &target).await,
                SyncOutcome::Rejected(status)
            );
        }
    }

    #[test]
    fn concurrent_syncs_for_one_target_share_the_same_lock() {
        let first = target_lock("https://git.example.com|team/repo");
        let second = target_lock("https://git.example.com|team/repo");
        let other = target_lock("https://git.example.com|team/other");
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
    }
}
