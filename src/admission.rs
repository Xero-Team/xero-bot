//! Deployment-owned admission policy. Repository TOML cannot grant access.
//! Names prevent arbitrary routing; immutable IDs prevent replacement/transfer.
use crate::{
    config::Config,
    forgejo::{ForgejoClient, ForgejoError},
    github::{Client, GhError},
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    github: Vec<GitHubRepository>,
    #[serde(default)]
    forgejo: Vec<Repository>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitHubRepository {
    installation_id: i64,
    owner_id: i64,
    repository_id: i64,
    repository: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Repository {
    owner_id: i64,
    repository_id: i64,
    repository: String,
}

impl Policy {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = if raw.trim().is_empty() { "{}" } else { raw };
        if !raw.trim_start().starts_with('{') {
            return Err("BOT_ACCESS_POLICY must be a JSON object".into());
        }
        let policy: Self =
            serde_json::from_str(raw).map_err(|e| format!("invalid BOT_ACCESS_POLICY: {e}"))?;
        let mut names = HashSet::new();
        let mut ids = HashSet::new();
        let mut installations = std::collections::HashMap::new();
        for entry in &policy.github {
            if entry.installation_id <= 0
                || entry.owner_id <= 0
                || entry.repository_id <= 0
                || !valid_name(&entry.repository)
                || !names.insert(entry.repository.to_ascii_lowercase())
                || !ids.insert(entry.repository_id)
                || installations
                    .insert(entry.installation_id, entry.owner_id)
                    .is_some_and(|id| id != entry.owner_id)
            {
                return Err(
                    "invalid BOT_ACCESS_POLICY GitHub identity or duplicate repository".into(),
                );
            }
        }
        for installation in installations.keys() {
            if policy
                .github
                .iter()
                .filter(|e| e.installation_id == *installation)
                .count()
                > 500
            {
                return Err(
                    "BOT_ACCESS_POLICY allows at most 500 repositories per installation".into(),
                );
            }
        }
        names.clear();
        ids.clear();
        for entry in &policy.forgejo {
            if entry.owner_id <= 0
                || entry.repository_id <= 0
                || !valid_name(&entry.repository)
                || !names.insert(entry.repository.to_ascii_lowercase())
                || !ids.insert(entry.repository_id)
            {
                return Err(
                    "invalid BOT_ACCESS_POLICY Forgejo identity or duplicate repository".into(),
                );
            }
        }
        Ok(policy)
    }

    fn github_entry(
        &self,
        installation: i64,
        name: &str,
        id: Option<i64>,
    ) -> Option<&GitHubRepository> {
        self.github.iter().find(|e| {
            e.installation_id == installation
                && e.repository.eq_ignore_ascii_case(name)
                && id.is_none_or(|id| e.repository_id == id)
        })
    }
    fn forgejo_entry(&self, name: &str, id: Option<i64>) -> Option<&Repository> {
        self.forgejo.iter().find(|e| {
            e.repository.eq_ignore_ascii_case(name) && id.is_none_or(|id| e.repository_id == id)
        })
    }
}
fn valid_name(name: &str) -> bool {
    let parts: Vec<_> = name.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}
fn policy(cfg: &Config) -> Option<Policy> {
    Policy::parse(&cfg.bot_access_policy).ok()
}
fn denied() -> GhError {
    GhError::Api {
        status: 403,
        message: "repository denied by BOT_ACCESS_POLICY".into(),
    }
}
fn forgejo_denied() -> ForgejoError {
    ForgejoError::Api {
        status: 403,
        message: "repository denied by BOT_ACCESS_POLICY".into(),
    }
}

pub fn github_installation(cfg: &Config, installation: i64) -> Result<(), GhError> {
    if policy(cfg).is_some_and(|p| p.github.iter().any(|e| e.installation_id == installation)) {
        Ok(())
    } else {
        Err(denied())
    }
}
pub fn github_scope(cfg: &Config, installation: i64, name: &str, id: Option<i64>) -> bool {
    policy(cfg).is_some_and(|p| p.github_entry(installation, name, id).is_some())
}
/// Resolve only a deployment-admitted name. Callers must still verify live
/// metadata and visibility before returning any project information publicly.
pub(crate) fn github_identity(cfg: &Config, name: &str) -> Option<(i64, i64)> {
    policy(cfg)?
        .github
        .iter()
        .find(|entry| entry.repository.eq_ignore_ascii_case(name))
        .map(|entry| (entry.installation_id, entry.repository_id))
}
/// Discovery candidates only: live identity and visibility must still be checked.
pub(crate) fn github_installations(cfg: &Config) -> Vec<i64> {
    policy(cfg)
        .unwrap_or_default()
        .github
        .into_iter()
        .map(|entry| entry.installation_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}
pub(crate) fn forgejo_repositories(cfg: &Config) -> Vec<String> {
    policy(cfg)
        .unwrap_or_default()
        .forgejo
        .into_iter()
        .map(|entry| entry.repository)
        .collect()
}
pub fn forgejo_scope(cfg: &Config, name: &str, id: Option<i64>) -> bool {
    policy(cfg).is_some_and(|p| p.forgejo_entry(name, id).is_some())
}
/// Validate the whole identity tuple from a verified webhook or live provider response.
pub fn github_metadata(cfg: &Config, installation: i64, meta: &Value) -> bool {
    let Some(p) = policy(cfg) else { return false };
    let (Some(name), Some(id), Some(owner)) = (
        meta["full_name"].as_str(),
        meta["id"].as_i64(),
        meta["owner"]["id"].as_i64(),
    ) else {
        return false;
    };
    p.github_entry(installation, name, Some(id))
        .is_some_and(|e| e.owner_id == owner)
}
pub fn github_webhook(cfg: &Config, payload: &Value) -> bool {
    payload["installation"]["id"]
        .as_i64()
        .is_some_and(|id| github_metadata(cfg, id, &payload["repository"]))
}
pub fn forgejo_metadata(cfg: &Config, meta: &Value) -> bool {
    let Some(p) = policy(cfg) else { return false };
    let (Some(name), Some(id), Some(owner)) = (
        meta["full_name"].as_str(),
        meta["id"].as_i64(),
        meta["owner"]["id"].as_i64(),
    ) else {
        return false;
    };
    p.forgejo_entry(name, Some(id))
        .is_some_and(|e| e.owner_id == owner)
}
/// Recheck durable work before loading TOML, running commands or touching Git.
/// Denied names/IDs do not even cause an API call. A live lookup catches transfers.
pub async fn github_repository(
    cfg: &Config,
    gh: &Client,
    installation: i64,
    name: &str,
    id: Option<i64>,
) -> Result<Value, GhError> {
    if !github_scope(cfg, installation, name, id) {
        return Err(denied());
    }
    let meta = gh.repo_info(name).await?;
    if !meta["full_name"]
        .as_str()
        .is_some_and(|n| n.eq_ignore_ascii_case(name))
        || !github_metadata(cfg, installation, &meta)
    {
        return Err(denied());
    }
    Ok(meta)
}
pub async fn forgejo_repository(
    cfg: &Config,
    client: &ForgejoClient,
    name: &str,
    id: Option<i64>,
) -> Result<Value, ForgejoError> {
    if !forgejo_scope(cfg, name, id) {
        return Err(forgejo_denied());
    }
    let meta = client.repository(name).await?;
    if !meta["full_name"]
        .as_str()
        .is_some_and(|n| n.eq_ignore_ascii_case(name))
        || !forgejo_metadata(cfg, &meta)
    {
        return Err(forgejo_denied());
    }
    Ok(meta)
}
/// Raw Git credentials are restricted to exactly the admitted repository IDs.
pub fn github_token_repositories(cfg: &Config, installation: i64) -> Result<Vec<i64>, GhError> {
    github_installation(cfg, installation)?;
    let p = policy(cfg).ok_or_else(denied)?;
    Ok(p.github
        .into_iter()
        .filter(|e| e.installation_id == installation)
        .map(|e| e.repository_id)
        .collect())
}

#[cfg(test)]
mod tests;
