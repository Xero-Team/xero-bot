//! Environment configuration, loaded once and passed by reference.
//!
//! Mirrors the Python bot's semantics: values in `.env` never override real
//! environment variables (so Vercel's dashboard config always wins there).

use std::env;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Config {
    // GitHub App
    pub app_id: String,
    pub private_key_pem: Option<String>,
    pub webhook_secret: String,
    pub bot_name: String,
    /// Optional override for the App's own login (without `[bot]`). Empty means
    /// resolve it via `GET /app`; set it on serverless to skip that round trip.
    pub app_slug: String,

    // AI provider (builtin engine)
    pub ai_base_url: String,
    pub ai_api_key: String,
    pub ai_model: String,
    pub api_format: String,
    /// Diff budget for the prompt, in **bytes** — `MAX_DIFF_CHARS` keeps its
    /// name for compatibility, but the guard has always measured `str::len`,
    /// and [`crate::review::truncate`] now cuts in the same unit it checks.
    pub max_diff_chars: usize,

    // Review engines
    pub review_engine: String, // auto | builtin | agent | pi | codex
    pub agent_max_turns: usize,
    pub agent_timeout_secs: u64,

    // Subprocess engines (self-hosted only)
    pub pi_path: String,
    pub pi_args: String,
    pub codex_path: String,
    pub codex_args: String,
    pub data_dir: String,

    // Cron (Vercel)
    pub cron_secret: String,

    // Rebase detection
    pub rebase_check_delay_secs: u64,
    pub rebase_sweep_enabled: bool,
    pub rebase_sweep_interval_secs: u64,

    // Labels
    pub label_needs_rebase: String,
    pub label_waiting_review: String,
    pub label_waiting_author: String,
    pub label_blocked: String,
    pub codeql_label: String,

    // Service
    pub port: u16,
}

/// Load `.env` into the process environment without clobbering real vars.
pub fn load_dotenv(path: &str) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return,
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let mut v = v.trim().to_string();
        // strip surrounding quotes
        if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
            || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
        {
            v = v[1..v.len() - 1].to_string();
        }
        if !k.is_empty() && env::var(k).is_err() {
            env::set_var(k, v);
        }
    }
}

fn cfg(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn int_cfg(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn bool_cfg(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

/// Resolve the App private key from `PRIVATE_KEY_B64` (works anywhere) or
/// `PRIVATE_KEY_PATH` (self-hosted only).
///
/// Both sources are treated as unset when blank. This matters: a leftover
/// `PRIVATE_KEY_B64=` in `.env` is a *set* variable whose empty value decodes
/// to an empty key, which would otherwise shadow `PRIVATE_KEY_PATH` and fail
/// much later as an opaque `InvalidKeyFormat` at JWT-signing time.
fn read_key_pem() -> Option<String> {
    if let Some(b64) = env::var("PRIVATE_KEY_B64")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        use base64::Engine;
        match base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(pem) if !pem.trim().is_empty() => return Some(pem),
                Ok(_) => tracing::warn!("PRIVATE_KEY_B64 decoded to empty content; ignoring"),
                Err(e) => tracing::warn!("PRIVATE_KEY_B64 is not valid UTF-8: {e}; ignoring"),
            },
            Err(e) => tracing::warn!("PRIVATE_KEY_B64 is not valid base64: {e}; ignoring"),
        }
    }

    let path = env::var("PRIVATE_KEY_PATH").ok()?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    match std::fs::read_to_string(Path::new(path)) {
        Ok(pem) if !pem.trim().is_empty() => Some(pem),
        Ok(_) => {
            tracing::warn!("PRIVATE_KEY_PATH file is empty: {path}");
            None
        }
        Err(e) => {
            tracing::warn!("cannot read PRIVATE_KEY_PATH {path}: {e}");
            None
        }
    }
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            app_id: cfg("APP_ID", ""),
            private_key_pem: read_key_pem(),
            webhook_secret: cfg("WEBHOOK_SECRET", ""),
            bot_name: cfg("BOT_NAME", "xero-review"),
            app_slug: cfg("APP_SLUG", ""),

            ai_base_url: cfg("AI_BASE_URL", ""),
            ai_api_key: cfg("AI_API_KEY", ""),
            ai_model: cfg("AI_MODEL", ""),
            api_format: cfg("API_FORMAT", "chat"),
            max_diff_chars: int_cfg("MAX_DIFF_CHARS", 60_000) as usize,

            review_engine: cfg("REVIEW_ENGINE", "auto"),
            agent_max_turns: int_cfg("AGENT_MAX_TURNS", 8) as usize,
            agent_timeout_secs: int_cfg("AGENT_TIMEOUT_SECS", 240),

            pi_path: cfg("PI_PATH", "pi"),
            pi_args: cfg("PI_ARGS", ""),
            codex_path: cfg("CODEX_PATH", "codex"),
            codex_args: cfg("CODEX_ARGS", ""),
            data_dir: cfg("XERO_DATA_DIR", "/tmp/xero"),

            cron_secret: cfg("CRON_SECRET", ""),

            rebase_check_delay_secs: int_cfg("REBASE_CHECK_DELAY_SECS", 10),
            rebase_sweep_enabled: bool_cfg("REBASE_SWEEP_ENABLED", true),
            rebase_sweep_interval_secs: int_cfg("REBASE_SWEEP_INTERVAL_SECS", 21_600),

            label_needs_rebase: cfg("LABEL_NEEDS_REBASE", "needs-rebase"),
            label_waiting_review: cfg("LABEL_WAITING_REVIEW", "waiting-on-review"),
            label_waiting_author: cfg("LABEL_WAITING_AUTHOR", "waiting-on-author"),
            label_blocked: cfg("LABEL_BLOCKED", "blocked"),
            codeql_label: cfg("CODEQL_LABEL", ""),

            port: int_cfg("PORT", 8080) as u16,
        }
    }

    /// Required for any GitHub API access. Returns Err listing what's missing.
    pub fn validate(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        if self.app_id.is_empty() {
            missing.push("APP_ID");
        }
        if self.private_key_pem.is_none() {
            missing.push("PRIVATE_KEY_PATH (or PRIVATE_KEY_B64)");
        }
        if self.webhook_secret.is_empty() {
            missing.push("WEBHOOK_SECRET");
        }
        if self.bot_name.is_empty() {
            missing.push("BOT_NAME");
        }
        if !missing.is_empty() {
            return Err(format!("missing required env vars: {}", missing.join(", ")));
        }

        // Parse the key now: signing happens on a background task whose errors
        // are only logged, so an unusable key would otherwise surface as a
        // per-webhook `InvalidKeyFormat` instead of a startup failure.
        if let Some(pem) = &self.private_key_pem {
            if jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).is_err() {
                let head = pem.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                return Err(format!(
                    "private key is not a usable RSA PEM (loaded {} bytes, first line {:?}). \
                     Use the .pem downloaded from the GitHub App settings page.",
                    pem.len(),
                    head.trim()
                ));
            }
        }
        Ok(())
    }

    /// AI config is only needed when a review actually runs; check lazily.
    pub fn ai_ready(&self) -> bool {
        !self.ai_base_url.is_empty() && !self.ai_api_key.is_empty() && !self.ai_model.is_empty()
    }

    pub fn pem(&self) -> Result<&str, String> {
        self.private_key_pem
            .as_deref()
            .ok_or_else(|| "GitHub App private key not configured".to_string())
    }
}
