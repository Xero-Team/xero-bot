//! Real process boundaries: no destructor runs at the injected crash points.
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use xero_bot::trigger_state::{EventContext, OperationSpec, State, Store};

/// Build a complete event for the child process crash protocol.
fn context() -> EventContext {
    EventContext::capture("issue_comment",&json!({"action":"created","repository":{"id":1,"full_name":"test/repo"},"installation":{"id":2},"issue":{"id":3,"number":1,"user":{"login":"alice"}},"comment":{"id":4,"body":"@bot ping","created_at":"2026-10-02T00:00:00Z","user":{"id":5,"login":"alice","type":"User"}}}),Some("crash-delivery")).unwrap().unwrap()
}
/// Create the operation whose durability is checked after abnormal process exit.
fn spec() -> OperationSpec {
    OperationSpec {
        key: "crash-action".into(),
        parent: None,
        kind: "comment".into(),
        context: context(),
        config_sha: Some("config".into()),
        request: json!({}),
    }
}
struct Dir(PathBuf);
impl Dir {
    /// Allocate a unique directory shared only with this test child.
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "xero-trigger-process-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    /// Remove the fixture volume after the child has exited.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Exit without destructors at the requested persistence boundary, or hold the volume lock.
#[test]
fn crash_child() {
    let Ok(dir) = std::env::var("XERO_TEST_CRASH_DIR") else {
        return;
    };
    let point = std::env::var("XERO_TEST_CRASH_POINT").unwrap();
    let store = Store::open(std::path::Path::new(&dir)).unwrap();
    if point == "hold" {
        std::fs::write(std::path::Path::new(&dir).join("ready"), b"ready").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    if point == "before-inbox" {
        std::process::exit(91);
    }
    store.enqueue(&context(), 100).unwrap();
    if point == "after-inbox" {
        std::process::exit(91);
    }
    let _inbox = store.claim_inbox(100).unwrap().unwrap();
    let claim = store.claim(&spec(), 100).unwrap().unwrap();
    if point == "after-claim" {
        std::process::exit(91);
    }
    store.mark_sent(&claim, 100).unwrap();
    if point == "remote-success" {
        std::fs::write(std::path::Path::new(&dir).join("remote-result"), b"100").unwrap();
    }
    std::process::exit(91);
}
/// Launch only the crash helper test with an explicit fixture directory and fault point.
fn child(dir: &Dir, point: &str) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", "crash_child", "--nocapture"])
        .env("XERO_TEST_CRASH_DIR", &dir.0)
        .env("XERO_TEST_CRASH_POINT", point)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

/// Check database evidence after real process termination at each crash boundary.
#[test]
fn crash_inbox_claim_send_and_remote_success_leave_durable_evidence() {
    for point in [
        "before-inbox",
        "after-inbox",
        "after-claim",
        "after-send",
        "remote-success",
    ] {
        let dir = Dir::new();
        assert_eq!(child(&dir, point).status().unwrap().code(), Some(91));
        let store = Store::open(&dir.0).unwrap();
        let inbox = store.inbox_status().unwrap();
        if point == "before-inbox" {
            assert!(inbox.is_empty());
            continue;
        }
        assert_eq!(inbox.len(), 1);
        if point == "after-inbox" {
            assert_eq!(inbox[0]["state"], "pending");
            continue;
        }
        assert_eq!(inbox[0]["state"], "unknown");
        let op = store.get("crash-action").unwrap().unwrap();
        assert_eq!(op.state, State::Unknown);
        assert_eq!(op.sent, point != "after-claim");
        assert!(store.claim(&spec(), 200).unwrap().is_none());
        if point == "remote-success" {
            assert!(dir.0.join("remote-result").exists());
        }
    }
}

/// Prove exclusive ownership across processes and automatic lock release on SIGKILL.
#[test]
fn second_process_refuses_same_volume_and_sigkill_releases_ownership() {
    let dir = Dir::new();
    let mut holder = child(&dir, "hold").spawn().unwrap();
    for _ in 0..300 {
        if dir.0.join("ready").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let ready = dir.0.join("ready").exists();
    let attempt = if ready {
        Some(
            Command::new(env!("CARGO_BIN_EXE_trigger-state"))
                .arg(&dir.0)
                .arg("list")
                .output()
                .unwrap(),
        )
    } else {
        None
    };
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(ready, "child did not acquire lock");
    let attempt = attempt.unwrap();
    assert_eq!(attempt.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&attempt.stderr).contains("locked"));
    assert!(Store::open(&dir.0).is_ok());
}

/// Exercise the packaged admin binary against a stopped process and persist its decision.
#[test]
fn offline_admin_cli_lists_and_records_explicit_confirmation() {
    let dir = Dir::new();
    let store = Store::open(&dir.0).unwrap();
    let claim = store.claim(&spec(), 100).unwrap().unwrap();
    store.mark_sent(&claim, 100).unwrap();
    drop(store);
    let output = Command::new(env!("CARGO_BIN_EXE_trigger-state"))
        .arg(&dir.0)
        .arg("list")
        .output()
        .unwrap();
    assert!(output.status.success());
    let row: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(row["state"], "unknown");
    let marker = row["marker"].as_str().unwrap();
    let id = marker
        .strip_prefix("<!-- xero-trigger:")
        .unwrap()
        .strip_suffix(" -->")
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_trigger-state"))
        .arg(&dir.0)
        .args([
            "confirm-not-sent",
            id,
            "operator verified no upstream request in proxy trace",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&dir.0).unwrap();
    assert_eq!(
        store.get("crash-action").unwrap().unwrap().state,
        State::Pending
    );
}

/// The offline restore command requires installation identity, scopes its
/// completeness proof, and never accepts the old ambiguous argument layout.
#[test]
fn offline_notification_restore_requires_and_persists_installation() {
    let dir = Dir::new();
    let old = Command::new(env!("CARGO_BIN_EXE_trigger-state"))
        .arg(&dir.0)
        .args(["restore-notification-ledger", "1", "3", "[]", "old syntax"])
        .output()
        .unwrap();
    assert_eq!(old.status.code(), Some(2));
    let output = Command::new(env!("CARGO_BIN_EXE_trigger-state"))
        .arg(&dir.0)
        .args([
            "restore-notification-ledger",
            "2",
            "1",
            "3",
            "[\"Alice\"]",
            "complete install 2 evidence",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&dir.0).unwrap();
    assert!(store.notification_ledger_ready(2, 1, 3, 0).unwrap());
    assert!(!store.notification_ledger_ready(4, 1, 3, 0).unwrap());
    let op = store.list(None).unwrap().remove(0);
    assert_eq!(op.spec.context.installation_id, 2);
    assert_eq!(op.recipients, vec!["alice"]);
}
