//! Opt-in Git transport for explicitly bound ordinary repositories.
//! Only heads/tags are copied; no force pushes, pruning, or refs/pull writes.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;

use super::ForgejoClient;
use crate::{config::Config, github::Client};

struct Remote {
    url: String,
    authorization: String,
}

impl Remote {
    fn new(url: String, user: &str, token: &str) -> Self {
        Self {
            url,
            authorization: format!(
                "Authorization: Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"))
            ),
        }
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "xero-ref-sync-{}-{}-{}",
            std::process::id(),
            crate::github::chrono_now_secs(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|_| "cannot create private ref sync directory")?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Credentials exist only in child environment, never URLs, argv or git config.
/// Ignore global configuration and disable redirects, helpers and hooks.
async fn git(
    dir: &Path,
    remote: Option<&Remote>,
    args: &[&str],
) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_PARAMETERS",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
    ] {
        cmd.env_remove(key);
    }
    let mut config = vec![
        ("credential.helper".to_string(), String::new()),
        ("core.hooksPath".into(), "/dev/null".into()),
        ("http.followRedirects".into(), "false".into()),
        ("protocol.allow".into(), "never".into()),
        ("protocol.https.allow".into(), "always".into()),
        ("protocol.http.allow".into(), "always".into()),
    ];
    #[cfg(test)]
    config.push(("protocol.file.allow".into(), "always".into()));
    if let Some(remote) = remote {
        config.push((
            format!("http.{}.extraHeader", remote.url),
            remote.authorization.clone(),
        ));
    }
    cmd.env("GIT_CONFIG_COUNT", config.len().to_string());
    for (i, (key, value)) in config.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), key)
            .env(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    // Never surface raw stderr: remote errors can echo credentials.
    tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .map_err(|_| "Git ref transport timed out; inspect refs before retrying".to_string())?
        .map_err(|_| "cannot start Git ref transport".to_string())
}

async fn run(dir: &Path, remote: Option<&Remote>, args: &[&str]) -> Result<String, String> {
    let result = git(dir, remote, args).await?;
    if !result.status.success() {
        return Err(format!(
            "Git {} rejected; inspect branch permissions or divergence",
            args[0]
        ));
    }
    String::from_utf8(result.stdout).map_err(|_| "invalid Git output".into())
}

async fn references(dir: &Path, prefix: &str) -> Result<BTreeMap<String, String>, String> {
    let text = run(
        dir,
        None,
        &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
    )
    .await?;
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let (name, sha) = line.split_once(' ').ok_or("invalid Git ref record")?;
        let name = name
            .strip_prefix(prefix)
            .ok_or("unexpected Git ref prefix")?;
        result.insert(format!("refs/{name}"), sha.into());
        if result.len() > 5000 {
            return Err("too many refs for bounded synchronization".into());
        }
    }
    Ok(result)
}

async fn ancestor(dir: &Path, before: &str, after: &str) -> Result<bool, String> {
    match git(dir, None, &["merge-base", "--is-ancestor", before, after])
        .await?
        .status
        .code()
    {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("cannot compare branch history".into()),
    }
}

/// A single selected branch must reach the exact source tip before a PR is made.
/// Reconciliation can leave a destination-ahead branch alone. Conflicts fail
/// visibly; already completed fast-forwards are safe to inspect/reconcile again.
async fn transfer(
    source: &Remote,
    destination: &Remote,
    branch: Option<&str>,
) -> Result<(), String> {
    tokio::time::timeout(
        Duration::from_secs(300),
        transfer_inner(source, destination, branch),
    )
    .await
    .map_err(|_| "Git ref synchronization exceeded its total deadline".to_string())?
}

async fn transfer_inner(
    source: &Remote,
    destination: &Remote,
    branch: Option<&str>,
) -> Result<(), String> {
    let scratch = Scratch::new()?;
    let dir = &scratch.0;
    run(dir, None, &["init", "--bare", "--quiet"]).await?;
    if let Some(branch) = branch {
        run(
            dir,
            None,
            &["check-ref-format", &format!("refs/heads/{branch}")],
        )
        .await?;
    }
    run(
        dir,
        Some(source),
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &source.url,
            "+refs/heads/*:refs/source/heads/*",
            "+refs/tags/*:refs/source/tags/*",
        ],
    )
    .await?;
    run(
        dir,
        Some(destination),
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &destination.url,
            "+refs/heads/*:refs/destination/heads/*",
            "+refs/tags/*:refs/destination/tags/*",
        ],
    )
    .await?;
    let sources = references(dir, "refs/source/").await?;
    let targets = references(dir, "refs/destination/").await?;
    let selected = branch.map(|b| format!("refs/heads/{b}"));
    if selected.as_ref().is_some_and(|r| !sources.contains_key(r)) {
        return Err("PR source branch does not exist".into());
    }
    let mut conflicts = Vec::new();
    let mut updates = Vec::new();
    for (name, sha) in sources {
        if selected.as_ref().is_some_and(|r| *r != name) {
            continue;
        }
        if let Some(previous) = targets.get(&name) {
            if previous == &sha {
                continue;
            }
            if !name.starts_with("refs/heads/") {
                conflicts.push(name);
                continue;
            }
            if !ancestor(dir, previous, &sha).await? {
                if branch.is_none() && ancestor(dir, &sha, previous).await? {
                    continue;
                }
                conflicts.push(name);
                continue;
            }
        }
        updates.push(format!("{sha}:{name}"));
    }
    if !updates.is_empty() {
        // One bounded push; no '+' or force. Receive-side checks guard races
        // with user pushes after the destination snapshot was fetched.
        let mut args = vec!["push", "--porcelain", &destination.url];
        args.extend(updates.iter().map(String::as_str));
        run(dir, Some(destination), &args).await?;
    }
    if !conflicts.is_empty() {
        return Err(format!(
            "ref sync conflict (no overwrite): {}",
            conflicts.join(", ")
        ));
    }
    Ok(())
}

async fn remotes(
    gh: &Client,
    cfg: &Config,
    installation: i64,
    github_repo: &str,
    forgejo_repo: &str,
) -> Result<(Remote, Remote), String> {
    if !super::commands::valid_repository(github_repo)
        || !super::commands::valid_repository(forgejo_repo)
    {
        return Err("invalid bound repository".into());
    }
    let client =
        ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token).map_err(|e| e.to_string())?;
    let metadata = client
        .repository(forgejo_repo)
        .await
        .map_err(|e| e.to_string())?;
    if metadata["mirror"].as_bool() != Some(false) {
        return Err("Git ref sync requires an ordinary Forgejo repository".into());
    }
    let user = client.current_user().await.map_err(|e| e.to_string())?;
    let login = user["login"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or("Forgejo login missing")?;
    let token = gh
        .installation_token(cfg, installation)
        .await
        .map_err(|e| e.to_string())?;
    let root = client
        .api_base
        .as_str()
        .strip_suffix("api/v1/")
        .ok_or("invalid Forgejo Git root")?;
    Ok((
        Remote::new(
            format!("https://github.com/{github_repo}.git"),
            "x-access-token",
            &token,
        ),
        Remote::new(
            format!("{root}{forgejo_repo}.git"),
            login,
            &cfg.forgejo_token,
        ),
    ))
}

pub(super) async fn sync_all(
    gh: &Client,
    cfg: &Config,
    installation: i64,
    github_repo: &str,
    forgejo_repo: &str,
) -> Result<(), String> {
    let lock = super::sync::target_lock(&format!(
        "refs|{}|{}",
        cfg.forgejo_base_url,
        forgejo_repo.to_ascii_lowercase()
    ));
    let _guard = lock.lock().await;
    let (github, forgejo) = remotes(gh, cfg, installation, github_repo, forgejo_repo).await?;
    transfer(&github, &forgejo, None).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn sync_pr(
    gh: &Client,
    cfg: &Config,
    installation: i64,
    github_repo: &str,
    forgejo_repo: &str,
    head: &str,
    base: &str,
    from_forgejo: bool,
) -> Result<(), String> {
    if head == base {
        return Err("PR head equals base".into());
    }
    let lock = super::sync::target_lock(&format!(
        "refs|{}|{}",
        cfg.forgejo_base_url,
        forgejo_repo.to_ascii_lowercase()
    ));
    let _guard = lock.lock().await;
    let (github, forgejo) = remotes(gh, cfg, installation, github_repo, forgejo_repo).await?;
    // Base history remains GitHub-owned, including for Forgejo-origin PRs.
    transfer(&github, &forgejo, Some(base)).await?;
    if from_forgejo {
        let metadata = gh.repo_info(github_repo).await.map_err(|e| e.to_string())?;
        if metadata["default_branch"].as_str() == Some(head) {
            return Err("cannot import a PR head onto the GitHub default branch".into());
        }
        transfer(&forgejo, &github, Some(head)).await
    } else {
        transfer(&github, &forgejo, Some(head)).await
    }
}

#[cfg(test)]
#[path = "refs_tests.rs"]
mod tests;
