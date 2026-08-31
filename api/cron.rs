//! Vercel function: GET /api/cron — rebase sweep, invoked by Vercel Cron.
//!
//! Vercel sends `Authorization: Bearer $CRON_SECRET`. CRON_SECRET is required:
//! the user agent was previously accepted as a fallback, but a UA is trivially
//! forged and the sweep walks every installation, so it isn't authentication.

use serde_json::{json, Value};
use vercel_runtime::{run, service_fn, Error, Request, Response, ResponseBody};

use xero_bot::config::{load_dotenv, Config};

#[tokio::main]
async fn main() -> Result<(), Error> {
    // initialize tracing so sweep errors show in function logs
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,xero_bot=debug".into()),
        )
        .with_ansi(false)
        .try_init();
    run(service_fn(handler)).await
}

async fn handler(req: Request, _state: AppState) -> Result<Response<ResponseBody>, Error> {
    load_dotenv(".env");
    let cfg = Config::from_env();

    if let Err(e) = cfg.validate() {
        tracing::error!("refusing cron: {e}");
        return json_response(500, json!({"error": "server misconfigured"}));
    }

    if req.method().as_str() != "GET" && req.method().as_str() != "HEAD" {
        return json_response(405, json!({"error": "method not allowed"}));
    }

    // Require the bearer secret. Fail closed when it isn't configured rather
    // than falling back to the user agent, which anyone can set.
    if cfg.cron_secret.is_empty() {
        tracing::error!("refusing cron: CRON_SECRET is not set");
        return json_response(500, json!({"error": "CRON_SECRET not configured"}));
    }
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != format!("Bearer {}", cfg.cron_secret) {
        return json_response(401, json!({"error": "unauthorized"}));
    }

    let summary = xero_bot::rebase::sweep(&cfg).await;
    json_response(200, json!({"ok": true, "summary": summary}))
}

use vercel_runtime::AppState;

fn json_response(status: u16, body: Value) -> Result<Response<ResponseBody>, Error> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(ResponseBody::from(body.to_string()))?)
}
