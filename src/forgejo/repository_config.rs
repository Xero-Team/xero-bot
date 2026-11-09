//! Load the shared repository policy through the Forgejo API.

use super::{ForgejoClient, ForgejoError};
use crate::config::repository::{RepositoryConfig, CONFIG_PATH};

#[cfg(test)]
#[path = "repository_config_tests.rs"]
mod tests;

pub struct RepositoryConfigSnapshot {
    pub config: RepositoryConfig,
    pub default_branch: String,
    pub commit_sha: String,
    pub file_present: bool,
}

impl ForgejoClient {
    /// Read the complete default-branch policy at a pinned commit. Domain errors
    /// remain in the returned config so consumers can fail closed for their domain.
    pub async fn repository_config(&self, repo: &str) -> Result<RepositoryConfig, String> {
        self.repository_config_snapshot(repo)
            .await
            .map(|snapshot| snapshot.config)
    }

    pub async fn repository_config_snapshot(
        &self,
        repo: &str,
    ) -> Result<RepositoryConfigSnapshot, String> {
        if !super::commands::valid_repository(repo) {
            return Err("invalid Forgejo repository name".into());
        }
        let client = self;
        let metadata = client
            .repository(repo)
            .await
            .map_err(|error| format!("cannot read Forgejo repository metadata: {error}"))?;
        let branch = metadata["default_branch"]
            .as_str()
            .filter(|branch| !branch.is_empty())
            .ok_or("Forgejo repository metadata has no default branch")?;
        let branch_info = client
            .get(&format!(
                "/repos/{repo}/branches/{}",
                crate::github::enc_seg(branch)
            ))
            .await
            .map_err(|e| format!("cannot resolve Forgejo default branch: {e}"))?;
        let commit = branch_info["commit"]["id"]
            .as_str()
            .filter(|sha| {
                matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .ok_or("Forgejo default branch has no commit identity")?;
        let route = format!(
            "/repos/{repo}/contents/{}?ref={}",
            crate::github::enc_path(CONFIG_PATH),
            crate::github::enc_seg(commit)
        );
        let value = match client.get(&route).await {
            Ok(value) => value,
            Err(ForgejoError::Api { status: 404, .. }) => {
                confirm_policy_absent(client, repo, commit).await?;
                return Ok(RepositoryConfigSnapshot {
                    config: RepositoryConfig::parse("", repo)
                        .map_err(|problem| problem.to_string())?,
                    default_branch: branch.into(),
                    commit_sha: commit.into(),
                    file_present: false,
                });
            }
            Err(error) => return Err(format!("cannot read Forgejo repository policy: {error}")),
        };
        if value["type"].as_str() != Some("file") || value["encoding"].as_str() != Some("base64") {
            return Err("Forgejo repository policy is not a readable base64 file".into());
        }
        let encoded = value["content"]
            .as_str()
            .ok_or("Forgejo repository policy content is missing")?
            .replace(['\r', '\n'], "");
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "Forgejo repository policy is not valid base64")?;
        let text =
            String::from_utf8(bytes).map_err(|_| "Forgejo repository policy is not valid UTF-8")?;
        Ok(RepositoryConfigSnapshot {
            config: RepositoryConfig::parse(&text, repo).map_err(|problem| problem.to_string())?,
            default_branch: branch.into(),
            commit_sha: commit.into(),
            file_present: true,
        })
    }
}

/// A file 404 can hide missing permissions. Only a readable parent listing at
/// the same commit can prove absence; a second ambiguous 404 remains closed.
async fn confirm_policy_absent(
    client: &ForgejoClient,
    repo: &str,
    commit: &str,
) -> Result<(), String> {
    let parent = client
        .get(&format!("/repos/{repo}/contents/.github?ref={commit}"))
        .await;
    let (listing, name) = match parent {
        Ok(listing) => (listing, "xero-bot.toml"),
        Err(ForgejoError::Api { status: 404, .. }) => (
            client
                .get(&format!("/repos/{repo}/contents?ref={commit}"))
                .await
                .map_err(|e| format!("cannot verify Forgejo policy absence: {e}"))?,
            ".github",
        ),
        Err(e) => return Err(format!("cannot verify Forgejo policy absence: {e}")),
    };
    let entries = listing
        .as_array()
        .ok_or("Forgejo policy parent is not a directory listing")?;
    for entry in entries {
        let current = entry["name"]
            .as_str()
            .ok_or("malformed Forgejo directory listing")?;
        if current == name {
            return Err("Forgejo policy exists but its contents are unreadable".into());
        }
    }
    Ok(())
}
