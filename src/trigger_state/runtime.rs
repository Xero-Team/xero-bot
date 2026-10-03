use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{EventContext, Operation, OperationSpec, Result, SessionWake, State, Store};
use crate::commands::Command;
use crate::config::cache::RepositoryConfigCache;
use crate::config::Config;
use crate::github::{Client, GhError};

#[path = "events.rs"]
mod events;

/// Use epoch seconds for restart-safe scheduling and audit timestamps.
fn now() -> i64 {
    crate::github::chrono_now_secs()
}
/// Represent local persistence failures as a GitHub-boundary refusal.
fn error(message: impl ToString) -> GhError {
    GhError::BadShape(message.to_string())
}

/// Derive a stable hidden marker from the business operation key.
pub fn operation_marker(key: &str) -> String {
    format!(
        "<!-- xero-trigger:{} -->",
        hex::encode(Sha256::digest(key.as_bytes()))
    )
}

#[derive(Debug, PartialEq, Eq)]
pub enum Recovery {
    Confirmed,
    RetrySafe,
    Paused,
}

/// Never infer absence from a missing marker. Verify the App identity, not
/// merely a copied body or a similar human login. Failed/incomplete GETs pause.
pub async fn reconcile(
    store: &Store,
    gh: &Client,
    app_id: i64,
    operation: &Operation,
    at: i64,
) -> Result<Recovery> {
    if operation.state != State::Unknown {
        return Err("reconciliation needs unknown state".into());
    }
    let spec = &operation.spec;
    if !operation.sent {
        store.resolve_unknown(
            operation,
            "not_sent",
            None,
            "request was never marked for sending",
            at,
        )?;
        return Ok(Recovery::RetrySafe);
    }
    let request = &spec.request;
    if matches!(spec.kind.as_str(), "comment" | "review") {
        let route = request["route"]
            .as_str()
            .ok_or("missing reconciliation route")?;
        let objects = gh.get_all(&format!("{route}?per_page=100")).await?;
        let marker = operation_marker(&spec.key);
        if let Some(found) = objects.iter().find(|object| {
            object["performed_via_github_app"]["id"].as_i64() == Some(app_id)
                && object["user"]["type"] == "Bot"
                && object["user"]["login"].as_str().is_some_and(|login| {
                    login.eq_ignore_ascii_case(&format!("{}[bot]", gh.app_slug))
                })
                && object["body"]
                    .as_str()
                    .is_some_and(|body| body.lines().any(|line| line == marker))
                && object["id"].as_i64().is_some_and(|id| id > 0)
        }) {
            store.resolve_unknown(
                operation,
                "succeeded",
                Some(&receipt(found)),
                "verified operation marker and App author",
                at,
            )?;
            return Ok(Recovery::Confirmed);
        }
    } else if spec.kind == "ensure_labels" {
        let labels = gh
            .get_all(&format!(
                "{}?per_page=100",
                request["route"].as_str().ok_or("missing labels route")?
            ))
            .await?;
        let expected = request["body"]["labels"]
            .as_array()
            .ok_or("missing label set")?;
        if !labels.iter().all(|label| label["name"].as_str().is_some()) {
            return Err("malformed label list".into());
        }
        let present = expected.iter().all(|name| {
            name.as_str().is_some_and(|name| {
                labels.iter().any(|label| {
                    label["name"]
                        .as_str()
                        .is_some_and(|current| current.eq_ignore_ascii_case(name))
                })
            })
        });
        store.resolve_unknown(
            operation,
            if present {
                "succeeded"
            } else {
                "retry_idempotent"
            },
            None,
            if present {
                "current labels confirm requested set"
            } else {
                "current labels checked; idempotent ensure may retry after preflight"
            },
            at,
        )?;
        return Ok(if present {
            Recovery::Confirmed
        } else {
            Recovery::RetrySafe
        });
    }
    tracing::error!(operation = %spec.key, delivery = %spec.context.delivery, "unknown GitHub write: automatic resend paused; inspect with trigger-state");
    Ok(Recovery::Paused)
}

/// Store only fields callers need to resume, never authentication material or
/// an arbitrary API response. The original request is already scrubbed.
pub(super) fn receipt(value: &Value) -> Value {
    let mut result = json!({});
    for key in ["id", "number", "sha", "merged"] {
        if let Some(v) = value.get(key) {
            result[key] = v.clone();
        }
    }
    if let Some(users) = value["assignees"].as_array() {
        result["assignees"] = json!(users
            .iter()
            .map(|u| json!({"login":u["login"]}))
            .collect::<Vec<_>>());
    }
    result
}

struct Frame {
    store: Arc<Store>,
    context: EventContext,
    app_id: i64,
    parent: String,
    snapshot: Mutex<(Option<String>, Option<String>, Option<String>)>,
    counts: Mutex<HashMap<(String, String), usize>>,
    blocked: AtomicBool,
    /// Present only for automatic PR actions; every engine sees one snapshot.
    pr: Option<Value>,
}
tokio::task_local! { static ACTIVE: Arc<Frame>; }

/// Report whether this task is inside a durable trigger execution scope.
pub(crate) fn active() -> bool {
    ACTIVE.try_with(|_| ()).is_ok()
}

/// Keep automatic publication constraints separate from comment command policy.
pub(crate) fn automatic() -> bool {
    ACTIVE
        .try_with(|f| f.context.event != "issue_comment")
        .unwrap_or(false)
}

/// Return only the PR snapshot owned by this automatic operation's scope.
pub(crate) fn automatic_pr(repo: &str, number: i64) -> Option<Value> {
    ACTIVE
        .try_with(|f| {
            (f.context.repo == repo && f.context.number == number)
                .then(|| f.pr.clone())
                .flatten()
        })
        .ok()
        .flatten()
}

/// Query durable session evidence for the current webhook delivery. A caller
/// outside the durable ingress has no session authority and must not fall back
/// to a history scan.
pub(crate) fn session_before(call: &SessionWake, ttl_days: u16) -> Result<Option<SessionWake>> {
    ACTIVE.try_with(|frame| {
        frame
            .store
            .session_before_at(call, ttl_days, chrono::Utc::now().timestamp_millis())
    })?
}

/// Record a wake only inside the durable ingress scope. The caller has already
/// checked syntax, applicability and any command-specific authorization.
pub(crate) fn record_wake(wake: &SessionWake) -> Result<()> {
    ACTIVE.try_with(|frame| frame.store.record_wake(wake))?
}

/// Mark an existing unsent command as refused before candidate resolution.
/// First-time refusals have no operation row and are intentionally a no-op.
pub(crate) fn reject_command(command: &Command, detail: &str) -> Result<()> {
    ACTIVE.try_with(|frame| {
        let comment = frame
            .context
            .comment_id
            .ok_or("missing source comment for command refusal")?;
        let key = super::manual_key(frame.context.repository_id, comment, command);
        frame.store.fail_unsent(&key, detail, now())
    })?
}
/// Capture the configuration and PR snapshot used by subsequent command claims.
pub(crate) fn snapshot(config: &str, head: Option<&str>, base: Option<&str>) -> Result<()> {
    if let Ok(frame) = ACTIVE.try_with(Arc::clone) {
        *frame
            .snapshot
            .lock()
            .map_err(|_| "snapshot mutex poisoned")? = (
            Some(config.into()),
            head.map(str::to_owned),
            base.map(str::to_owned),
        );
    }
    Ok(())
}

pub struct Runtime {
    pub store: Arc<Store>,
    pump: tokio::sync::Mutex<()>,
}
impl Runtime {
    /// Acquire exclusive state ownership before starting any trigger worker.
    pub fn open(data_dir: &std::path::Path) -> Result<Self> {
        Ok(Self {
            store: Arc::new(Store::open(data_dir)?),
            pump: tokio::sync::Mutex::new(()),
        })
    }

    /// Run up to eight deliveries concurrently, continuously refilling free
    /// slots so a slow review does not delay commands arriving after it.
    pub async fn pump(&self, cfg: &Config) -> Result<usize> {
        self.pump_with(
            |context| async move {
                let gh = Client::installation_resolved(cfg, context.installation_id).await?;
                self.process(&gh, cfg, &RepositoryConfigCache::shared(), &context)
                    .await
            },
            std::time::Duration::from_secs(900),
            std::time::Duration::from_secs(1),
        )
        .await
    }

    /// Production scheduling with an injectable processor/clock duration for
    /// concurrency tests. Local storage errors stop admission and drain owned
    /// futures before returning the first error; external cancellation drops them.
    pub(super) async fn pump_with<F, Fut>(
        &self,
        run: F,
        timeout: std::time::Duration,
        poll: std::time::Duration,
    ) -> Result<usize>
    where
        F: Fn(EventContext) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let Ok(_guard) = self.pump.try_lock() else {
            return Ok(0);
        };
        // No workers from this pump can be active while the guard is available.
        self.store.recover_running(i64::MAX, now())?;
        self.store.resume_inbox(now())?;
        self.store.cleanup_inbox(now())?;
        let mut cleanup_at = tokio::time::Instant::now();
        let mut workers = FuturesUnordered::new();
        let mut count = 0;
        let mut failure = None;
        loop {
            while failure.is_none() && workers.len() < 8 {
                let item = match self.store.claim_inbox(now()) {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(error) => {
                        tracing::error!("trigger inbox claim failed; draining workers: {error}");
                        failure.get_or_insert(error);
                        break;
                    }
                };
                let work = run(item.context.clone());
                workers.push(self.process_item(item, work, timeout));
            }
            if workers.is_empty() {
                return failure.map_or(Ok(count), Err);
            }
            // Never use `?` while workers are owned here: a local bookkeeping
            // error must not cancel another delivery's in-flight external write.
            // Existing per-item timeouts bound this drain, even for hung work.
            tokio::select! {
                result = workers.next() => {
                    if let Err(error) = result.expect("worker set is nonempty") {
                        tracing::error!("trigger bookkeeping failed; draining workers: {error}");
                        failure.get_or_insert(error);
                    }
                    count += 1;
                }
                _ = tokio::time::sleep(poll), if failure.is_none() => {}
            }
            if failure.is_none() && cleanup_at.elapsed() >= std::time::Duration::from_secs(60) {
                if let Err(error) = self.store.cleanup_inbox(now()) {
                    tracing::error!("trigger inbox cleanup failed; draining workers: {error}");
                    failure.get_or_insert(error);
                }
                cleanup_at = tokio::time::Instant::now();
            }
        }
    }

    /// Timeout/panic handling is local to this delivery, including writes first
    /// created by a different delivery and reclaimed by this worker.
    async fn process_item<F: Future<Output = Result<()>>>(
        &self,
        item: super::InboxItem,
        work: F,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let result =
            tokio::time::timeout(timeout, std::panic::AssertUnwindSafe(work).catch_unwind()).await;
        let mut problem = match result {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(e))) => Some(crate::redact::scrub(&e.to_string())),
            Err(_) | Ok(Err(_)) => {
                Some("trigger worker interrupted; writes require reconciliation".into())
            }
        };
        // All local request futures have ended. A missing receipt is unknown,
        // even after an otherwise normal return, and only its owner is fenced.
        if self.store.recover_delivery(&item.context.delivery, now())? > 0 {
            problem.get_or_insert_with(|| "trigger writes awaiting reconciliation".into());
        }
        if let Some(problem) = &problem {
            tracing::warn!(delivery=%item.context.delivery,"trigger deferred: {problem}");
        }
        self.store.finish_inbox(&item, problem.as_deref(), now())?;
        Ok(())
    }

    /// Injectable client/cache for acceptance tests. This is also the consumer
    /// seam for #15–#17: no generic command-string automatic executor exists.
    pub(super) async fn process(
        &self,
        gh: &Client,
        cfg: &Config,
        cache: &RepositoryConfigCache,
        context: &EventContext,
    ) -> Result<()> {
        if context.event != "issue_comment" {
            return events::process(self, gh, cfg, cache, context).await;
        }
        let frame = Arc::new(Frame {
            store: self.store.clone(),
            context: context.clone(),
            app_id: cfg.app_id.parse()?,
            parent: json!(["diagnostic", context.repository_id, context.comment_id]).to_string(),
            snapshot: Mutex::new((None, context.head_sha.clone(), context.base_sha.clone())),
            counts: Mutex::new(HashMap::new()),
            blocked: AtomicBool::new(false),
            pr: None,
        });
        ACTIVE
            .scope(Arc::clone(&frame), async {
                if let crate::dispatch::Routing::Act(work @ crate::dispatch::Work::Comment { .. }) =
                    crate::dispatch::route_event(cfg, "issue_comment", &context.comment_payload())
                {
                    crate::dispatch::execute_comment_with_client(gh, cfg, cache, work)
                        .await
                        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
                }
                if frame.blocked.load(Ordering::SeqCst) {
                    return Err("trigger write/claim failed; keep inbox for recovery".into());
                }
                let unresolved = self.store.unresolved(&context.delivery)?;
                if unresolved {
                    return Err(
                        "operations awaiting recovery; unknown non-idempotent writes are paused"
                            .into(),
                    );
                }
                Ok(())
            })
            .await
    }
}

/// One command is a planner with independent child write receipts. Its key
/// ignores source spelling and delivery ID. A successful sibling never reruns.
pub(crate) async fn command<F: Future<Output = String>>(
    gh: &Client,
    command: &Command,
    future: F,
) -> String {
    let Ok(outer) = ACTIVE.try_with(Arc::clone) else {
        return future.await;
    };
    match run_command(gh, Arc::clone(&outer), command, future).await {
        Ok(result) => result,
        Err(e) => {
            outer.blocked.store(true, Ordering::SeqCst);
            tracing::error!("durable command paused: {e}");
            format!("state-error: {e}")
        }
    }
}

/// Recover and claim one canonical command, then checkpoint its independent writes.
async fn run_command<F: Future<Output = String>>(
    gh: &Client,
    outer: Arc<Frame>,
    command: &Command,
    future: F,
) -> Result<String> {
    let key = super::manual_key(
        outer.context.repository_id,
        outer.context.comment_id.ok_or("missing source comment")?,
        command,
    );
    let (config_sha, head, base) = outer
        .snapshot
        .lock()
        .map_err(|_| "snapshot mutex poisoned")?
        .clone();
    let mut context = outer.context.clone();
    context.head_sha = head;
    context.base_sha = base;
    let spec = OperationSpec {
        key: key.clone(),
        parent: None,
        kind: "command".into(),
        context: context.clone(),
        config_sha: config_sha.clone(),
        request: json!({"command":command.id().name()}),
    };
    if let Some(old) = outer.store.get(&key)? {
        if old.state == State::Unknown {
            for child in outer.store.children(&key)? {
                if child.state == State::Unknown {
                    reconcile(&outer.store, gh, outer.app_id, &child, now()).await?;
                }
            }
            if outer
                .store
                .children(&key)?
                .iter()
                .any(|c| matches!(c.state, State::Unknown | State::Running))
            {
                return Ok("unknown: operator attention required".into());
            }
            outer.store.resolve_unknown(
                &old,
                "not_sent",
                None,
                "command planner has no uncertain children",
                now(),
            )?;
        }
    }
    let Some(claim) = outer.store.claim(&spec, now())? else {
        return Ok("already recorded or awaiting recovery".into());
    };
    let previous = &claim.operation.spec.context;
    if previous.head_sha.is_some()
        && (previous.head_sha != context.head_sha || previous.base_sha != context.base_sha)
    {
        outer.store.finish(
            &claim,
            State::Superseded,
            None,
            "PR head/base changed before retry",
            now(),
        )?;
        outer.store.supersede_unsent_children(&key, now())?;
        return Ok("superseded".into());
    }
    let frame = Arc::new(Frame {
        store: outer.store.clone(),
        context,
        app_id: outer.app_id,
        parent: key.clone(),
        snapshot: Mutex::new((config_sha, spec.context.head_sha, spec.context.base_sha)),
        counts: Mutex::new(HashMap::new()),
        blocked: AtomicBool::new(false),
        pr: None,
    });
    let status = ACTIVE.scope(frame, future).await;
    let children = outer.store.children(&key)?;
    let failed_write = children.iter().any(|child| {
        child.state == State::Failed
            && !(child.spec.kind == "review"
                && status.starts_with("ok")
                && children.iter().any(|next| {
                    next.state == State::Succeeded
                        && matches!(next.spec.kind.as_str(), "review" | "comment")
                }))
    });
    let state = if children
        .iter()
        .any(|c| matches!(c.state, State::Unknown | State::Running | State::Pending))
    {
        State::Unknown
    } else if failed_write || status.contains("error") || status == "partial" {
        State::Failed
    } else {
        State::Succeeded
    };
    outer.store.finish(
        &claim,
        state,
        Some(&json!({"status":status})),
        &status,
        now(),
    )?;
    Ok(status)
}

/// Called by Client at its write boundary. Task-local scoping leaves legacy
/// sweep/queue/idle consumers unchanged while all durable command subwrites
/// get independent receipts, markers and fencing before the request is sent.
pub(crate) async fn write(
    gh: &Client,
    method: &str,
    route: &str,
    body: Option<Value>,
) -> std::result::Result<Value, GhError> {
    let Ok(frame) = ACTIVE.try_with(Arc::clone) else {
        return gh.raw_write(method, route, body).await;
    };
    match durable_write(gh, &frame, method, route, body).await {
        Ok(value) => Ok(value),
        Err(e) => {
            if !matches!(
                &e,
                GhError::Api {
                    status: 400 | 401 | 403 | 404 | 409 | 410 | 422,
                    ..
                }
            ) {
                frame.blocked.store(true, Ordering::SeqCst);
            }
            Err(e)
        }
    }
}

/// Persist intent before sending, reuse confirmed receipts, and pause ambiguous writes.
async fn durable_write(
    gh: &Client,
    frame: &Frame,
    method: &str,
    route: &str,
    body: Option<Value>,
) -> std::result::Result<Value, GhError> {
    if frame.blocked.load(Ordering::SeqCst) {
        return Err(error(
            "preceding write failed or is unknown; remaining writes paused",
        ));
    }
    let ordinal = {
        let mut counts = frame.counts.lock().map_err(error)?;
        let count = counts.entry((method.into(), route.into())).or_default();
        *count += 1;
        *count
    };
    let key = json!(["write", frame.parent, method, route, ordinal]).to_string();
    let kind = if method == "POST" && route.ends_with("/comments") {
        "comment"
    } else if method == "POST" && route.ends_with("/reviews") {
        "review"
    } else if method == "POST" && route.ends_with("/labels") {
        "ensure_labels"
    } else {
        "other"
    };
    let mut body = body;
    if automatic() && matches!(kind, "comment" | "review") {
        if let Some(object) = body.as_mut() {
            let text = object["body"].as_str().unwrap_or("");
            // Courtesy placeholders carry no result. Omitting them makes a
            // confirmed automatic child receipt evidence of the actual report.
            if text.starts_with("🔄")
                || text.starts_with("🔍 Building")
                || text.starts_with("🔍 正在生成")
                || text.starts_with("⏳")
            {
                return Ok(json!({}));
            }
            let config = frame
                .snapshot
                .lock()
                .map_err(error)?
                .0
                .clone()
                .unwrap_or_default();
            let plan = frame.store.get(&frame.parent).map_err(error)?;
            let rules = plan
                .as_ref()
                .map(|p| p.spec.request["rules"].to_string())
                .unwrap_or_default();
            object["body"] = json!(format!(
                "{text}\n\nAutomatic {}.{} · rules: `{rules}` · config: `{config}` · head: `{}`",
                frame.context.event,
                frame.context.action,
                frame.context.head_sha.as_deref().unwrap_or("n/a")
            ));
            if kind == "review" {
                if object["event"] != "COMMENT" {
                    return Err(error("automatic reviews must use COMMENT"));
                }
                object["commit_id"] = json!(frame.context.head_sha);
            }
        }
    }
    if matches!(kind, "comment" | "review") {
        if let Some(object) = body.as_mut() {
            let text = object["body"].as_str().unwrap_or("");
            object["body"] = json!(format!("{text}\n\n{}", operation_marker(&key)));
        }
    }
    let request: Value = serde_json::from_str(&crate::redact::scrub(
        &json!({"method":method,"route":route,"body":body}).to_string(),
    ))
    .map_err(error)?;
    let spec = OperationSpec {
        key: key.clone(),
        parent: Some(frame.parent.clone()),
        kind: kind.into(),
        context: frame.context.clone(),
        config_sha: frame.snapshot.lock().map_err(error)?.0.clone(),
        request,
    };
    if let Some(op) = frame.store.get(&key).map_err(error)? {
        if op.state == State::Unknown {
            reconcile(&frame.store, gh, frame.app_id, &op, now())
                .await
                .map_err(error)?;
        }
    }
    let Some(claim) = frame.store.claim(&spec, now()).map_err(error)? else {
        let op = frame
            .store
            .get(&key)
            .map_err(error)?
            .ok_or_else(|| error("operation missing"))?;
        if op.state == State::Succeeded {
            return Ok(op.result.unwrap_or(json!({})));
        }
        if op.state == State::Failed {
            if let Some(result) = &op.result {
                if let Some(status) = result["status"].as_u64() {
                    return Err(GhError::Api {
                        status: status as u16,
                        message: result["message"]
                            .as_str()
                            .unwrap_or("request rejected")
                            .into(),
                    });
                }
            }
        }
        return Err(error(format!(
            "operation {} is {:?}; no resend",
            operation_marker(&key),
            op.state
        )));
    };
    // Reuse the original intent after preflight: regenerated AI wording cannot
    // turn a retry into a new notification or change its stable marker.
    let request = &claim.operation.spec.request;
    frame.store.mark_sent(&claim, now()).map_err(error)?;
    let result = gh
        .raw_write(
            method,
            route,
            request.get("body").filter(|v| !v.is_null()).cloned(),
        )
        .await;
    match result {
        Ok(value) => {
            frame
                .store
                .finish(
                    &claim,
                    State::Succeeded,
                    Some(&receipt(&value)),
                    "GitHub confirmed write",
                    now(),
                )
                .map_err(error)?;
            Ok(value)
        }
        Err(e) => {
            // The command handlers already treat an absent label as removed.
            // Preserve that idempotent result in its receipt as well.
            if method == "DELETE"
                && route.contains("/labels/")
                && matches!(&e, GhError::Api { status: 404, .. })
            {
                frame
                    .store
                    .finish(
                        &claim,
                        State::Succeeded,
                        Some(&Value::Null),
                        "label already absent",
                        now(),
                    )
                    .map_err(error)?;
                return Ok(Value::Null);
            }
            let state = if matches!(
                &e,
                GhError::Api {
                    status: 400 | 401 | 403 | 404 | 409 | 410 | 422,
                    ..
                }
            ) {
                State::Failed
            } else {
                State::Unknown
            };
            let rejection = match &e {
                GhError::Api { status, message } if state == State::Failed => {
                    Some(json!({"status":status,"message":crate::redact::scrub(message)}))
                }
                _ => None,
            };
            frame
                .store
                .finish(&claim, state, rejection.as_ref(), &e.to_string(), now())
                .map_err(error)?;
            if state == State::Unknown {
                tracing::error!(operation=%key,"GitHub write result unknown; no automatic non-idempotent retry");
            }
            Err(e)
        }
    }
}
