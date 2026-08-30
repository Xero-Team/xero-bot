//! Vercel function: GET /api/health — liveness probe.

use serde_json::json;
use vercel_runtime::{run, service_fn, Error, Request, Response, ResponseBody};

use xero_bot::config::{load_dotenv, Config};

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}

async fn handler(_req: Request, _state: AppState) -> Result<Response<ResponseBody>, Error> {
    let _ = load_dotenv(".env");
    let cfg = Config::from_env();
    let ready = cfg.validate().is_ok();
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(ResponseBody::from(
            json!({"status": "ok", "configured": ready, "bot": cfg.bot_name}).to_string(),
        ))?)
}

use vercel_runtime::AppState;
