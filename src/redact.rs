//! Keep operator secrets out of anything the bot says in public.
//!
//! The bot reports its own failures into the PR thread, and it formats those
//! reports from whatever the provider handed back — which is how a 401 from the
//! AI provider published the endpoint it was talking to:
//!
//! ```text
//! ❌ 审查出错: `AI request failed (401 Unauthorized) to https://api.example.ai/v1/responses: …`
//! ```
//!
//! A self-hosted or relayed AI endpoint is not something a PR reader is
//! entitled to, and `reqwest`'s own transport errors carry the URL too, so the
//! leak has more sources than the one format string. Error text is assembled in
//! a dozen places and every new engine adds more, so the guard sits at the
//! point of publication instead: [`crate::github::Client`] scrubs every body it
//! posts. Fixing an individual message is still worth doing — this is the net
//! under it, not a licence to interpolate secrets.
//!
//! Logs are deliberately *not* scrubbed: they are the operator's own, and the
//! full URL is what makes a misconfiguration diagnosable.

use std::sync::{Mutex, OnceLock};

use crate::config::Config;

/// Registered (secret, placeholder) pairs, longest secret first.
static SECRETS: OnceLock<Mutex<Vec<(String, &'static str)>>> = OnceLock::new();

fn store() -> &'static Mutex<Vec<(String, &'static str)>> {
    SECRETS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Values this short are not redacted.
///
/// A secret is replaced by plain substring match, so a 3-character one would
/// rewrite every innocent occurrence of those characters in a review body. A
/// secret that short is already unprotectable; mangling the review to pretend
/// otherwise helps nobody.
const MIN_SECRET_LEN: usize = 8;

/// Register a config's secrets as unpublishable. Idempotent; call it as often
/// as configs are built (`Config::from_env` does).
pub fn register(cfg: &Config) {
    let base = cfg.ai_base_url.trim().trim_end_matches('/');
    let candidates: Vec<(String, &'static str)> = vec![
        (cfg.ai_api_key.trim().to_string(), "<AI_API_KEY>"),
        (cfg.webhook_secret.trim().to_string(), "<WEBHOOK_SECRET>"),
        (cfg.cron_secret.trim().to_string(), "<CRON_SECRET>"),
        // The full base URL and its host are registered separately: an error
        // may quote either, and the host alone still identifies the provider.
        (base.to_string(), "<AI_ENDPOINT>"),
        (host_of(base).to_string(), "<AI_ENDPOINT>"),
    ];

    let mut secrets = match store().lock() {
        Ok(g) => g,
        // A poisoned lock means a previous caller panicked mid-update. The
        // registry is a plain Vec, so it is still readable and extending it is
        // safe — and refusing to register would fail *open*, publishing the
        // very strings this module exists to hide.
        Err(poisoned) => poisoned.into_inner(),
    };
    for (value, placeholder) in candidates {
        if value.len() < MIN_SECRET_LEN {
            continue;
        }
        if secrets.iter().any(|(v, _)| *v == value) {
            continue;
        }
        secrets.push((value, placeholder));
    }
    // Longest first, so a base URL is replaced as a whole before the host
    // pattern gets to eat the middle of it.
    secrets.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
}

/// Replace every registered secret in `text` with its placeholder, then scrub
/// anything that merely *looks* like a credential.
///
/// The second pass is [`crate::engines_subproc::redact_any`], which needs no
/// registration and so catches what this module can't know about — a git
/// credential quoted back by a subprocess, a token a model copied out of a
/// file. Two mechanisms, one boundary: don't add a third.
pub fn scrub(text: &str) -> String {
    let secrets = match store().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    crate::engines_subproc::redact_any(&apply(text, &secrets))
}

fn apply(text: &str, secrets: &[(String, &'static str)]) -> String {
    let mut out = text.to_string();
    for (value, placeholder) in secrets {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), placeholder);
        }
    }
    out
}

/// Host part of a URL: what's between the scheme and the first `/`, minus any
/// `user:pass@` prefix. Returns "" for anything that doesn't look like a URL,
/// which [`register`] then drops for being too short.
fn host_of(url: &str) -> &str {
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets() -> Vec<(String, &'static str)> {
        let mut s = vec![
            (
                "https://api.example.ai/v1".to_string(),
                "<AI_ENDPOINT>" as &'static str,
            ),
            ("api.example.ai".to_string(), "<AI_ENDPOINT>"),
            ("sk-RELlongenoughtobeasecret".to_string(), "<AI_API_KEY>"),
        ];
        s.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
        s
    }

    /// The comment that started this: a 401 report quoting the endpoint.
    #[test]
    fn endpoint_is_replaced_in_an_error_report() {
        let leak = "❌ 审查出错: `AI request failed (401 Unauthorized) to \
                    https://api.example.ai/v1/responses: {\"message\":\"Invalid token\"}`";
        let out = apply(leak, &secrets());
        assert!(!out.contains("api.example.ai"), "{out}");
        assert!(out.contains("<AI_ENDPOINT>/responses"), "{out}");
        // the useful part of the report survives
        assert!(out.contains("401 Unauthorized"), "{out}");
        assert!(out.contains("Invalid token"), "{out}");
    }

    #[test]
    fn base_url_is_replaced_whole_not_in_pieces() {
        let out = apply("see https://api.example.ai/v1 for details", &secrets());
        assert_eq!(out, "see <AI_ENDPOINT> for details");
    }

    #[test]
    fn bare_host_is_replaced_too() {
        let out = apply("dns lookup for api.example.ai failed", &secrets());
        assert_eq!(out, "dns lookup for <AI_ENDPOINT> failed");
    }

    #[test]
    fn api_key_is_replaced() {
        let out = apply(
            "Authorization: Bearer sk-RELlongenoughtobeasecret",
            &secrets(),
        );
        assert!(!out.contains("sk-REL"), "{out}");
        assert!(out.contains("<AI_API_KEY>"), "{out}");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        let body = "## 🤖 AI Code Review\n\n✅ 未发现问题。";
        assert_eq!(apply(body, &secrets()), body);
    }

    #[test]
    fn short_values_are_not_registered() {
        let cfg = Config {
            ai_api_key: "sk-x".into(),
            ai_base_url: String::new(),
            webhook_secret: String::new(),
            cron_secret: String::new(),
            ..blank_config()
        };
        register(&cfg);
        // "sk-x" is below the floor, so a review mentioning it stays readable
        assert_eq!(scrub("sk-x marks the spot"), "sk-x marks the spot");
    }

    #[test]
    fn host_of_parses_what_it_should() {
        assert_eq!(host_of("https://api.example.ai/v1"), "api.example.ai");
        assert_eq!(
            host_of("http://user:pass@api.example.ai:8080/v1"),
            "api.example.ai:8080"
        );
        assert_eq!(host_of("not a url"), "");
        assert_eq!(host_of(""), "");
    }

    /// A Config with everything empty, for tests that set two or three fields.
    fn blank_config() -> Config {
        Config {
            app_id: String::new(),
            private_key_pem: None,
            webhook_secret: String::new(),
            bot_name: String::new(),
            app_slug: String::new(),
            ai_base_url: String::new(),
            ai_api_key: String::new(),
            ai_model: String::new(),
            api_format: String::new(),
            max_diff_chars: 0,
            review_engine: String::new(),
            agent_max_turns: 0,
            agent_timeout_secs: 0,
            pi_path: String::new(),
            pi_args: String::new(),
            codex_path: String::new(),
            codex_args: String::new(),
            data_dir: String::new(),
            checkout_depth: 0,
            cron_secret: String::new(),
            r_plus_allow_on_behalf: false,
            rebase_check_delay_secs: 0,
            rebase_sweep_enabled: false,
            rebase_sweep_interval_secs: 0,
            label_needs_rebase: String::new(),
            label_waiting_review: String::new(),
            label_waiting_author: String::new(),
            label_blocked: String::new(),
            codeql_label: String::new(),
            port: 0,
        }
    }
}
