//! Environment configuration, loaded once and passed by reference.
//!
//! Values in `.env` never override real environment variables, so an
//! orchestrator's `environment:` block (docker compose) always wins.

pub mod cache;
pub mod repository;

use std::env;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Config {
    // GitHub App
    pub app_id: String,
    pub private_key_pem: Option<String>,
    pub webhook_secret: String,
    pub bot_name: String,
    /// Forgejo command mention/login, independent from the GitHub App identity.
    pub forgejo_bot_name: String,
    /// Optional override for the App's own login (without `[bot]`). Empty means
    /// resolve it via `GET /app`; set it on serverless to skip that round trip.
    pub app_slug: String,

    // Forgejo provider (independent token API + webhook secret)
    pub forgejo_base_url: String,
    pub forgejo_token: String,
    pub forgejo_webhook_secret: String,
    /// JSON object: Forgejo login -> GitHub login. Presentation only; never permissions.
    pub forgejo_user_map: String,

    // AI provider (builtin engine)
    pub ai_base_url: String,
    pub ai_api_key: String,
    pub ai_model: String,
    pub api_format: String,
    /// Diff budget for the prompt, in **bytes** — `MAX_DIFF_CHARS` keeps its
    /// name for compatibility, but the guard has always measured `str::len`,
    /// and [`crate::review::truncate`] now cuts in the same unit it checks.
    pub max_diff_chars: usize,

    /// Adversarial re-check of the model's own findings.
    ///
    /// A second AI call, blind to the first verdict, is asked to *refute* each
    /// critical/high/medium finding against the diff alone. Findings it cannot
    /// confirm are demoted, not published as fact. Off by default because it
    /// multiplies the model spend roughly by the number of findings; turn it on
    /// with `REVIEW_VERIFY=true` on deployments that prefer precision over
    /// latency.
    pub review_verify: bool,

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
    /// How much history to fetch for a review checkout.
    ///
    /// Deep enough that the PR and its base branch usually share an ancestor —
    /// without one, `git diff base...HEAD` has no left side and the model is
    /// shown the whole branch instead of the change. When it isn't enough the
    /// checkout deepens once and carries on rather than paying for a full clone.
    pub checkout_depth: u32,

    // Cron endpoint (/cron Bearer secret)
    pub cron_secret: String,

    /// May `r+ as @someone-else` credit its approval to another user?
    ///
    /// Default **false**, because the approval is posted by the App and a
    /// branch-protection rule counts it as a real one: with this on, anyone
    /// holding write access can manufacture an approval in a third party's name
    /// and satisfy a required-review rule without that person seeing the PR.
    /// Plain `r+` — crediting the commenter, who had to have write access
    /// anyway — is unaffected and stays enabled.
    pub r_plus_allow_on_behalf: bool,

    // Rebase detection
    pub rebase_check_delay_secs: u64,
    pub rebase_sweep_enabled: bool,
    pub rebase_sweep_interval_secs: u64,

    /// Discover opt-in .github/xero-bot.toml rules and persist scheduling state.
    pub idle_workflows_enabled: bool,
    pub idle_workflows_poll_interval_secs: u64,

    // Merge queue
    /// Legacy staging queue switch. Default false; repositories with rollup
    /// enabled use their own TOML policy instead of this driver.
    pub merge_queue_enabled: bool,
    /// Rollup service availability, default true. Each repository controls
    /// activation with [rollup].enabled; false disables Rollup service support.
    pub rollup_enabled: bool,
    pub public_url: String,
    /// The branch PR heads are merged into, one batch at a time.
    pub merge_queue_staging_branch: String,
    /// Upper bound on one batch; the queue takes PRs in ascending number.
    pub merge_queue_max_batch: usize,
    /// How long a batch may sit without reaching a CI verdict before every
    /// member is dequeued with an explanation.
    pub merge_queue_ci_timeout_secs: u64,
    /// Driver tick interval, floored at 15s (see `validate`).
    pub merge_queue_poll_interval_secs: u64,
    /// How main is advanced once the batch is green: `"pr"` (a staging→main
    /// PR, inherits branch protection) or `"ref"` (fast-forward ref update,
    /// needs the App exempted from push restrictions).
    pub merge_queue_advance_method: String,
    /// Merge method for the advance PR: `"merge"` or `"squash"` (`"rebase"`
    /// would re-linearize the staging commits — not offered).
    pub merge_queue_advance_merge_method: String,
    /// Delete the staging ref after a batch completes; recreated next batch.
    pub merge_queue_cleanup_staging: bool,
    /// When the advance PR cannot merge because main moved by hand: keep the
    /// batch and retry (false, default) or dequeue everything (true).
    pub merge_queue_advance_strict: bool,

    // Labels
    pub label_needs_rebase: String,
    pub label_waiting_review: String,
    pub label_waiting_author: String,
    pub label_blocked: String,
    /// PR is waiting for a batch to start (`merge queue: queued`) / is a member of
    /// the batch currently under test (`merge queue: testing`). Together with the
    /// staging branch's merge-commit chain, these labels are the whole queue
    /// state — the bot has no database.
    pub label_merge_queue_queued: String,
    pub label_merge_queue_testing: String,
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
        let cfg = Config::read_env();
        // Registered here rather than at each call site: every entry point —
        // the server, the cron sweep — builds its config through this one
        // function, so this is the only place that cannot be forgotten when a
        // new one is added.
        crate::redact::register(&cfg);
        cfg
    }

    fn read_env() -> Config {
        Config {
            app_id: cfg("APP_ID", ""),
            private_key_pem: read_key_pem(),
            webhook_secret: cfg("WEBHOOK_SECRET", ""),
            bot_name: cfg("BOT_NAME", "xero-review"),
            forgejo_bot_name: {
                let configured = cfg("FORGEJO_BOT_NAME", "");
                if configured.trim().is_empty() {
                    cfg("BOT_NAME", "xero-review")
                } else {
                    configured
                }
            },
            app_slug: cfg("APP_SLUG", ""),

            forgejo_base_url: cfg("FORGEJO_BASE_URL", ""),
            forgejo_token: cfg("FORGEJO_TOKEN", ""),
            forgejo_webhook_secret: cfg("FORGEJO_WEBHOOK_SECRET", ""),
            forgejo_user_map: cfg("FORGEJO_USER_MAP", "{}"),

            ai_base_url: cfg("AI_BASE_URL", ""),
            ai_api_key: cfg("AI_API_KEY", ""),
            ai_model: cfg("AI_MODEL", ""),
            api_format: cfg("API_FORMAT", "chat"),
            max_diff_chars: int_cfg("MAX_DIFF_CHARS", 60_000) as usize,
            review_verify: bool_cfg("REVIEW_VERIFY", false),

            review_engine: cfg("REVIEW_ENGINE", "auto"),
            agent_max_turns: int_cfg("AGENT_MAX_TURNS", 8) as usize,
            agent_timeout_secs: int_cfg("AGENT_TIMEOUT_SECS", 240),

            pi_path: cfg("PI_PATH", "pi"),
            pi_args: cfg("PI_ARGS", ""),
            codex_path: cfg("CODEX_PATH", "codex"),
            codex_args: cfg("CODEX_ARGS", ""),
            data_dir: cfg("XERO_DATA_DIR", "/tmp/xero"),
            checkout_depth: int_cfg("CHECKOUT_DEPTH", 100) as u32,

            cron_secret: cfg("CRON_SECRET", ""),
            r_plus_allow_on_behalf: bool_cfg("R_PLUS_ALLOW_ON_BEHALF", false),

            rebase_check_delay_secs: int_cfg("REBASE_CHECK_DELAY_SECS", 10),
            rebase_sweep_enabled: bool_cfg("REBASE_SWEEP_ENABLED", true),
            rebase_sweep_interval_secs: int_cfg("REBASE_SWEEP_INTERVAL_SECS", 21_600),

            idle_workflows_enabled: bool_cfg("IDLE_WORKFLOWS_ENABLED", false),
            idle_workflows_poll_interval_secs: int_cfg("IDLE_WORKFLOWS_POLL_INTERVAL_SECS", 60),

            merge_queue_enabled: bool_cfg("MERGE_QUEUE_ENABLED", false),
            rollup_enabled: bool_cfg("ROLLUP_ENABLED", true),
            public_url: cfg("PUBLIC_URL", "https://bot.thefool.chat"),
            merge_queue_staging_branch: cfg("MERGE_QUEUE_STAGING_BRANCH", "staging"),
            merge_queue_max_batch: int_cfg("MERGE_QUEUE_MAX_BATCH", 8) as usize,
            merge_queue_ci_timeout_secs: int_cfg("MERGE_QUEUE_CI_TIMEOUT_SECS", 7_200),
            merge_queue_poll_interval_secs: int_cfg("MERGE_QUEUE_POLL_INTERVAL_SECS", 30),
            merge_queue_advance_method: cfg("MERGE_QUEUE_ADVANCE_METHOD", "pr"),
            merge_queue_advance_merge_method: cfg("MERGE_QUEUE_ADVANCE_MERGE_METHOD", "merge"),
            merge_queue_cleanup_staging: bool_cfg("MERGE_QUEUE_CLEANUP_STAGING", true),
            merge_queue_advance_strict: bool_cfg("MERGE_QUEUE_ADVANCE_STRICT", false),

            label_needs_rebase: cfg("LABEL_NEEDS_REBASE", "needs-rebase"),
            label_waiting_review: cfg("LABEL_WAITING_REVIEW", "waiting-on-review"),
            label_waiting_author: cfg("LABEL_WAITING_AUTHOR", "waiting-on-author"),
            label_blocked: cfg("LABEL_BLOCKED", "blocked"),
            label_merge_queue_queued: cfg("LABEL_MERGE_QUEUE_QUEUED", "merge queue: queued"),
            label_merge_queue_testing: cfg("LABEL_MERGE_QUEUE_TESTING", "merge queue: testing"),
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
        validate_forgejo_values(
            &self.forgejo_base_url,
            &self.forgejo_token,
            &self.forgejo_webhook_secret,
        )?;
        if self.forgejo_ready() && self.forgejo_bot_name.trim().is_empty() {
            return Err("FORGEJO_BOT_NAME is empty".into());
        }
        crate::forgejo::identity::UserMap::parse(&self.forgejo_user_map)?;

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

        // Merge-queue values are validated here rather than defaulted silently: a
        // typo'd MERGE_QUEUE_ADVANCE_METHOD would otherwise fall back to an implicit
        // "pr" — merging with a method the operator never chose.
        if self.merge_queue_enabled {
            if self.merge_queue_staging_branch.is_empty() {
                return Err("MERGE_QUEUE_STAGING_BRANCH is empty".into());
            }
            if !matches!(self.merge_queue_advance_method.as_str(), "pr" | "ref") {
                return Err(format!(
                    "MERGE_QUEUE_ADVANCE_METHOD must be \"pr\" or \"ref\" (got {:?})",
                    self.merge_queue_advance_method
                ));
            }
            if !matches!(
                self.merge_queue_advance_merge_method.as_str(),
                "merge" | "squash"
            ) {
                return Err(format!(
                    "MERGE_QUEUE_ADVANCE_MERGE_METHOD must be \"merge\" or \"squash\" (got {:?})",
                    self.merge_queue_advance_merge_method
                ));
            }
            if self.merge_queue_max_batch == 0 {
                return Err("MERGE_QUEUE_MAX_BATCH must be at least 1".into());
            }
        }
        if self.idle_workflows_enabled && self.idle_workflows_poll_interval_secs < 15 {
            return Err("IDLE_WORKFLOWS_POLL_INTERVAL_SECS must be at least 15".into());
        }
        Ok(())
    }

    /// Forgejo is opt-in at deployment level. All three values are required
    /// together so an incomplete setup cannot silently downgrade security.
    pub fn forgejo_ready(&self) -> bool {
        !self.forgejo_base_url.trim().is_empty()
            && !self.forgejo_token.trim().is_empty()
            && !self.forgejo_webhook_secret.trim().is_empty()
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

/// Validate the optional Forgejo deployment triple without echoing secrets.
fn validate_forgejo_values(
    base_url: &str,
    token: &str,
    webhook_secret: &str,
) -> Result<(), String> {
    let supplied = [base_url, token, webhook_secret]
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .count();
    if supplied == 0 {
        return Ok(());
    }
    let missing: Vec<_> = [
        ("FORGEJO_BASE_URL", base_url),
        ("FORGEJO_TOKEN", token),
        ("FORGEJO_WEBHOOK_SECRET", webhook_secret),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.trim().is_empty().then_some(name))
    .collect();
    if !missing.is_empty() {
        return Err(format!(
            "Forgejo configuration is incomplete; missing: {}",
            missing.join(", ")
        ));
    }
    crate::forgejo::validated_base_url(base_url).map_err(|e| e.to_string())?;
    crate::forgejo::validate_credential("FORGEJO_TOKEN", token).map_err(|e| e.to_string())?;
    crate::forgejo::validate_credential("FORGEJO_WEBHOOK_SECRET", webhook_secret)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod forgejo_config_tests {
    use super::validate_forgejo_values;

    #[test]
    fn forgejo_configuration_is_optional_or_complete() {
        assert!(validate_forgejo_values("", "", "").is_ok());
        assert!(validate_forgejo_values("https://git.example.com", "", "").is_err());
        assert!(
            validate_forgejo_values("https://git.example.com", "test-token", "test-secret").is_ok()
        );
        assert!(validate_forgejo_values("git.example.com", "test-token", "test-secret").is_err());
        assert!(validate_forgejo_values(
            "https://git.example.com/base?x=1",
            "test-token",
            "test-secret"
        )
        .is_err());
    }
}

#[cfg(test)]
mod forgejo_audit_config_tests {
    use super::*;
    #[test]
    fn audit_forgejo_deployment_rejects_short_secrets_and_plaintext() {
        assert!(validate_forgejo_values("https://forgejo.example", "test-token", "short").is_err());
        assert!(
            validate_forgejo_values("http://forgejo.example", "test-token", "long-secret").is_err()
        );
    }
}
