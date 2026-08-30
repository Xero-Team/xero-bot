//! Self-hosted server (Docker / VPS): axum HTTP server.
//!
//! Routes:
//!   POST /webhook  — GitHub App webhook (same logic as the Vercel function)
//!   GET  /health   — liveness
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
    load_dotenv(".env");
    let cfg = Config::from_env();
    if let Err(e) = cfg.validate() {
        eprintln!("ERROR: {e}");
        std::process::exit(2);
    }

    let port = cfg.port;
    let state = AppState { cfg };

    let app = Router::new()
        .route("/", get(health))
        .route("/health", get(health))
        .route("/webhook", post(webhook))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!(
        "xero-bot listening on 0.0.0.0:{port} (POST /webhook) — bot: @{}",
        Config::from_env().bot_name
    );
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
