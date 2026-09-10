//! Opt-in repository workflow scheduling. Webhooks record activity; a single
//! reconciler checks GitHub before every write. SQLite survives process restarts.

pub mod config;
mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::github::actions::{Workflow, WorkflowRun};
use crate::github::{normalize_login, Client, GhError};
use config::{Rules, Task, CONFIG_PATH};
use store::{Pending, Store, TargetState};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const CONFIRMATION_GRACE_SECS: i64 = 300;

pub struct Scheduler {
    store: Store,
    pump: tokio::sync::Mutex<()>,
    boot: i64,
    poll_secs: u64,
}

struct Repository {
    name: String,
    default_branch: String,
    gh: Arc<Client>,
    actions_read: bool,
    actions_write: bool,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Scheduler {
    pub fn open(data_dir: &Path, poll_secs: u64) -> Result<Self> {
        Ok(Self {
            store: Store::open(&data_dir.join("workflow-scheduler.sqlite"))?,
            pump: tokio::sync::Mutex::new(()),
            boot: now(),
            poll_secs,
        })
    }

    /// Called only after HMAC verification, before acknowledging the webhook.
    /// Errors propagate so an activity update is never silently acknowledged.
    pub fn observe_webhook(
        &self,
        event: &str,
        payload: &Value,
        delivery: Option<&str>,
    ) -> Result<()> {
        let activity = match event {
            "push" => {
                payload["ref"]
                    .as_str()
                    .is_some_and(|r| r.starts_with("refs/heads/"))
                    && payload["deleted"].as_bool() != Some(true)
            }
            "pull_request" => {
                matches!(
                    payload["action"].as_str(),
                    Some("opened" | "synchronize" | "reopened")
                ) || (payload["action"] == "closed" && payload["pull_request"]["merged"] == true)
            }
            "merge_group" => payload["action"] == "checks_requested",
            _ => false,
        };
        if activity && payload["installation"]["id"].as_i64().is_some() {
            if let Some(repo) = payload["repository"]["full_name"].as_str() {
                let repo = config::repository_name(repo)?;
                self.store.activity(&repo, now(), delivery)?;
            }
        }
        Ok(())
    }

    /// Both /cron and the built-in timer share this lock. Never run two writers
    /// concurrently, even when an external cron overlaps the background loop.
    pub async fn pump_all(&self, cfg: &Config) -> String {
        if !cfg.idle_workflows_enabled {
            return "idle workflows disabled".into();
        }
        let Ok(_guard) = self.pump.try_lock() else {
            return "idle workflow reconciliation already running".into();
        };
        match tokio::time::timeout(
            std::time::Duration::from_secs(300),
            self.discover_and_pump(cfg),
        )
        .await
        {
            Ok(Ok(summary)) => summary,
            Ok(Err(error)) => {
                tracing::warn!("idle workflows: {error}");
                format!("idle workflows error: {error}")
            }
            Err(_) => {
                tracing::warn!(
                    "idle workflow reconciliation timed out; pending writes will be reconciled"
                );
                "idle workflows: reconciliation timed out".into()
            }
        }
    }

    async fn discover_and_pump(&self, cfg: &Config) -> Result<String> {
        let app = Client::app_client(cfg)?;
        let installations =
            crate::github::paginate(&app, "/app/installations?per_page=100").await?;
        let mut repositories = BTreeMap::new();
        for installation in installations {
            let id = installation["id"]
                .as_i64()
                .ok_or("installation ID missing")?;
            let permissions = installation["permissions"]["actions"]
                .as_str()
                .unwrap_or("");
            let gh = Arc::new(Client::installation_resolved(cfg, id).await?);
            for repo in gh
                .get_all("/installation/repositories?per_page=100")
                .await?
            {
                let name = config::repository_name(
                    repo["full_name"]
                        .as_str()
                        .ok_or("repository name missing")?,
                )?;
                let default_branch = repo["default_branch"]
                    .as_str()
                    .ok_or("default branch missing")?
                    .to_string();
                repositories.insert(
                    name.clone(),
                    Repository {
                        name,
                        default_branch,
                        gh: Arc::clone(&gh),
                        actions_read: matches!(permissions, "read" | "write"),
                        actions_write: permissions == "write",
                    },
                );
            }
        }
        self.pump_repositories(&repositories, now()).await
    }

    async fn load_rules(&self, repo: &Repository, timestamp: i64) -> Result<Option<Rules>> {
        let content = match repo
            .gh
            .get_file_content(&repo.name, CONFIG_PATH, &repo.default_branch)
            .await
        {
            Ok(Some(content)) => content,
            Err(GhError::Api { status: 404, .. }) => {
                self.store.configure(&repo.name, "disabled", timestamp)?;
                return Ok(None);
            }
            Ok(None) => return Err("repository config must be a file".into()),
            Err(e) => return Err(e.into()),
        };
        let rules = config::parse(&content, &repo.name)?;
        self.store.configure(
            &repo.name,
            &hex::encode(Sha256::digest(content.as_bytes())),
            timestamp,
        )?;
        Ok(rules)
    }

    async fn pump_repositories(
        &self,
        repositories: &BTreeMap<String, Repository>,
        timestamp: i64,
    ) -> Result<String> {
        let mut configured = Vec::new();
        let mut observed = BTreeSet::new();
        let mut errors = 0;
        for repo in repositories.values() {
            match self.load_rules(repo, timestamp).await {
                Ok(Some(rules)) => {
                    for monitor in &rules.monitors {
                        observed.insert(monitor.repository.clone().unwrap());
                    }
                    configured.push((repo, rules));
                }
                Ok(None) => {}
                Err(e) => {
                    errors += 1;
                    self.store.activity(&repo.name, timestamp, None)?;
                    tracing::warn!("idle workflows {}: configuration: {e}", repo.name);
                }
            }
        }
        let mut available = BTreeSet::new();
        for name in observed {
            let Some(repo) = repositories.get(&name) else {
                errors += 1;
                tracing::warn!(
                    "idle workflows: monitored repository {name} is not accessible to this App"
                );
                continue;
            };
            if !repo.actions_read {
                errors += 1;
                self.store.activity(&name, timestamp, None)?;
                tracing::warn!(
                    "idle workflows {name}: Actions: read permission is required for monitoring"
                );
                continue;
            }
            match repo.gh.actions_activity_snapshot(&name).await {
                Ok(snapshot) => {
                    self.store.observe(
                        &name,
                        snapshot,
                        timestamp,
                        (self.poll_secs.saturating_mul(2).saturating_add(30).max(120)) as i64,
                    )?;
                    available.insert(name);
                }
                Err(e) => {
                    errors += 1;
                    self.store.activity(&name, timestamp, None)?;
                    tracing::warn!("idle workflows {name}: activity could not be read: {e}");
                }
            }
        }
        let mut tasks = 0;
        for (repo, rules) in configured {
            // A missing/inaccessible monitor blocks the whole configured scope.
            if rules
                .monitors
                .iter()
                .any(|m| !available.contains(m.repository.as_ref().unwrap()))
            {
                continue;
            }
            for task in &rules.tasks {
                tasks += 1;
                match self
                    .reconcile_task(repo, task, &rules, repositories, timestamp)
                    .await
                {
                    Ok(outcome) => tracing::debug!(
                        "idle workflows {} {}@{}: {outcome}",
                        repo.name,
                        task.workflow,
                        task.branch
                    ),
                    Err(e) => {
                        errors += 1;
                        // An API outage may hide activity; resume with a full
                        // observation window once the affected scope recovers.
                        self.store.activity(&repo.name, timestamp, None)?;
                        tracing::warn!(
                            "idle workflows {} {}@{}: {e}",
                            repo.name,
                            task.workflow,
                            task.branch
                        );
                    }
                }
            }
        }
        Ok(format!(
            "idle workflows: {tasks} tasks checked, {errors} errors"
        ))
    }

    async fn validate_task(&self, repo: &Repository, task: &Task) -> Result<Workflow> {
        if !repo.actions_write {
            return Err("Actions: write permission is required for dispatch and retries".into());
        }
        let workflow = repo
            .gh
            .actions_workflow(&repo.name, config::workflow_name(&task.workflow)?)
            .await?;
        if workflow.id <= 0
            || workflow.state != "active"
            || !workflow.path.starts_with(".github/workflows/")
        {
            return Err(
                "target workflow is missing, disabled, or is not a repository workflow".into(),
            );
        }
        // GitHub requires the dispatch workflow on the default branch as well
        // as a valid definition at the requested ref.
        let text = repo
            .gh
            .get_file_content(&repo.name, &workflow.path, &repo.default_branch)
            .await?
            .ok_or("workflow definition is not a file")?;
        config::validate_dispatch(&text, task)?;
        if task.branch != repo.default_branch {
            let text = repo
                .gh
                .get_file_content(&repo.name, &workflow.path, &task.branch)
                .await?
                .ok_or("target branch workflow definition is not a file")?;
            config::validate_dispatch(&text, task)?;
        }
        Ok(workflow)
    }

    async fn scope_busy(
        &self,
        rules: &Rules,
        repositories: &BTreeMap<String, Repository>,
    ) -> Result<bool> {
        for monitor in &rules.monitors {
            let repo = repositories
                .get(monitor.repository.as_ref().unwrap())
                .ok_or("monitor unavailable")?;
            let mut selected = BTreeSet::new();
            for name in &monitor.workflows {
                selected.insert(
                    repo.gh
                        .actions_workflow(&repo.name, config::workflow_name(name)?)
                        .await?
                        .id,
                );
            }
            // Include waiting for approval/runners/concurrency. Looking only at
            // check conclusions can hide a running check behind a failed one.
            for status in ["queued", "in_progress", "waiting", "pending", "requested"] {
                let runs = repo
                    .gh
                    .actions_runs(&repo.name, None, &[("status", status.into())])
                    .await?;
                if runs.iter().any(|r| {
                    r.active() && (selected.is_empty() || selected.contains(&r.workflow_id))
                }) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn branch_sha(repo: &Repository, branch: &str) -> Result<String> {
        let reference = repo.gh.get_branch_ref(&repo.name, branch).await?;
        Ok(reference["object"]["sha"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("branch SHA missing")?
            .into())
    }

    fn record_run(state: &mut TargetState, run: &WorkflowRun) {
        let build = state.builds.entry(run.head_sha.clone()).or_default();
        build.run_id = Some(run.id);
        build.attempts = build.attempts.max(run.run_attempt);
        if run.status == "completed" {
            match run.conclusion.as_deref() {
                Some("success") => build.terminal = Some("success".into()),
                Some("cancelled") if build.terminal.as_deref() != Some("success") => {
                    build.terminal = Some("cancelled".into())
                }
                _ => {}
            }
        }
    }

    async fn reconcile_pending(
        &self,
        repo: &Repository,
        task: &Task,
        workflow: i64,
        state: &mut TargetState,
        timestamp: i64,
    ) -> Result<bool> {
        let Some(pending) = state.pending.clone() else {
            return Ok(false);
        };
        let run = if let Some(id) = pending.run_id {
            match repo.gh.actions_run(&repo.name, id).await {
                Ok(run) => Some(run),
                Err(GhError::Api { status: 404, .. }) => None,
                Err(e) => return Err(e.into()),
            }
        } else {
            let since = chrono::DateTime::from_timestamp(pending.sent_at - 5, 0)
                .ok_or("invalid dispatch timestamp")?
                .to_rfc3339();
            repo.gh
                .actions_runs(
                    &repo.name,
                    Some(workflow),
                    &[
                        ("branch", task.branch.clone()),
                        ("created", format!(">={since}")),
                    ],
                )
                .await?
                .into_iter()
                .filter(|run| {
                    run.event == "workflow_dispatch"
                        && !pending.known_run_ids.contains(&run.id)
                        && run.created_at.timestamp() >= pending.sent_at - 5
                        && run
                            .actor
                            .as_ref()
                            .is_some_and(|a| normalize_login(&a.login) == repo.gh.app_slug)
                })
                .max_by_key(|run| run.id)
        };
        if let Some(run) = run {
            if run.workflow_id != workflow || run.head_branch.as_deref() != Some(&task.branch) {
                return Err("dispatch run does not match the configured workflow/branch".into());
            }
            if pending.previous_run_attempt == 0 || run.run_attempt > pending.previous_run_attempt {
                // ref may have moved between our final read and GitHub accepting
                // dispatch. Credit the SHA GitHub actually ran, never the guess.
                Self::record_run(state, &run);
                state.pending = None;
                return Ok(false);
            }
            // A manual cancellation in the confirmation window takes precedence
            // over retry recovery, even if GitHub has not incremented the attempt.
            if run.conclusion.as_deref() == Some("cancelled") {
                Self::record_run(state, &run);
                state.pending = None;
                return Ok(false);
            }
        }
        if timestamp.saturating_sub(pending.sent_at) < CONFIRMATION_GRACE_SECS {
            return Ok(true);
        }
        // We searched GitHub successfully and waited for visibility. Releasing
        // uncertainty only makes the normal idle/retry gates eligible again.
        state.pending = None;
        Ok(false)
    }

    async fn reconcile_task(
        &self,
        repo: &Repository,
        task: &Task,
        rules: &Rules,
        repositories: &BTreeMap<String, Repository>,
        timestamp: i64,
    ) -> Result<&'static str> {
        let workflow = self.validate_task(repo, task).await?;
        let key = Store::target_key(&repo.name, workflow.id, &task.branch);
        let mut state = self.store.target(&key)?;
        let waiting = self
            .reconcile_pending(repo, task, workflow.id, &mut state, timestamp)
            .await?;
        self.store.save_target(&key, &state)?;
        if waiting {
            return Ok("awaiting dispatch confirmation");
        }

        let sha = Self::branch_sha(repo, &task.branch).await?;
        self.store
            .observe_branch(&repo.name, &task.branch, &sha, timestamp)?;
        if state
            .builds
            .get(&sha)
            .is_some_and(|b| b.terminal.as_deref() == Some("success"))
        {
            return Ok("already succeeded");
        }
        let mut runs = repo
            .gh
            .actions_runs(
                &repo.name,
                Some(workflow.id),
                &[("branch", task.branch.clone()), ("head_sha", sha.clone())],
            )
            .await?;
        // A known run can disappear temporarily from list/search responses.
        if let Some(id) = state.builds.get(&sha).and_then(|b| b.run_id) {
            if !runs.iter().any(|r| r.id == id) {
                match repo.gh.actions_run(&repo.name, id).await {
                    Ok(run) => runs.push(run),
                    Err(GhError::Api { status: 404, .. }) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if runs.iter().any(|r| {
            r.workflow_id != workflow.id
                || r.head_sha != sha
                || r.head_branch.as_deref() != Some(&task.branch)
        }) {
            return Err("run history does not match the requested workflow/branch/SHA".into());
        }
        runs.sort_by_key(|run| run.id);
        runs.dedup_by_key(|run| run.id);
        let before = runs.clone();
        let active = runs.iter().any(WorkflowRun::active);
        // Respect cancellation even for a trigger that is not considered an
        // equivalent successful build (for example a repository's nightly cron).
        for run in runs
            .iter()
            .filter(|r| r.conclusion.as_deref() == Some("cancelled"))
        {
            Self::record_run(&mut state, run);
        }
        runs.retain(|r| task.run_events.contains(&r.event));
        runs.sort_by_key(|r| (r.updated_at, r.id));
        for run in &runs {
            Self::record_run(&mut state, run);
        }
        let build = state.builds.entry(sha.clone()).or_default();
        // Manual attempts and runs already present at enablement count too;
        // persisted counts remain a floor if runs are later deleted.
        build.attempts = build.attempts.max(
            runs.iter()
                .map(|r| r.run_attempt)
                .fold(0u32, u32::saturating_add),
        );
        let terminal = build.terminal.clone();
        self.store.save_target(&key, &state)?;
        if terminal.as_deref() == Some("success") {
            return Ok("already succeeded");
        }
        if active {
            return Ok("run already queued or running");
        }
        if terminal.as_deref() == Some("cancelled") {
            return Ok("cancelled; automatic retries suppressed");
        }
        if runs.last().is_some_and(|r| !r.retryable()) {
            return Ok("run needs manual attention");
        }
        let build = state.builds.get(&sha).unwrap();
        if build.attempts > task.max_retries {
            return Ok("attempt limit reached");
        }
        let retry_at = runs
            .last()
            .map(|r| r.updated_at.timestamp())
            .unwrap_or(0)
            .max(build.last_attempt_at);
        if retry_at > 0
            && timestamp.saturating_sub(retry_at) < i64::from(task.retry_interval_minutes) * 60
        {
            return Ok("waiting for retry interval");
        }
        let scope: Vec<_> = rules
            .monitors
            .iter()
            .map(|m| m.repository.clone().unwrap())
            .collect();
        if !self.store.idle(
            &scope,
            timestamp,
            self.boot,
            i64::from(rules.idle_minutes) * 60,
        )? {
            return Ok("waiting for development idle period");
        }
        if self.scope_busy(rules, repositories).await? {
            return Ok("waiting for monitored CI");
        }
        if Self::branch_sha(repo, &task.branch).await? != sha {
            self.store.activity(&repo.name, timestamp, None)?;
            return Ok("branch advanced; waiting for latest commit");
        }
        // A human can start/cancel/rerun the target while we query CI in other
        // repositories. Verify the target again immediately before reserving.
        let mut fresh = repo
            .gh
            .actions_runs(
                &repo.name,
                Some(workflow.id),
                &[("branch", task.branch.clone()), ("head_sha", sha.clone())],
            )
            .await?;
        if let Some(id) = state.builds.get(&sha).and_then(|b| b.run_id) {
            if !fresh.iter().any(|r| r.id == id) {
                match repo.gh.actions_run(&repo.name, id).await {
                    Ok(run) => fresh.push(run),
                    Err(GhError::Api { status: 404, .. }) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        fresh.sort_by_key(|run| run.id);
        fresh.dedup_by_key(|run| run.id);
        if fresh != before {
            return Ok("workflow state changed; reconcile again");
        }
        // Recheck activity after the network reads; a webhook can arrive while
        // we are querying Actions. Reserve synchronously before the next await.
        if !self.store.idle(
            &scope,
            timestamp,
            self.boot,
            i64::from(rules.idle_minutes) * 60,
        )? {
            return Ok("new development activity");
        }
        let previous_run = runs.last();
        let known_run_ids = state
            .builds
            .values()
            .filter_map(|b| b.run_id)
            .chain(runs.iter().map(|r| r.id))
            .collect();
        state.pending = Some(Pending {
            sha: sha.clone(),
            run_id: previous_run.map(|r| r.id),
            previous_run_attempt: previous_run.map(|r| r.run_attempt).unwrap_or(0),
            sent_at: timestamp,
            known_run_ids,
        });
        let build = state.builds.get_mut(&sha).unwrap();
        build.attempts += 1;
        build.last_attempt_at = timestamp;
        self.store.save_target(&key, &state)?;

        let result = if let Some(run) = previous_run {
            repo.gh
                .actions_rerun(&repo.name, run.id)
                .await
                .map(|_| Some(run.id))
        } else {
            repo.gh
                .actions_dispatch(&repo.name, workflow.id, &task.branch, &task.inputs)
                .await
        };
        match result {
            Ok(run_id) => {
                state.pending.as_mut().unwrap().run_id = run_id;
                self.store.save_target(&key, &state)?;
                tracing::info!(
                    "idle workflows {} {}@{}: {} requested for {sha}, run {run_id:?}",
                    repo.name,
                    task.workflow,
                    task.branch,
                    if previous_run.is_some() {
                        "retry"
                    } else {
                        "dispatch"
                    }
                );
                Ok("request sent; awaiting confirmation")
            }
            Err(e) => {
                if matches!(&e, GhError::Api { status, .. } if (400..500).contains(status) && *status != 408)
                {
                    // A definite rejection consumed no workflow attempt. Keep
                    // the request timestamp to avoid hammering a broken setup.
                    state.pending = None;
                    state.builds.get_mut(&sha).unwrap().attempts -= 1;
                    self.store.save_target(&key, &state)?;
                }
                Err(e.into())
            }
        }
    }
}

#[cfg(test)]
mod tests;
