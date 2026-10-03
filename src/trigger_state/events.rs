//! Opened-event plans are trusted repository policy, never synthetic comments.
use super::*;
use crate::config::cache::{ConfigState, RepositoryKey};
use crate::config::repository::{Event, EventAction, EventRule, PathRule, Paths, ReasonCode, Rule};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
struct Plan {
    rules: Vec<String>,
    action: EventAction,
    config_sha: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct PathPlan {
    rules: Vec<PathRule>,
    labels: Vec<String>,
    event: Event,
    head_sha: String,
    base_sha: String,
    config_sha: String,
}

impl Serialize for Event {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Self::PullRequestOpened => "pull_request.opened",
            Self::PullRequestSynchronize => "pull_request.synchronize",
            Self::IssueOpened => "issues.opened",
        })
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Event::parse(&value).ok_or_else(|| serde::de::Error::custom("invalid event"))
    }
}

/// A fetched invalid document/domain is a durable refusal, not an outage.
/// Freeze no subscription (or revoke an existing plan) so repairs cannot
/// backfill old threads. Only unavailable reads retain the inbox for retry.
fn event_policy<'a>(
    state: &'a ConfigState,
    repo: &str,
) -> Result<(Option<&'a str>, &'a [Rule<EventRule>])> {
    match state {
        ConfigState::Ready(snapshot) => {
            let rules = match &snapshot.config.events {
                Ok(rules) => rules.as_slice(),
                Err(problem) => {
                    tracing::warn!(repo, "opened policy refused: {problem}");
                    &[]
                }
            };
            Ok((Some(&snapshot.commit_sha), rules))
        }
        ConfigState::Unavailable { problem, .. } if problem.code == ReasonCode::InvalidDocument => {
            tracing::warn!(repo, "opened policy refused: {problem}");
            Ok((None, &[]))
        }
        ConfigState::Unavailable { problem, .. } => Err(problem.clone().into()),
    }
}

/// Load path policy while preserving the distinction between a verified empty
/// domain and a transient configuration outage.
fn path_policy<'a>(
    state: &'a ConfigState,
    repo: &str,
) -> Result<(Option<&'a str>, Option<&'a Paths>)> {
    match state {
        ConfigState::Ready(snapshot) => match &snapshot.config.paths {
            Ok(paths) => Ok((Some(&snapshot.commit_sha), Some(paths))),
            Err(problem) => {
                tracing::warn!(repo, "path policy refused: {problem}");
                Ok((Some(&snapshot.commit_sha), None))
            }
        },
        ConfigState::Unavailable { problem, .. } if problem.code == ReasonCode::InvalidDocument => {
            tracing::warn!(repo, "path policy refused: {problem}");
            Ok((None, None))
        }
        ConfigState::Unavailable { problem, .. } => Err(problem.clone().into()),
    }
}

/// Return the stable configuration spelling used in durable plan metadata.
fn event_name(event: Event) -> &'static str {
    match event {
        Event::PullRequestOpened => "pull_request.opened",
        Event::PullRequestSynchronize => "pull_request.synchronize",
        Event::IssueOpened => "issues.opened",
    }
}

/// Verify App provenance; a copied marker or a human with a similar login
/// cannot suppress an ordinary contribution. Older payloads can omit App ID.
async fn own_source(gh: &Client, cfg: &Config, ctx: &EventContext) -> Result<bool> {
    let app_id = cfg.app_id.parse::<i64>()?;
    if let Some(via) = ctx.via_app_id {
        return Ok(via == app_id);
    }
    if ctx.user_type.as_deref() != Some("Bot") {
        return Ok(false);
    }
    if gh.app_slug.is_empty() {
        return Err("cannot verify automatic event App identity".into());
    }
    let identity = gh
        .get(&format!(
            "/users/{}",
            crate::github::enc_seg(&format!("{}[bot]", gh.app_slug))
        ))
        .await?;
    let id = identity["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or("missing bot identity")?;
    if identity["type"] != "Bot" {
        return Err("App identity is not a bot".into());
    }
    Ok(ctx.user_id == Some(id))
}

/// Route durable PR/Issue creation and PR path-trigger work through one inbox.
pub(super) async fn process(
    runtime: &Runtime,
    gh: &Client,
    cfg: &Config,
    cache: &RepositoryConfigCache,
    ctx: &EventContext,
) -> Result<()> {
    if own_source(gh, cfg, ctx).await? {
        return Ok(());
    }
    let mut first_error = None;
    if matches!(
        (ctx.event.as_str(), ctx.action.as_str(), ctx.is_pr),
        ("issues", "opened", false) | ("pull_request", "opened", true)
    ) {
        if let Err(error) = process_opened(runtime, gh, cfg, cache, ctx).await {
            first_error = Some(error);
        }
    }
    if matches!(
        (ctx.event.as_str(), ctx.action.as_str(), ctx.is_pr),
        ("pull_request", "opened" | "synchronize", true)
    ) {
        if let Err(error) = process_paths(runtime, gh, cfg, cache, ctx).await {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Load trusted opened-event policy, atomically freeze a plan, and execute it.
async fn process_opened(
    runtime: &Runtime,
    gh: &Client,
    cfg: &Config,
    cache: &RepositoryConfigCache,
    ctx: &EventContext,
) -> Result<()> {
    let event = match (ctx.event.as_str(), ctx.action.as_str(), ctx.is_pr) {
        ("issues", "opened", false) => Event::IssueOpened,
        ("pull_request", "opened", true) => Event::PullRequestOpened,
        _ => return Ok(()),
    };
    let key = RepositoryKey {
        installation_id: ctx.installation_id,
        repository_id: ctx.repository_id,
    };
    let state = cache.load(gh, key, &ctx.repo).await;
    let (config_sha, rules) = event_policy(&state, &ctx.repo)?;
    let current: Vec<&EventRule> = rules
        .iter()
        .filter_map(|r| {
            let rule = match &r.value {
                Ok(rule) => rule,
                Err(problem) => {
                    tracing::warn!(rule=%r.id, "opened rule refused: {problem}");
                    return None;
                }
            };
            if let Err(problem) = rule.action.validate_deployment(cfg) {
                tracing::warn!(rule=%r.id, "opened rule refused: {problem}");
                return None;
            }
            (rule.event == event).then_some(rule)
        })
        .collect();
    let mut proposed: Vec<Plan> = Vec::new();
    for rule in &current {
        if let Some(plan) = proposed.iter_mut().find(|p| p.action == rule.action) {
            plan.rules.push(rule.id.clone());
        } else {
            proposed.push(Plan {
                rules: vec![rule.id.clone()],
                action: rule.action.clone(),
                config_sha: config_sha.unwrap_or_default().into(),
            });
        }
    }
    for plan in &mut proposed {
        plan.rules.sort();
    }
    let plan_key = json!(["opened-plan", ctx.repository_id, ctx.thread_id]).to_string();
    let plans: Vec<Plan> = serde_json::from_value(
        runtime
            .store
            .opened_plan(&plan_key, &serde_json::to_value(proposed)?)?,
    )?;
    let mut deferred = false;
    for plan in plans {
        // A preceding review may take longer than the cache TTL. Revalidate
        // before each action instead of lending it the old sibling's policy.
        let state = cache.load(gh, key, &ctx.repo).await;
        let (config_sha, rules) = event_policy(&state, &ctx.repo)?;
        // A merged action retains authority while at least one of its original
        // rules still grants exactly that action. Added rules never backfill.
        let compatible = rules.iter().filter_map(|r| r.value.as_ref().ok()).any(|r| {
            r.event == event
                && plan.rules.contains(&r.id)
                && plan.action == r.action
                && r.action.validate_deployment(cfg).is_ok()
        });
        if let Err(error) = execute(runtime, gh, cfg, ctx, config_sha, &plan, compatible).await {
            tracing::error!(repo=%ctx.repo, thread=ctx.number, rules=?plan.rules, "opened action deferred: {error}");
            deferred = true;
        }
    }
    if deferred {
        Err("opened actions require retry or operator reconciliation".into())
    } else {
        Ok(())
    }
}

/// Reconcile the stable rule operation before starting any new computation.
async fn execute(
    runtime: &Runtime,
    gh: &Client,
    cfg: &Config,
    ctx: &EventContext,
    config_sha: Option<&str>,
    plan: &Plan,
    compatible: bool,
) -> Result<()> {
    let store = &runtime.store;
    let key = super::super::opened_key(ctx.repository_id, ctx.thread_id, &plan.rules[0]);
    if let Some(old) = store.get(&key)? {
        if old.state == State::Running {
            return Err("opened action is owned by another worker".into());
        }
        let revoked = !compatible || old.state == State::Superseded;
        if revoked {
            // Persist the veto before any fallible network reconciliation.
            store.revoke_opened(&key, now())?;
        }
        for child in store.children(&key)? {
            if child.state == State::Unknown {
                reconcile(store, gh, cfg.app_id.parse()?, &child, now()).await?;
            }
        }
        if revoked {
            // Reconciliation may have proved a child safe to retry. A revoked
            // parent must discard that child even if the rule was restored.
            store.revoke_opened(&key, now())?;
        }
        let children = store.children(&key)?;
        if children
            .iter()
            .any(|c| matches!(c.state, State::Unknown | State::Running))
        {
            return Err(
                "automatic report publication uncertain; no recomputation or resend".into(),
            );
        }
        if revoked
            || matches!(
                old.state,
                State::Succeeded | State::Failed | State::Superseded
            )
        {
            return Ok(());
        }
        if old.state == State::Unknown {
            if !children.iter().any(|c| c.state == State::Pending)
                && children.iter().any(|c| {
                    c.state == State::Succeeded
                        && (c.spec.kind == "ensure_labels"
                            || c.spec.kind == "review"
                            || (c.spec.kind == "comment"
                                && c.spec.request["automatic_report"] == true))
                })
            {
                store.resolve_unknown(
                    &old,
                    "succeeded",
                    None,
                    "automatic result confirmed from child receipt",
                    now(),
                )?;
                return Ok(());
            }
            if old.sent && !matches!(plan.action, EventAction::AddLabels(_)) {
                return Err("automatic computation was interrupted; operator reconciliation required before retry".into());
            }
            store.resolve_unknown(
                &old,
                "not_sent",
                None,
                "no uncertain report; label ensure is idempotent",
                now(),
            )?;
        }
    }
    if !compatible {
        // Persist revocation even if the process died after freezing the plan
        // but before creating its operation. Restoring a rule is not a replay.
        let spec = OperationSpec {
            key,
            parent: None,
            kind: "opened".into(),
            context: ctx.clone(),
            config_sha: config_sha.map(str::to_owned),
            request: json!({"action":plan.action,"rules":plan.rules,"planned_config_sha":plan.config_sha}),
        };
        if let Some(claim) = store.claim(&spec, now())? {
            store.finish(
                &claim,
                State::Superseded,
                None,
                "opened rule removed, disabled or changed",
                now(),
            )?;
        }
        return Ok(());
    }

    let pr = if ctx.is_pr {
        Some(gh.get_pr(&ctx.repo, ctx.number).await?)
    } else {
        None
    };
    let mut context = ctx.clone();
    if let Some(pr) = &pr {
        for name in ["head", "base"] {
            let sha = pr[name]["sha"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("PR snapshot missing SHA")?;
            if name == "head" {
                context.head_sha = Some(sha.into());
            } else {
                context.base_sha = Some(sha.into());
            }
        }
    }
    let spec = OperationSpec {
        key: key.clone(),
        parent: None,
        kind: "opened".into(),
        context: context.clone(),
        config_sha: config_sha.map(str::to_owned),
        request: json!({"action":plan.action,"rules":plan.rules,"planned_config_sha":plan.config_sha}),
    };
    let Some(claim) = store.claim(&spec, now())? else {
        return Err("opened action already claimed or deferred".into());
    };
    // A never-started plan can use today's head. Once started, recovery above
    // only reconciles receipts and never launches a second AI computation.
    store.refresh_opened_snapshot(&claim, &spec, now())?;
    let frame = Arc::new(Frame {
        store: store.clone(),
        context,
        app_id: cfg.app_id.parse()?,
        parent: key.clone(),
        snapshot: Mutex::new((
            config_sha.map(str::to_owned),
            spec.context.head_sha.clone(),
            spec.context.base_sha.clone(),
        )),
        counts: Mutex::new(HashMap::new()),
        blocked: AtomicBool::new(false),
        pr,
    });
    store.mark_sent(&claim, now())?;
    let status = ACTIVE
        .scope(frame.clone(), async {
            let lang = if matches!(plan.action, EventAction::Review | EventAction::Codeql) {
                crate::lang::for_pr(gh, &ctx.repo, ctx.number, None).await
            } else {
                crate::lang::Lang::En
            };
            match &plan.action {
                EventAction::Review => {
                    crate::engines_subproc::run_review(
                        gh,
                        cfg,
                        &ctx.repo,
                        ctx.number,
                        ctx.installation_id,
                        lang,
                    )
                    .await
                }
                EventAction::Codeql => {
                    crate::codeql::run_codeql_report(gh, cfg, &ctx.repo, ctx.number, lang).await
                }
                EventAction::AddLabels(labels) => {
                    match add_existing_labels(gh, ctx, labels).await {
                        Ok(()) => "ok".into(),
                        Err(error) => format!("error: {error}"),
                    }
                }
            }
        })
        .await;
    let children = store.children(&key)?;
    if status == "already-running" && children.is_empty() {
        store.finish(
            &claim,
            State::Unknown,
            None,
            "shared PR review mutex busy; no engine started",
            now(),
        )?;
        let old = store.get(&key)?.ok_or("missing opened planner")?;
        store.resolve_unknown(
            &old,
            "not_sent",
            None,
            "shared PR review mutex refused before computation",
            now(),
        )?;
        return Err("waiting for the existing per-PR review".into());
    }
    let uncertain = frame.blocked.load(Ordering::SeqCst)
        || children
            .iter()
            .any(|c| matches!(c.state, State::Pending | State::Running | State::Unknown));
    let state = if uncertain {
        State::Unknown
    } else if status.contains("error") || status == "parse-failed" {
        State::Failed
    } else {
        State::Succeeded
    };
    store.finish(
        &claim,
        state,
        Some(&json!({"status":status})),
        &status,
        now(),
    )?;
    if uncertain {
        Err("automatic publication paused for reconciliation".into())
    } else {
        Ok(())
    }
}

/// A full repository label inventory avoids GitHub implicitly creating names.
async fn add_existing_labels(gh: &Client, ctx: &EventContext, labels: &[String]) -> Result<()> {
    let available = gh
        .get_all(&format!("/repos/{}/labels?per_page=100", ctx.repo))
        .await?;
    let names = available
        .iter()
        .map(|v| {
            v["name"]
                .as_str()
                .ok_or("malformed repository label inventory")
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let selected = labels
        .iter()
        .map(|wanted| {
            names
                .iter()
                .find(|name| name.to_lowercase() == wanted.to_lowercase())
                .map(|name| name.to_string())
                .ok_or("automatic label does not exist in repository")
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    gh.add_labels(&ctx.repo, ctx.number, &selected).await?;
    Ok(())
}

/// Match one complete PR snapshot and freeze the result before any label write.
/// A later configuration edit or redelivery cannot backfill a previously
/// ignored head; the next opened/synchronize event gets its own snapshot key.
async fn process_paths(
    runtime: &Runtime,
    gh: &Client,
    cfg: &Config,
    cache: &RepositoryConfigCache,
    ctx: &EventContext,
) -> Result<()> {
    let event = match ctx.action.as_str() {
        "opened" => Event::PullRequestOpened,
        "synchronize" => Event::PullRequestSynchronize,
        _ => return Ok(()),
    };
    let key = RepositoryKey {
        installation_id: ctx.installation_id,
        repository_id: ctx.repository_id,
    };
    let state = cache.load(gh, key, &ctx.repo).await;
    let (config_sha, paths) = path_policy(&state, &ctx.repo)?;
    let Some(paths) = paths else {
        return Ok(());
    };
    if paths.rules.is_empty() || !paths.events.contains(&event) {
        return Ok(());
    }
    let mut pr = gh.get_pr(&ctx.repo, ctx.number).await?;
    let files = {
        let mut snapshot = pr.clone();
        let mut last = None;
        for attempt in 0..=2 {
            match gh
                .list_pr_files_complete(&ctx.repo, ctx.number, &snapshot)
                .await
            {
                Ok(files) => {
                    pr = snapshot;
                    last = Some(files);
                    break;
                }
                Err(error)
                    if error.to_string().contains("PR changed while reading") && attempt < 2 =>
                {
                    snapshot = gh.get_pr(&ctx.repo, ctx.number).await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        last.ok_or("path trigger file list was not obtained")?
    };
    let head_sha = pr["head"]["sha"]
        .as_str()
        .filter(|sha| !sha.is_empty())
        .ok_or("PR snapshot missing head SHA")?;
    let base_sha = pr["base"]["sha"]
        .as_str()
        .filter(|sha| !sha.is_empty())
        .ok_or("PR snapshot missing base SHA")?;
    let plan_key = serde_json::json!([
        "path-plan",
        ctx.repository_id,
        ctx.thread_id,
        event_name(event),
        head_sha,
        base_sha
    ])
    .to_string();
    let proposed = {
        let usable: Vec<PathRule> = paths
            .rules
            .iter()
            .filter_map(|rule| {
                let value = rule.value.as_ref().ok()?;
                if let Err(problem) = value.validate_deployment(cfg) {
                    tracing::warn!(rule=%rule.id, "path rule refused: {problem}");
                    return None;
                }
                Some(value.clone())
            })
            .collect();
        let matched = crate::path_triggers::matched_rules(&usable, &files)
            .map_err(|problem| problem.to_string())?;
        let mut labels = BTreeMap::new();
        let mut matched_rules = Vec::new();
        for (id, rule_labels) in matched {
            if let Some(rule) = usable.iter().find(|rule| rule.id == id) {
                for label in rule_labels {
                    labels.entry(label.to_lowercase()).or_insert(label);
                }
                matched_rules.push(rule.clone());
            }
        }
        if matched_rules.is_empty() {
            Vec::new()
        } else {
            matched_rules.sort_by(|a, b| a.id.cmp(&b.id));
            vec![PathPlan {
                rules: matched_rules,
                labels: labels.into_values().collect(),
                event,
                head_sha: head_sha.into(),
                base_sha: base_sha.into(),
                config_sha: config_sha.unwrap_or_default().into(),
            }]
        }
    };
    let plans: Vec<PathPlan> = serde_json::from_value(
        runtime
            .store
            .opened_plan(&plan_key, &serde_json::to_value(proposed)?)?,
    )?;
    for plan in plans {
        let state = cache.load(gh, key, &ctx.repo).await;
        let (_, current) = path_policy(&state, &ctx.repo)?;
        let compatible = current.is_some_and(|paths| {
            paths.events.contains(&plan.event)
                && plan.rules.iter().all(|planned| {
                    paths.rules.iter().any(|current| {
                        current.value.as_ref().is_ok_and(|value| {
                            value == planned && value.validate_deployment(cfg).is_ok()
                        })
                    })
                })
        });
        execute_path(runtime, gh, cfg, ctx, &pr, &plan, compatible).await?;
    }
    Ok(())
}

/// Execute the one coalesced label action for a matched path snapshot.
async fn execute_path(
    runtime: &Runtime,
    gh: &Client,
    cfg: &Config,
    ctx: &EventContext,
    pr: &serde_json::Value,
    plan: &PathPlan,
    compatible: bool,
) -> Result<()> {
    let current = gh
        .get(&format!("/repos/{}/pulls/{}", ctx.repo, ctx.number))
        .await?;
    if current["head"]["sha"].as_str() != Some(plan.head_sha.as_str())
        || current["base"]["sha"].as_str() != Some(plan.base_sha.as_str())
    {
        return Err("PR changed before path label execution".into());
    }
    let parent = serde_json::json!([
        "path",
        ctx.repository_id,
        ctx.thread_id,
        event_name(plan.event),
        plan.head_sha,
        plan.base_sha,
        plan.rules
            .first()
            .map(|rule| rule.id.as_str())
            .unwrap_or("")
    ])
    .to_string();
    let store = &runtime.store;
    if let Some(old) = store.get(&parent)? {
        if old.state == State::Running {
            return Err("path action is owned by another worker".into());
        }
        if !compatible {
            store.revoke_opened(&parent, now())?;
        }
        for child in store.children(&parent)? {
            if child.state == State::Unknown {
                reconcile(store, gh, cfg.app_id.parse()?, &child, now()).await?;
            }
        }
        let children = store.children(&parent)?;
        if children
            .iter()
            .any(|child| matches!(child.state, State::Unknown | State::Running))
        {
            return Err(
                "path label publication uncertain; operator reconciliation required".into(),
            );
        }
        if !compatible
            || matches!(
                old.state,
                State::Succeeded | State::Failed | State::Superseded
            )
        {
            return Ok(());
        }
        if old.state == State::Unknown {
            if children
                .iter()
                .any(|child| child.state == State::Succeeded && child.spec.kind == "ensure_labels")
            {
                store.resolve_unknown(
                    &old,
                    "succeeded",
                    None,
                    "automatic label result confirmed from child receipt",
                    now(),
                )?;
                return Ok(());
            }
            store.resolve_unknown(
                &old,
                "not_sent",
                None,
                "no uncertain path label write",
                now(),
            )?;
        }
    }
    let mut context = ctx.clone();
    context.head_sha = Some(plan.head_sha.clone());
    context.base_sha = Some(plan.base_sha.clone());
    let spec = OperationSpec {
        key: parent.clone(),
        parent: None,
        kind: "path".into(),
        context: context.clone(),
        config_sha: (!plan.config_sha.is_empty()).then(|| plan.config_sha.clone()),
        request: serde_json::json!({
            "event": event_name(plan.event),
            "rules": plan.rules.iter().map(|rule| rule.id.clone()).collect::<Vec<_>>(),
            "labels": plan.labels,
            "head_sha": plan.head_sha,
            "base_sha": plan.base_sha,
        }),
    };
    if !compatible {
        if let Some(claim) = store.claim(&spec, now())? {
            store.finish(
                &claim,
                State::Superseded,
                None,
                "path rule removed, disabled or changed",
                now(),
            )?;
        }
        return Ok(());
    }
    let Some(claim) = store.claim(&spec, now())? else {
        return Ok(());
    };
    store.mark_sent(&claim, now())?;
    let frame = Arc::new(Frame {
        store: store.clone(),
        context,
        app_id: cfg.app_id.parse()?,
        parent: parent.clone(),
        snapshot: Mutex::new((
            (!plan.config_sha.is_empty()).then(|| plan.config_sha.clone()),
            Some(plan.head_sha.clone()),
            Some(plan.base_sha.clone()),
        )),
        counts: Mutex::new(HashMap::new()),
        blocked: AtomicBool::new(false),
        pr: Some(pr.clone()),
    });
    let status = ACTIVE
        .scope(frame.clone(), async {
            match add_present_labels(gh, ctx, &plan.labels).await {
                Ok(()) => "ok".into(),
                Err(error) => format!("error: {error}"),
            }
        })
        .await;
    let children = store.children(&parent)?;
    let uncertain = frame.blocked.load(Ordering::SeqCst)
        || children.iter().any(|child| {
            matches!(
                child.state,
                State::Pending | State::Running | State::Unknown
            )
        });
    let state = if uncertain {
        State::Unknown
    } else if status.contains("error") {
        State::Failed
    } else {
        State::Succeeded
    };
    store.finish(
        &claim,
        state,
        Some(&serde_json::json!({"status": status})),
        &status,
        now(),
    )?;
    if uncertain {
        Err("automatic path label publication paused for reconciliation".into())
    } else {
        Ok(())
    }
}

/// Path-trigger labels are best effort across matching rules: a removed or
/// misspelled label must not block valid labels from another rule.
async fn add_present_labels(gh: &Client, ctx: &EventContext, labels: &[String]) -> Result<()> {
    let available = gh
        .get_all(&format!("/repos/{}/labels?per_page=100", ctx.repo))
        .await?;
    let names = available
        .iter()
        .map(|value| {
            value["name"]
                .as_str()
                .ok_or("malformed repository label inventory")
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut selected: Vec<String> = Vec::new();
    for wanted in labels {
        if let Some(name) = names.iter().find(|name| name.eq_ignore_ascii_case(wanted)) {
            if !selected
                .iter()
                .any(|current| current.eq_ignore_ascii_case(name))
            {
                selected.push((*name).to_string());
            }
        } else {
            tracing::warn!(repo=%ctx.repo, label=%wanted, "path label does not exist; skipped");
        }
    }
    if !selected.is_empty() {
        gh.add_labels(&ctx.repo, ctx.number, &selected).await?;
    }
    Ok(())
}
