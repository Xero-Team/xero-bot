//! Vercel function: POST /api/webhook — GitHub App webhook receiver.
//!
//! Verify signature → route → respond 200 immediately → run the actual work
//! via AppState::wait_until (survives the response, bounded by maxDuration).

use http_body_util::BodyExt;
use serde_json::{json, Value};
use vercel_runtime::{run, service_fn, AppState, Error, Request, Response, ResponseBody};

use xero_bot::config::{load_dotenv, Config};
use xero_bot::dispatch::{execute_work, route_event, Routing};
use xero_bot::webhook::verify_signature;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // initialize tracing so background-work errors show in function logs
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,xero_bot=debug".into()),
        )
        .with_ansi(false)
        .try_init();
    run(service_fn(handler)).await
}

async fn handler(req: Request, state: AppState) -> Result<Response<ResponseBody>, Error> {
    load_dotenv(".env");
    let cfg = Config::from_env();

    // The self-hosted server exits at startup on a bad config; a serverless
    // function has no startup, so check per request. Answer 500 rather than 200
    // so the failure shows up red on GitHub's delivery page instead of looking
    // like the bot quietly ignored the event.
    if let Err(e) = cfg.validate() {
        tracing::error!("refusing webhook: {e}");
        return json_response(500, json!({"error": "server misconfigured"}));
    }

    if req.method().as_str() != "POST" {
        return json_response(405, json!({"error": "method not allowed"}));
    }

    // split request: keep headers, read body to bytes
    let (parts, body) = req.into_parts();
    let body_bytes: Vec<u8> = body
        .collect()
        .await
        .map_err(|e| format!("read body: {e}"))?
        .to_bytes()
        .to_vec();

    let signature = parts
        .headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if !verify_signature(&cfg.webhook_secret, &body_bytes, signature.as_deref()) {
        return json_response(401, json!({"error": "invalid signature"}));
    }

    let event_header = parts
        .headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let payload: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => return json_response(400, json!({"error": "bad json"})),
    };

    match route_event(&cfg, &event_header, &payload) {
        Routing::Respond(body) => json_response(200, body),
        Routing::Act(work) => {
            // respond now; keep the future alive after the response
            state.wait_until(async move {
                execute_work(&cfg, work).await;
            });
            json_response(200, json!({"accepted": true}))
        }
    }
}

fn json_response(status: u16, body: Value) -> Result<Response<ResponseBody>, Error> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(ResponseBody::from(body.to_string()))?)
}
