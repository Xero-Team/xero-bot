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
}

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

    let port = cfg.port;
    let bot_name = cfg.bot_name.clone();
    let state = AppState { cfg };

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
    (
        StatusCode::OK,
        Json(json!({"ok": true, "summary": summary, "merge_queue": merge_queue_summary})),
    )
}
