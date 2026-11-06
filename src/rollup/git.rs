//! Prepare one immutable candidate locally; only the final ref triggers CI.
use super::*;
use crate::{
    config::Config,
    forgejo::refs::{run, Remote, Scratch},
    github::Client,
};
pub struct Prepared {
    scratch: Scratch,
    remote: Remote,
    pub tip: String,
}
fn github_url(repo: &str) -> String {
    format!("https://github.com/{repo}.git")
}
pub async fn prepare(
    gh: &Client,
    cfg: &Config,
    repo: &Repository,
    batch: &Batch,
) -> Result<Prepared> {
    let token = gh.installation_token(cfg, repo.installation).await?;
    let remote = Remote::new(github_url(&repo.name), "x-access-token", &token);
    let scratch = Scratch::new()?;
    let path = &scratch.0;
    run(path, None, &["init", "--quiet"]).await?;
    run(path, None, &["config", "user.name", "xero-bot"]).await?;
    run(
        path,
        None,
        &["config", "user.email", "xero-bot@users.noreply.github.com"],
    )
    .await?;
    run(
        path,
        Some(&remote),
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &github_url(&repo.name),
            &batch.base,
        ],
    )
    .await?;
    run(
        path,
        None,
        &["checkout", "--quiet", "--detach", &batch.base],
    )
    .await?;
    for (key, head) in batch.members.iter().zip(&batch.heads) {
        let entry = repo.entry(key).ok_or("missing batch entry")?;
        let reference = format!("refs/pull/{}/head", entry.number);
        run(
            path,
            Some(&remote),
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                &github_url(&repo.name),
                &reference,
            ],
        )
        .await?;
        let current = run(path, None, &["rev-parse", "FETCH_HEAD"]).await?;
        if current.trim() != head {
            return Err("PR head changed during candidate preparation".into());
        }
        run(
            path,
            None,
            &[
                "merge",
                "--no-ff",
                "--no-edit",
                "--no-verify",
                "-m",
                &format!("xero-bot: merge #{} (head {head})", entry.number),
                head,
            ],
        )
        .await
        .map_err(|_| "candidate merge conflict; inspect PR compatibility")?;
    }
    let tip = run(path, None, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string();
    Ok(Prepared {
        scratch,
        remote,
        tip,
    })
}
impl Prepared {
    pub async fn publish(&self, repo: &Repository, branch: &str) -> Result<()> {
        run(
            &self.scratch.0,
            Some(&self.remote),
            &[
                "push",
                "--porcelain",
                &github_url(&repo.name),
                &format!("{}:refs/heads/{branch}", self.tip),
            ],
        )
        .await?;
        Ok(())
    }
}
pub async fn fallback(
    gh: &Client,
    cfg: &Config,
    repo: &Repository,
    batch: &Batch,
    destination: &str,
) -> Result<()> {
    let token = gh.installation_token(cfg, repo.installation).await?;
    let source = Remote::new(github_url(&repo.name), "x-access-token", &token);
    let fj = crate::forgejo::ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)?;
    let me = fj.current_user().await?;
    let user = me["login"].as_str().ok_or("Forgejo identity missing")?;
    let base = cfg
        .forgejo_base_url
        .trim_end_matches('/')
        .trim_end_matches("/api/v1");
    let url = format!("{base}/{destination}.git");
    let target = Remote::new(url.clone(), user, &cfg.forgejo_token);
    let scratch = Scratch::new()?;
    run(&scratch.0, None, &["init", "--bare", "--quiet"]).await?;
    run(
        &scratch.0,
        Some(&source),
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &github_url(&repo.name),
            &batch.tip,
        ],
    )
    .await?;
    run(
        &scratch.0,
        Some(&target),
        &[
            "push",
            "--porcelain",
            &url,
            &format!("{}:refs/heads/{}", batch.tip, batch.branch),
        ],
    )
    .await?;
    Ok(())
}
