//! Verified webhook admission and restart-safe bridge processing.
use super::*;
use serde::{Deserialize, Serialize};

type Result<T> = crate::trigger_state::Result<T>;

#[derive(Serialize, Deserialize)]
enum Delivery {
    GitHub {
        repo: String,
        installation: i64,
        repository: i64,
        event: String,
        payload: Value,
    },
    Forgejo {
        repo: String,
        instance: String,
        event: Box<ForgejoEvent>,
    },
}

/// Keep only routing identity and comment text, never the raw webhook envelope.
/// Object metadata is read live by the worker after obtaining the bridge lock.
pub fn enqueue_github(
    store: &Store,
    cfg: &Config,
    event: &str,
    payload: &Value,
    delivery_id: Option<&str>,
) -> Result<bool> {
    if !cfg.forgejo_ready() {
        return Ok(false);
    }
    let item = github_item(event, payload);
    let comment = (event == "issue_comment")
        .then(|| github_comment(payload))
        .flatten();
    let (number, is_pr) = match (&item, &comment) {
        (Some(item), _) => (item.number, item.is_pr),
        (_, Some(comment)) => (comment.number, comment.is_pr),
        _ => return Ok(false),
    };
    let repo = payload["repository"]["full_name"]
        .as_str()
        .filter(|repo| canonical_repo(repo).is_some())
        .ok_or("invalid GitHub bridge repository")?;
    let positive = |value: &Value| {
        value
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or("missing GitHub bridge identity")
    };
    let repository = positive(&payload["repository"]["id"])?;
    let installation = positive(&payload["installation"]["id"])?;
    let clean = json!({
        "action":payload["action"],
        "issue":{"number":number,"pull_request":if is_pr {json!({})} else {Value::Null}},
        "pull_request":{"number":number},
        "sender":{"login":payload["sender"]["login"]},
        "comment": comment.map(|c| json!({"id":c.id,"body":crate::redact::scrub(&c.body),"user":{"login":c.author}})),
        // Distinct source edits need distinct inbox keys even after text pruning.
        "source_revision": item.map(|_| digest(&[&payload.to_string()])),
    });
    let lane = json!([
        "github",
        installation,
        repository,
        kind(is_pr).as_str(),
        number
    ])
    .to_string();
    enqueue(
        store,
        &lane,
        Delivery::GitHub {
            repo: repo.into(),
            installation,
            repository,
            event: event.into(),
            payload: clean,
        },
        delivery_id,
    )
}

pub fn enqueue_forgejo(
    store: &Store,
    cfg: &Config,
    event: &ForgejoEvent,
    delivery_id: Option<&str>,
) -> Result<bool> {
    if !cfg.forgejo_ready() {
        return Ok(false);
    }
    let (repo, number, is_pr) = match event {
        ForgejoEvent::Issue {
            repo,
            number,
            action,
            ..
        } if supported_action(action) => (repo, *number, false),
        ForgejoEvent::PullRequest {
            repo,
            number,
            action,
            ..
        } if supported_action(action) => (repo, *number, true),
        ForgejoEvent::IssueComment {
            repo,
            number,
            is_pr,
            comment_id: Some(id),
            ..
        } if *id > 0 => (repo, *number, *is_pr),
        _ => return Ok(false),
    };
    if canonical_repo(repo).is_none() || number <= 0 {
        return Err("invalid Forgejo bridge identity".into());
    }
    let repository_id = match event {
        ForgejoEvent::Issue { repository_id, .. }
        | ForgejoEvent::PullRequest { repository_id, .. }
        | ForgejoEvent::IssueComment { repository_id, .. } => *repository_id,
        _ => None,
    };
    if !repository_id.is_some_and(|id| id > 0) {
        return Err("missing Forgejo bridge repository identity".into());
    }
    let mut event = event.clone();
    match &mut event {
        ForgejoEvent::Issue { body, .. }
        | ForgejoEvent::PullRequest { body, .. }
        | ForgejoEvent::IssueComment { body, .. } => *body = crate::redact::scrub(body),
        _ => {}
    }
    let client = ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)?;
    let lane = json!([
        client.api_base.as_str(),
        repo.to_ascii_lowercase(),
        repository_id,
        kind(is_pr).as_str(),
        number
    ])
    .to_string();
    enqueue(
        store,
        &lane,
        Delivery::Forgejo {
            repo: repo.into(),
            instance: client.api_base.to_string(),
            event: Box::new(event),
        },
        delivery_id,
    )
}

fn enqueue(
    store: &Store,
    lane: &str,
    delivery: Delivery,
    delivery_id: Option<&str>,
) -> Result<bool> {
    let payload = serde_json::to_value(delivery)?;
    // Delivery IDs distinguish identical edits made within the timestamp precision.
    // Missing IDs retain payload deduplication for older webhook proxies.
    let key = digest(&[lane, &payload.to_string(), delivery_id.unwrap_or("")]);
    store.bridge_enqueue(&key, lane, &payload)
}

/// One worker drains both providers in admission order. Interrupted requests
/// resume through the mapping/reservation checks; failures remain inspectable.
pub async fn pump(cfg: &Config, store: &Store) -> Result<()> {
    static PUMP: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let Ok(_guard) = PUMP.get_or_init(|| tokio::sync::Mutex::new(())).try_lock() else {
        return Ok(());
    };
    for _ in 0..50 {
        let Some((key, payload)) = store.bridge_claim_delivery()? else {
            break;
        };
        let outcome = match serde_json::from_value::<Delivery>(payload) {
            Ok(Delivery::GitHub {
                repo,
                installation,
                repository,
                event,
                payload,
            }) => match Client::installation_resolved(cfg, installation).await {
                Ok(gh) => {
                    let identity = RepositoryKey {
                        installation_id: installation,
                        repository_id: repository,
                    };
                    if event == "issue_comment" {
                        comment_from_github(&gh, cfg, store, &repo, identity, &payload).await
                    } else {
                        from_github(&gh, cfg, store, &repo, identity, &event, &payload).await
                    }
                }
                Err(error) => github_error(error),
            },
            Ok(Delivery::Forgejo {
                repo,
                instance,
                event,
            }) => {
                if ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)
                    .is_ok_and(|client| client.api_base.as_str() == instance)
                {
                    if matches!(*event, ForgejoEvent::IssueComment { .. }) {
                        comment_from_forgejo(cfg, store, &repo, &event).await
                    } else {
                        from_forgejo(cfg, store, &repo, &event).await
                    }
                } else {
                    BridgeOutcome::InvalidPayload("Forgejo instance changed since admission".into())
                }
            }
            Err(_) => BridgeOutcome::InvalidPayload("invalid persisted bridge delivery".into()),
        };
        let done = outcome.is_skip() || outcome == BridgeOutcome::Synced;
        store.bridge_finish_delivery(&key, done, &outcome.to_string())?;
        if done {
            tracing::debug!(key, "bridge: {outcome}");
        } else {
            tracing::warn!(key, "bridge paused: {outcome}");
        }
    }
    Ok(())
}
