//! Explicit path CC uses the label planner's complete, frozen diff. The only
//! mentions are recipients reserved by the PR-lifetime transaction.
use super::*;
use std::collections::BTreeSet;

/// A rule with CC must be valid in its entirety before either of its effects.
/// Label-only rules retain the existing best-effort label behavior.
pub(super) async fn valid_rules(
    gh: &Client,
    ctx: &EventContext,
    rules: &[PathRule],
) -> Result<Vec<PathRule>> {
    if !rules
        .iter()
        .any(|r| !r.cc.is_empty() && !r.labels.is_empty())
    {
        return Ok(rules.to_vec());
    }
    let inventory = gh
        .get_all(&format!("/repos/{}/labels?per_page=100", ctx.repo))
        .await?;
    let names = inventory
        .iter()
        .map(|label| {
            label["name"]
                .as_str()
                .map(str::to_lowercase)
                .ok_or("malformed repository label inventory")
        })
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    Ok(rules.iter().filter(|rule| {
        let valid = rule.cc.is_empty() || rule.labels.iter().all(|label| names.contains(&label.to_lowercase()));
        if !valid { tracing::warn!(repo=%ctx.repo, rule=%rule.id, "path rule skipped: configured label does not exist"); }
        valid
    }).cloned().collect())
}

/// Bound display size and neutralize ALL mentions, HTML, Markdown and control
/// characters from paths, rule IDs and metadata. Only validated logins use @.
fn display(value: &str) -> String {
    let mut text = String::new();
    for ch in value.chars().take(80) {
        match ch {
            '@' => text.push('＠'),
            '&' => text.push_str("&amp;"),
            '<' => text.push_str("&lt;"),
            '>' => text.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '#' | '!' | '|' | '~' => {
                text.push('\\');
                text.push(ch);
            }
            ch if ch.is_control() => text.push(' '),
            ch => text.push(ch),
        }
    }
    if value.chars().count() > 80 {
        text.push('…');
    }
    text
}

/// Render only reserved mentions, escaped evidence, and a stable recovery marker.
fn render(plan: &PathPlan, claim: &super::super::super::Claim) -> String {
    let mentions = claim
        .operation
        .recipients
        .iter()
        .map(|login| format!("@{login}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut body = format!("Configured path notification\n\ncc {mentions}\n\nChecked head: {}\n\nMatched rules (path examples):\n",display(&plan.head_sha));
    for matched in &plan.matches {
        body.push_str(&format!(
            "- {}: {}\n",
            display(&matched.id),
            matched
                .paths
                .iter()
                .map(|p| display(p))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let suppressed = claim.operation.spec.request["suppressed"]
        .as_array()
        .map_or(0, Vec::len);
    if suppressed > 0 {
        body.push_str(&format!(
            "\n{suppressed} additional recipient(s) suppressed by the PR lifetime limit.\n"
        ));
    }
    body.push_str(&format!(
        "\n{}",
        operation_marker(&claim.operation.spec.key)
    ));
    body
}

/// Distinguish a permanent recipient refusal from an outdated snapshot.
enum Preflight {
    Ready,
    Superseded,
    InvalidRecipient { login: String, reason: String },
}

/// A missing/non-personal/renamed account is a definite refusal. API outages
/// and malformed identity responses remain retryable without sending a comment.
async fn recipient_refusal(gh: &Client, login: &str) -> Result<Option<String>> {
    let user = match gh.get(&format!("/users/{login}")).await {
        Ok(user) => user,
        Err(GhError::Api { status: 404, .. }) => return Ok(Some("account does not exist".into())),
        Err(error) => return Err(error.into()),
    };
    let kind = user["type"].as_str().filter(|s| !s.is_empty());
    let actual_login = user["login"].as_str().filter(|s| !s.is_empty());
    let (Some(kind), Some(actual_login), Some(_)) =
        (kind, actual_login, user["id"].as_i64().filter(|id| *id > 0))
    else {
        return Err(
            format!("incomplete GitHub identity response for path CC recipient {login}").into(),
        );
    };
    if kind != "User" {
        return Ok(Some(format!("account type {kind} is not a personal User")));
    }
    if !actual_login.eq_ignore_ascii_case(login) {
        return Ok(Some(
            "account login no longer matches the configured login".into(),
        ));
    }
    Ok(None)
}

/// Reconcile or claim one snapshot's CC, then preflight and checkpoint its send.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    runtime: &Runtime,
    gh: &Client,
    cfg: &Config,
    cache: &RepositoryConfigCache,
    ctx: &EventContext,
    plan: &PathPlan,
    compatible: bool,
) -> Result<()> {
    if plan.rules.iter().all(|rule| rule.cc.is_empty()) {
        return Ok(());
    }
    let store = &runtime.store;
    // One aggregate comment per snapshot, regardless of source event/rule IDs.
    let key = json!([
        "path-cc",
        ctx.repository_id,
        ctx.thread_id,
        plan.head_sha,
        plan.base_sha
    ])
    .to_string();
    if let Some(old) = store.get(&key)? {
        if old.state == State::Unknown
            && reconcile(store, gh, cfg.app_id.parse()?, &old, now()).await? == Recovery::Paused
        {
            return Err(
                "path notification unknown; slots retained, operator reconciliation required"
                    .into(),
            );
        }
        let old = store.get(&key)?.ok_or("missing notification")?;
        if matches!(
            old.state,
            State::Succeeded | State::Failed | State::Superseded
        ) {
            return Ok(());
        }
        if old.state == State::Running {
            return Err("path notification owned by another worker".into());
        }
    }
    let repo_key = RepositoryKey {
        installation_id: ctx.installation_id,
        repository_id: ctx.repository_id,
    };
    let state = cache.load(gh, repo_key, &ctx.repo).await;
    let (_, paths) = path_policy(&state, &ctx.repo)?;
    let max = paths.map_or(0, |p| p.max_cc_users_per_pr);
    let allowed = compatible && path_plan_compatible(paths, cfg, plan) && max > 0;
    let mut context = ctx.clone();
    context.head_sha = Some(plan.head_sha.clone());
    context.base_sha = Some(plan.base_sha.clone());
    let spec = OperationSpec {
        key: key.clone(),
        parent: None,
        kind: "comment".into(),
        context,
        config_sha: Some(plan.config_sha.clone()),
        request: json!({"method":"POST",
        "route":format!("/repos/{}/issues/{}/comments",ctx.repo,ctx.number),"path_notification":true}),
    };
    if !allowed {
        if let Some(claim) = store.claim(&spec, now())? {
            store.finish(
                &claim,
                State::Superseded,
                None,
                "path CC rule revoked or disabled",
                now(),
            )?;
        }
        return Ok(());
    }
    let pr = gh.get_pr(&ctx.repo, ctx.number).await?;
    if path_snapshot(&pr)? != (plan.head_sha.as_str(), plan.base_sha.as_str()) {
        if let Some(claim) = store.claim(&spec, now())? {
            store.finish(
                &claim,
                State::Superseded,
                None,
                "path CC snapshot changed before reservation",
                now(),
            )?;
        }
        return Err("path CC snapshot superseded".into());
    }
    let created = chrono::DateTime::parse_from_rfc3339(
        pr["created_at"]
            .as_str()
            .ok_or("PR missing creation time; cannot verify notification ledger")?,
    )?
    .timestamp();
    if !store.notification_ledger_ready(ctx.repository_id, ctx.thread_id, created)? {
        return Err("path CC blocked: old PR notification ledger is unverified; restore persistent state or use trigger-state restore-notification-ledger with complete evidence".into());
    }
    let users: Vec<String> = plan
        .rules
        .iter()
        .flat_map(|r| &r.cc)
        .filter(|u| !u.eq_ignore_ascii_case(&gh.app_slug))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let Some(claim) = store.claim_notification(&spec, &users, max, now())? else {
        return Ok(());
    };
    if claim.operation.recipients.is_empty() {
        let suppressed = &claim.operation.spec.request["suppressed"];
        tracing::info!(repo=%ctx.repo,pr=ctx.number,?suppressed,"path CC has no new recipients; no comment");
        store.finish(
            &claim,
            State::Succeeded,
            Some(&json!({"posted":false,"suppressed":suppressed})),
            "no new recipients",
            now(),
        )?;
        return Ok(());
    }
    // Every preflight refusal is unsent. Permanent recipient errors end this
    // snapshot; only transient failures release slots into a retryable plan.
    let preflight: Result<Preflight> = async {
        for login in &claim.operation.recipients {
            if let Some(reason) = recipient_refusal(gh, login).await? {
                return Ok(Preflight::InvalidRecipient {
                    login: login.clone(),
                    reason,
                });
            }
        }
        if valid_rules(gh, ctx, &plan.rules).await?.len() != plan.rules.len() {
            return Ok(Preflight::Superseded);
        }
        let current = gh.get_pr(&ctx.repo, ctx.number).await?;
        let state = cache.load(gh, repo_key, &ctx.repo).await;
        let (_, paths) = path_policy(&state, &ctx.repo)?;
        Ok(
            if path_snapshot(&current)? == (plan.head_sha.as_str(), plan.base_sha.as_str())
                && path_plan_compatible(paths, cfg, plan)
                && paths.is_some_and(|p| p.max_cc_users_per_pr >= max)
            {
                Preflight::Ready
            } else {
                Preflight::Superseded
            },
        )
    }
    .await;
    match preflight {
        Ok(Preflight::Ready) => {}
        Ok(Preflight::InvalidRecipient { login, reason }) => {
            let rules: Vec<_> = plan
                .rules
                .iter()
                .filter(|rule| rule.cc.contains(&login))
                .map(|rule| rule.id.as_str())
                .collect();
            let detail = format!("path CC recipient {login} refused: {reason}");
            store.finish(
                &claim,
                State::Failed,
                Some(&json!({"invalid_recipient":login,"rules":rules,"reason":reason})),
                &detail,
                now(),
            )?;
            tracing::error!(repo=%ctx.repo, pr=ctx.number, %login, ?rules, %reason,
                "path CC snapshot failed permanently; correct the configured recipient");
            return Ok(());
        }
        Ok(Preflight::Superseded) => {
            store.finish(
                &claim,
                State::Superseded,
                None,
                "head, rules or budget changed before send",
                now(),
            )?;
            return Err("path notification superseded before send".into());
        }
        Err(error) => {
            store.finish(
                &claim,
                State::Unknown,
                None,
                &format!("preflight failed, request not sent: {error}"),
                now(),
            )?;
            let op = store.get(&key)?.ok_or("missing notification")?;
            store.resolve_unknown(
                &op,
                "not_sent",
                None,
                &format!("preflight ended before send: {error}"),
                now(),
            )?;
            return Err(error);
        }
    }
    let body = crate::redact::scrub(&render(plan, &claim));
    store.prepare_notification(&claim, &body, now())?;
    store.mark_sent(&claim, now())?;
    let result = gh
        .raw_write(
            "POST",
            spec.request["route"]
                .as_str()
                .ok_or("missing notification route")?,
            Some(json!({"body":body})),
        )
        .await;
    match result {
        Ok(value) => {
            store.finish(
                &claim,
                State::Succeeded,
                Some(&receipt(&value)),
                "GitHub accepted mention comment; delivery depends on recipient settings",
                now(),
            )?;
            Ok(())
        }
        Err(error) => {
            let state = if matches!(
                &error,
                GhError::Api {
                    status: 400 | 401 | 403 | 404 | 409 | 410 | 422,
                    ..
                }
            ) {
                State::Failed
            } else {
                State::Unknown
            };
            store.finish(&claim, state, None, &error.to_string(), now())?;
            Err(error.into())
        }
    }
}
