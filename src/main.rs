//! Self-hosted server (Docker / VPS): axum HTTP server.
//!
//! Routes:
//!   POST /webhook  — GitHub App webhook receiver
//!   GET  /health   — liveness
//!   GET  /cron     — rebase sweep (protect with CRON_SECRET, or bind to
//!                    localhost and drive it with an external cron)
//!
//! Background work runs on tokio::spawn (no time limit).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use xero_bot::config::{load_dotenv, Config};
use xero_bot::dispatch::{execute_work, route_event, Routing};
use xero_bot::webhook::verify_signature;

#[derive(Clone)]
struct AppState {
    cfg: Config,
    idle_workflows: Option<Arc<xero_bot::idle_workflows::Scheduler>>,
    triggers: Arc<xero_bot::trigger_state::Runtime>,
}

/// Validate deployment settings, acquire state ownership, then start workers and HTTP ingress.
#[tokio::main]
async fn main() {
    // initialize tracing so background-work errors actually show in logs
    // (without this, tracing::error!/info! calls are silently dropped)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,xero_bot=debug".into()),
        )
        .init();

    load_dotenv(".env");
    let cfg = Config::from_env();
    if let Err(e) = cfg.validate() {
        eprintln!("ERROR: {e}");
        std::process::exit(2);
    }

    let triggers = match xero_bot::trigger_state::Runtime::open(std::path::Path::new(&cfg.data_dir))
    {
        Ok(runtime) => Arc::new(runtime),
        Err(e) => {
            eprintln!("ERROR: cannot own trigger state in XERO_DATA_DIR (storage unavailable or another instance owns it): {e}");
            std::process::exit(2);
        }
    };

    let idle_workflows = if cfg.idle_workflows_enabled {
        match xero_bot::idle_workflows::Scheduler::open(
            std::path::Path::new(&cfg.data_dir),
            cfg.idle_workflows_poll_interval_secs,
        ) {
            Ok(scheduler) => Some(Arc::new(scheduler)),
            Err(e) => {
                eprintln!(
                    "ERROR: cannot open idle workflow state (another instance may own it): {e}"
                );
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    // We bind 0.0.0.0, so an unauthenticated /cron is reachable from anywhere
    // the port is. The sweep walks every installation and can post reminder
    // comments, so make the exposure visible rather than silently trusting the
    // ".env" note about binding to an internal network.
    if cfg.cron_secret.is_empty() {
        tracing::warn!(
            "CRON_SECRET is empty: GET /cron is unauthenticated and this server \
             listens on 0.0.0.0 — set CRON_SECRET or firewall port {}",
            cfg.port
        );
    }

    // One throwaway AI request before serving. A bad key or a mismatched
    // API_FORMAT otherwise stays hidden until the first `@bot review`, where it
    // surfaces as a failed review on someone's PR. A failure here is a warning,
    // not an exit: the non-AI commands (r+, labels, rebase checks) still work,
    // and refusing to boot would take those down over an unrelated outage.
    match xero_bot::review::preflight(&cfg).await {
        Some(probe) => match probe.verdict {
            Ok(detail) => tracing::info!("AI check — {}: {detail} [{}]", probe.what, probe.url),
            Err(detail) => tracing::warn!(
                "AI check FAILED — {}: {detail} [{}] — reviews will fail until this is fixed",
                probe.what,
                probe.url
            ),
        },
        None => tracing::info!(
            "AI check skipped: AI_BASE_URL / AI_API_KEY / AI_MODEL are not all set \
             (non-AI commands still work)"
        ),
    }

    // rebase sweep loop
    if cfg.rebase_sweep_enabled {
        let sweep_cfg = cfg.clone();
        tokio::spawn(async move {
            let interval =
                std::time::Duration::from_secs(sweep_cfg.rebase_sweep_interval_secs.max(60));
            // Sweep right at boot: a deployment gap can hide base-branch moves
            // (no webhook fires for "someone else's PR merged and dirtied an
            // open PR"), and sleeping a full interval first turned that gap
            // into hours of silent conflicts on every redeploy. The delay is
            // only long enough for the installation clients to be ready; real
            // base-move latency is covered by the push event, not this loop.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let _ = xero_bot::rebase::sweep(&sweep_cfg).await;
            loop {
                tokio::time::sleep(interval).await;
                let _ = xero_bot::rebase::sweep(&sweep_cfg).await;
            }
        });
    }

    // merge queue driver loop. Sleep-first like the rebase sweep: a restart
    // mid-batch recovers on the first tick, but booting shouldn't immediately
    // race GitHub's webhooks with a burst of staging writes. One loop, never
    // concurrent with itself — the staging ref and the label dance have no
    // lock, and two writers would corrupt both.
    if cfg.merge_queue_enabled {
        tracing::info!(
            "merge queue enabled: staging branch {:?}, poll every {}s",
            cfg.merge_queue_staging_branch,
            cfg.merge_queue_poll_interval_secs
        );
        let merge_queue_cfg = cfg.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(
                merge_queue_cfg.merge_queue_poll_interval_secs.max(15),
            );
            loop {
                tokio::time::sleep(interval).await;
                let _ = xero_bot::merge_queue::pump_all(&merge_queue_cfg).await;
            }
        });
    }

    if let Some(scheduler) = idle_workflows.as_ref().map(Arc::clone) {
        let scheduler_cfg = cfg.clone();
        tokio::spawn(async move {
            let interval =
                std::time::Duration::from_secs(scheduler_cfg.idle_workflows_poll_interval_secs);
            loop {
                let summary = scheduler.pump_all(&scheduler_cfg).await;
                tracing::debug!("{summary}");
                tokio::time::sleep(interval).await;
            }
        });
    }

    let trigger_worker = Arc::clone(&triggers);
    let trigger_cfg = cfg.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = trigger_worker.pump(&trigger_cfg).await {
                tracing::error!("trigger recovery unavailable: {error}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });

    let port = cfg.port;
    let bot_name = cfg.bot_name.clone();
    let state = AppState {
        cfg,
        idle_workflows,
        triggers,
    };

    let app = Router::new()
        .route("/", get(health))
        .route("/health", get(health))
        .route("/webhook", post(webhook))
        .route("/cron", get(cron_sweep))
        .with_state(state);

    // Bind *before* announcing it. The other order printed "listening on
    // 0.0.0.0:8080" and then panicked on `AddrInUse`, so the log's last
    // successful-looking line described something that never happened.
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            // Not a panic: a busy port is an operator's mistake, not a bug, and
            // a backtrace buries the one line that says which port and why.
            eprintln!("ERROR: cannot bind 0.0.0.0:{port}: {e}");
            std::process::exit(2);
        }
    };
    println!("xero-bot listening on 0.0.0.0:{port} (POST /webhook, GET /cron) — bot: @{bot_name}");

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("ERROR: server stopped: {e}");
        std::process::exit(1);
    }
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// Verify signatures, reject irrelevant comments, and persist work before acknowledging it.
async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if !verify_signature(&state.cfg.webhook_secret, &body, signature.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid signature"})),
        );
    }

    let event_header = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad json"}))),
    };

    xero_bot::config::cache::RepositoryConfigCache::shared()
        .observe_webhook(&event_header, &payload);

    // Reject irrelevant/self comments before retaining user text. Routing is
    // pure; execution still reparses and checks current policy in the worker.
    if event_header == "issue_comment" {
        if let Routing::Respond(response) = route_event(&state.cfg, &event_header, &payload) {
            return (StatusCode::OK, Json(response));
        }
    }

    let delivery = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok());
    let durable =
        match xero_bot::trigger_state::EventContext::capture(&event_header, &payload, delivery) {
            Ok(context) => context,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":error.to_string()})),
                )
            }
        };
    if let Some(context) = &durable {
        if let Err(error) = state
            .triggers
            .store
            .enqueue(context, xero_bot::github::chrono_now_secs())
        {
            tracing::error!("trigger inbox persistence failed: {error}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"trigger persistence unavailable"})),
            );
        }
    }

    if let Some(scheduler) = &state.idle_workflows {
        let delivery = headers
            .get("x-github-delivery")
            .and_then(|v| v.to_str().ok());
        if let Err(error) = scheduler.observe_webhook(&event_header, &payload, delivery) {
            tracing::error!("idle workflow activity could not be persisted: {error}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "activity persistence unavailable"})),
            );
        }
    }

    if durable.is_some() && event_header == "issue_comment" {
        return (
            StatusCode::OK,
            Json(json!({"accepted":true,"persisted":true})),
        );
    }

    match route_event(&state.cfg, &event_header, &payload) {
        Routing::Respond(body) => (StatusCode::OK, Json(body)),
        Routing::Act(work) => {
            let cfg = state.cfg.clone();
            tokio::spawn(async move {
                execute_work(&cfg, work).await;
            });
            (StatusCode::OK, Json(json!({"accepted": true})))
        }
    }
}

async fn cron_sweep(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    // allow: bearer CRON_SECRET, localhost, or no secret configured
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let authorized =
        state.cfg.cron_secret.is_empty() || auth == format!("Bearer {}", state.cfg.cron_secret);
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        );
    }
    let summary = xero_bot::rebase::sweep(&state.cfg).await;
    // The queue rides the same external trigger as the sweep, as a
    // belt-and-braces complement to the built-in loop (and the only driver
    // tick when the built-in loop is off).
    let merge_queue_summary = if state.cfg.merge_queue_enabled {
        xero_bot::merge_queue::pump_all(&state.cfg).await
    } else {
        "merge queue disabled".to_string()
    };
    let idle_workflows_summary = match &state.idle_workflows {
        Some(scheduler) => scheduler.pump_all(&state.cfg).await,
        None => "idle workflows disabled".into(),
    };
    (
        StatusCode::OK,
        Json(
            json!({"ok": true, "summary": summary, "merge_queue": merge_queue_summary, "idle_workflows": idle_workflows_summary}),
        ),
    )
}

#[cfg(test)]
mod trigger_ingress_tests {
    use super::*;
    use hmac::Mac;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Dir(PathBuf);
    impl Dir {
        /// Allocate an isolated state directory for ingress tests.
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "xero-trigger-ingress-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Dir {
        /// Remove only the temporary state directory owned by this test.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    /// Build a signed-command fixture with complete recovery identity.
    fn payload() -> Value {
        json!({"action":"created","repository":{"id":1,"full_name":"test/repo"},"installation":{"id":2},"issue":{"id":3,"number":1,"user":{"login":"alice"}},"comment":{"id":4,"body":"@bot ping","created_at":"2026-10-02T00:00:00Z","user":{"id":5,"login":"alice","type":"User"}}})
    }
    /// Create ingress state with durable storage and the idle scheduler disabled.
    fn state(dir: &Dir) -> AppState {
        let mut cfg = Config::from_env();
        cfg.webhook_secret = "secret".into();
        cfg.bot_name = "bot".into();
        cfg.idle_workflows_enabled = false;
        AppState {
            cfg,
            idle_workflows: None,
            triggers: Arc::new(xero_bot::trigger_state::Runtime::open(&dir.0).unwrap()),
        }
    }
    /// Sign the exact fixture bytes and attach GitHub event and delivery headers.
    fn signed(body: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        let mut mac = xero_bot::webhook::HmacSha256::new_from_slice(b"secret").unwrap();
        mac.update(body);
        h.insert(
            "x-hub-signature-256",
            format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
                .parse()
                .unwrap(),
        );
        h.insert("x-github-delivery", "delivery".parse().unwrap());
        h.insert("x-github-event", "issue_comment".parse().unwrap());
        h
    }
    /// Assert that accepted command events are already durable with idle scheduling disabled.
    #[tokio::test]
    async fn verified_inbox_is_committed_before_acceptance_without_idle() {
        let dir = Dir::new();
        let s = state(&dir);
        let body = serde_json::to_vec(&payload()).unwrap();
        let headers = signed(&body);
        let (status, result) = webhook(State(s.clone()), headers, body.into()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result.0["persisted"], true);
        assert_eq!(
            s.triggers.store.inbox_status().unwrap()[0]["state"],
            "pending"
        );
    }
    /// Reject untrusted or incomplete command envelopes before inserting inbox rows.
    #[tokio::test]
    async fn invalid_signature_and_missing_recovery_metadata_cannot_enter_inbox() {
        let dir = Dir::new();
        let s = state(&dir);
        let body = serde_json::to_vec(&payload()).unwrap();
        assert_eq!(
            webhook(State(s.clone()), HeaderMap::new(), body.clone().into())
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let mut h = signed(&body);
        h.remove("x-github-delivery");
        assert_eq!(
            webhook(State(s.clone()), h, body.into()).await.0,
            StatusCode::BAD_REQUEST
        );
        assert!(s.triggers.store.inbox_status().unwrap().is_empty());
    }
    /// Inject an inbox failure and verify a retryable response with no action execution.
    #[tokio::test]
    async fn injected_disk_failure_returns_retryable_failure_before_any_execution() {
        let dir = Dir::new();
        drop(state(&dir));
        let db = rusqlite::Connection::open(dir.0.join("command-triggers.sqlite")).unwrap();
        db.execute_batch("CREATE TRIGGER inject_disk_failure BEFORE INSERT ON inbox BEGIN SELECT RAISE(ABORT,'injected disk failure'); END;").unwrap();
        drop(db);
        let s = state(&dir);
        let body = serde_json::to_vec(&payload()).unwrap();
        let headers = signed(&body);
        let (status, result) = webhook(State(s.clone()), headers, body.into()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_ne!(result.0["accepted"], true);
        assert!(s.triggers.store.list(None).unwrap().is_empty());
    }
    /// Ordinary discussion and self replies must never consume durable inbox space.
    #[tokio::test]
    async fn irrelevant_and_self_comments_are_filtered_before_persistence() {
        let dir = Dir::new();
        let s = state(&dir);
        for self_reply in [false, true] {
            let mut event = payload();
            if self_reply {
                event["comment"]["user"] = json!({"id":9,"login":"bot[bot]","type":"Bot"});
            } else {
                event["comment"]["body"] = json!("claim 是什么意思？");
            }
            let body = serde_json::to_vec(&event).unwrap();
            let (status, result) = webhook(State(s.clone()), signed(&body), body.into()).await;
            assert_eq!(status, StatusCode::OK);
            assert!(result.0["ignored"].is_string());
            assert_ne!(result.0["persisted"], true);
        }
        assert!(s.triggers.store.inbox_status().unwrap().is_empty());
    }
}
