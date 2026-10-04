//! Durable trigger primitives. One process owns the volume; SQLite and GitHub
//! cannot commit atomically. Unknown non-idempotent writes require reconciliation.
mod context;
pub(crate) mod runtime;
mod store;

pub use context::{EventContext, SessionWake};
pub use runtime::{operation_marker, reconcile, Recovery, Runtime};
pub use store::{Claim, InboxItem, Operation, OperationSpec, State, Store};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Backoff is bounded even after a long configuration outage.
pub fn retry_delay(attempt: u32) -> i64 {
    (5_i64 * (1_i64 << attempt.min(10))).min(3600)
}

/// JSON tuple encoding avoids delimiter collisions. Parameters are semantic,
/// not source spelling; login lists are case folded, sorted and deduplicated.
pub fn manual_key(repository: i64, comment: i64, command: &crate::commands::Command) -> String {
    use crate::commands::Command::*;
    let mut args = serde_json::json!({});
    match command {
        RequestReview { user } | Assign { user } => {
            args = serde_json::json!(user.to_ascii_lowercase())
        }
        Cc { users } => {
            let mut users: Vec<_> = users.iter().map(|u| u.to_ascii_lowercase()).collect();
            users.sort();
            users.dedup();
            args = serde_json::json!(users);
        }
        Approve { on_behalf_of } => {
            args = serde_json::json!(on_behalf_of.as_ref().map(|u| u.to_ascii_lowercase()))
        }
        Label { add, remove } => {
            let mut add = add.clone();
            let mut remove = remove.clone();
            add.sort();
            add.dedup();
            remove.sort();
            remove.dedup();
            args = serde_json::json!([add, remove]);
        }
        _ => {}
    }
    serde_json::json!(["manual", repository, comment, command.id().name(), args]).to_string()
}

/// Key an opened rule independently of delivery, configuration, and head SHA.
pub fn opened_key(repository: i64, thread: i64, rule: &str) -> String {
    serde_json::json!(["opened", repository, thread, rule]).to_string()
}

/// Key a normalized recipient within one installation/repository/PR lifetime.
pub fn recipient_key(installation: i64, repository: i64, pr: i64, login: &str) -> Result<String> {
    if installation <= 0 || repository <= 0 || pr <= 0 {
        return Err("notification scope requires positive installation/repository/PR IDs".into());
    }
    if !crate::commands::is_valid_login(login) {
        return Err("notification recipient must be a personal GitHub login".into());
    }
    Ok(serde_json::json!([
        "recipient",
        installation,
        repository,
        pr,
        login.to_ascii_lowercase()
    ])
    .to_string())
}

#[cfg(test)]
mod tests;
