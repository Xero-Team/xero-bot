//! Regression tests for App private-key loading.
//!
//! Two independent bugs used to produce the same opaque runtime error,
//! `bad PEM key: InvalidKeyFormat`, on every webhook:
//!   1. the signing key was parsed with `from_ec_pem`, which rejects the RSA
//!      keys GitHub actually issues;
//!   2. a blank `PRIVATE_KEY_B64=` counted as set, resolved to an empty key,
//!      and shadowed `PRIVATE_KEY_PATH`.

use std::io::Write;

use xero_bot::config::Config;

/// PKCS#8-wrapped RSA key, test-only. Held in a fixture file rather than a
/// literal so it can't be silently truncated by a bad copy-paste — a short
/// key still fails to parse, which would look like the bug under test.
const TEST_KEY: &str = include_str!("fixtures/rsa_test_key.txt");

/// The key GitHub hands you must parse as RSA. Parsing it as EC — the old
/// behaviour — is what produced `InvalidKeyFormat` on every request.
#[test]
fn app_key_parses_as_rsa_not_ec() {
    assert!(
        jsonwebtoken::EncodingKey::from_rsa_pem(TEST_KEY.as_bytes()).is_ok(),
        "GitHub App RSA key must parse via from_rsa_pem"
    );
    assert!(
        jsonwebtoken::EncodingKey::from_ec_pem(TEST_KEY.as_bytes()).is_err(),
        "an RSA key is not an EC key — from_ec_pem must not be used for App keys"
    );
}

/// `validate()` must reject an unusable key at startup rather than letting
/// each webhook fail in a background task whose error is only logged.
#[test]
fn validate_rejects_unusable_key() {
    let mut cfg = base_cfg();

    cfg.private_key_pem = Some(String::new());
    assert!(cfg.validate().is_err(), "empty key must fail validation");

    cfg.private_key_pem = Some("not a pem at all".into());
    assert!(cfg.validate().is_err(), "garbage key must fail validation");

    cfg.private_key_pem = Some(TEST_KEY.into());
    assert!(cfg.validate().is_ok(), "valid RSA key must pass: {:?}", cfg.validate());
}

/// A blank `PRIVATE_KEY_B64=` left in `.env` must not shadow the path.
///
/// Serialised with the other env-mutating test via a shared lock, since the
/// process environment is global.
#[test]
fn blank_b64_does_not_shadow_key_path() {
    let _guard = env_lock();

    let mut file = tempfile();
    file.write_all(TEST_KEY.as_bytes()).unwrap();
    let path = file.path_string();

    // Blank, whitespace-only, and unset must all fall through to the path.
    for blank in ["", "   "] {
        std::env::set_var("PRIVATE_KEY_B64", blank);
        std::env::set_var("PRIVATE_KEY_PATH", &path);
        let cfg = Config::from_env();
        assert_eq!(
            cfg.private_key_pem.as_deref().map(str::trim),
            Some(TEST_KEY.trim()),
            "blank PRIVATE_KEY_B64 ({blank:?}) must fall through to PRIVATE_KEY_PATH"
        );
    }

    // A non-blank B64 value still takes precedence over the path.
    use base64::Engine;
    std::env::set_var(
        "PRIVATE_KEY_B64",
        base64::engine::general_purpose::STANDARD.encode(TEST_KEY),
    );
    std::env::set_var("PRIVATE_KEY_PATH", "/definitely/not/here.pem");
    let cfg = Config::from_env();
    assert_eq!(
        cfg.private_key_pem.as_deref(),
        Some(TEST_KEY),
        "a set PRIVATE_KEY_B64 must win over PRIVATE_KEY_PATH"
    );

    std::env::remove_var("PRIVATE_KEY_B64");
    std::env::remove_var("PRIVATE_KEY_PATH");
}

/// A path pointing at a missing file yields no key (not an empty one), so
/// validate() reports it as missing config.
#[test]
fn missing_key_file_is_none() {
    let _guard = env_lock();

    std::env::remove_var("PRIVATE_KEY_B64");
    std::env::set_var("PRIVATE_KEY_PATH", "/definitely/not/here.pem");
    let cfg = Config::from_env();
    assert!(cfg.private_key_pem.is_none());
    assert!(cfg.validate().is_err());

    std::env::remove_var("PRIVATE_KEY_PATH");
}

// --- helpers -------------------------------------------------------------

fn base_cfg() -> Config {
    let mut cfg = Config::from_env();
    cfg.app_id = "12345".into();
    cfg.webhook_secret = "whsec".into();
    cfg.bot_name = "xero-review".into();
    cfg.private_key_pem = Some(TEST_KEY.into());
    cfg
}

/// Serialise the env-mutating tests (cargo runs tests in threads).
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Minimal self-cleaning temp file (avoids adding a dev-dependency).
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn path_string(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Write for TempFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut f = std::fs::OpenOptions::new().append(true).open(&self.0)?;
        f.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn tempfile() -> TempFile {
    let mut p = std::env::temp_dir();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    p.push(format!("xero-key-{unique}-{:?}.pem", std::thread::current().id()));
    std::fs::File::create(&p).unwrap();
    TempFile(p)
}
