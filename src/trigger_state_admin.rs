//! Offline administration: stop the server first so this process can own /data.
use xero_bot::trigger_state::{operation_marker, Store};

/// Run one offline query or evidence-backed state transition under exclusive ownership.
fn run() -> xero_bot::trigger_state::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: trigger-state DATA_DIR list|inbox|show [CURSOR_OR_OPERATION_SHA256]\n       trigger-state DATA_DIR confirm-success|confirm-not-sent|retry OPERATION_SHA256 EVIDENCE [RECEIPT_JSON]\nStop the server first. Unknown writes cannot be blindly retried; confirm-not-sent requires external evidence.";
    if args.len() < 2 {
        return Err(usage.into());
    }
    let store = Store::open(std::path::Path::new(&args[0]))?;
    if args[1] == "inbox" {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &store.inbox_status_page(args.get(2).map(String::as_str), 100)?
            )?
        );
        return Ok(());
    }
    let operations = store.list(None)?;
    if args[1] == "list" {
        for op in operations {
            println!(
                "{}",
                serde_json::json!({"marker":operation_marker(&op.spec.key),"state":op.state,"kind":op.spec.kind,"delivery":op.spec.context.delivery,"detail":op.detail})
            );
        }
        return Ok(());
    }
    let id = args.get(2).ok_or(usage)?;
    let op = operations
        .iter()
        .find(|op| operation_marker(&op.spec.key) == format!("<!-- xero-trigger:{id} -->"))
        .ok_or("operation not found")?;
    if args[1] == "show" {
        println!("{}", serde_json::to_string_pretty(op)?);
        return Ok(());
    }
    let evidence = args.get(3).ok_or(usage)?;
    let receipt = args
        .get(4)
        .map(|text| serde_json::from_str::<serde_json::Value>(text))
        .transpose()?;
    store.administer_with_receipt(
        &op.spec.key,
        &args[1],
        evidence,
        receipt.as_ref(),
        xero_bot::github::chrono_now_secs(),
    )?;
    println!(
        "Recorded {}; restart the server to recheck live policy before any retry.",
        args[1]
    );
    Ok(())
}
/// Report administration errors and exit without starting a server or sending requests.
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(2);
    }
}
