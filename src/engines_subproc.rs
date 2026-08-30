//! Subprocess review engines: `pi` and `codex` CLIs (self-hosted / Docker only).
//!
//! Both need a local checkout, so we maintain a per-repo cache under
//! `XERO_DATA_DIR/repos/{repo}` (shallow clone + fetch of the PR ref).
//! pi keeps per-repo sessions under `XERO_DATA_DIR/sessions/{repo}` — that is
//! the incremental "project understanding" memory across reviews.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::config::Config;

/// Where the repo checkout lives for a given repository full name.
fn repo_dir(cfg: &Config, repo: &str) -> PathBuf {
    Path::new(&cfg.data_dir)
        .join("repos")
        .join(repo.replace('/', "__"))
}

fn sessions_dir(cfg: &Config, repo: &str) -> PathBuf {
    Path::new(&cfg.data_dir)
        .join("sessions")
        .join(repo.replace('/', "__"))
}

/// Does a subprocess engine look available? (binary on PATH)
pub async fn engine_available(binary: &str) -> bool {
    tokio::process::Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Clone-or-fetch the repo at `ref` into the cache dir. `token` is an
/// installation token used for the clone URL.
pub async fn ensure_checkout(
    cfg: &Config,
    repo: &str,
    pr_ref: &str,
    token: &str,
) -> Result<PathBuf, String> {
    let dir = repo_dir(cfg, repo);
    let parent = dir
        .parent()
        .ok_or_else(|| "bad repo dir".to_string())?
        .to_path_buf();
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|e| e.to_string())?;

    let authed_url = format!("https://x-access-token:{token}@github.com/{repo}.git");

    if !dir.join(".git").exists() {
        // fresh shallow clone of the base repo
        let out = tokio::process::Command::new("git")
            .args(["clone", "--depth=50", "--no-single-branch", &authed_url])
            .arg(&dir)
            .output()
            .await
            .map_err(|e| format!("git clone spawn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    } else {
        // existing cache: fetch the PR ref
        let out = tokio::process::Command::new("git")
            .args(["fetch", "--depth=50", "origin", pr_ref])
            .current_dir(&dir)
            .output()
            .await
            .map_err(|e| format!("git fetch spawn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git fetch failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }

    // check out the PR ref
    let out = tokio::process::Command::new("git")
        .args(["checkout", "--force", "FETCH_HEAD"])
        .current_dir(&dir)
        .output()
        .await
        .map_err(|e| format!("git checkout spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(dir)
}
