//! Durable, single-owner Forgejo comment inbox. Acknowledgements follow the
//! SQLite commit. Unknown writes are retained for operator reconciliation.
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{ForgejoClient, ForgejoError, ForgejoEvent};
use crate::{config::Config, trigger_state::Store};

type Result<T> = std::result::Result<T, String>;
const MAX_ATTEMPTS: i64 = 5;

pub struct Inbox {
    db: Mutex<Connection>,
    pump: tokio::sync::Mutex<()>,
}

struct Active {
    inbox: Arc<Inbox>,
    key: String,
    scope: String,
    source_at: i64,
    comment_id: i64,
    attempt: i64,
}
tokio::task_local! { static ACTIVE: Active; }

fn failure(e: impl std::fmt::Display) -> String {
    crate::redact::scrub(&e.to_string())
}

impl Inbox {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(failure)?;
        let db = Connection::open(dir.join("forgejo-comments.sqlite")).map_err(failure)?;
        db.busy_timeout(std::time::Duration::from_millis(250))
            .map_err(failure)?;
        db.execute_batch("PRAGMA locking_mode=EXCLUSIVE;
            PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            BEGIN EXCLUSIVE;
            CREATE TABLE IF NOT EXISTS comments(
                key TEXT PRIMARY KEY, provider TEXT NOT NULL, scope TEXT NOT NULL,
                source_at INTEGER NOT NULL, comment_id INTEGER NOT NULL, event TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
                next_at INTEGER NOT NULL DEFAULT 0, wrote INTEGER NOT NULL DEFAULT 0,
                failed INTEGER NOT NULL DEFAULT 0, uncertain INTEGER NOT NULL DEFAULT 0,
                detail TEXT NOT NULL DEFAULT '');
            CREATE INDEX IF NOT EXISTS comment_due ON comments(provider,state,next_at,source_at,comment_id);
            CREATE INDEX IF NOT EXISTS comment_scope ON comments(scope,state);
            CREATE TABLE IF NOT EXISTS statuses(scope TEXT PRIMARY KEY, source_at INTEGER NOT NULL, comment_id INTEGER NOT NULL);
            UPDATE comments SET state=CASE WHEN wrote=0 THEN 'pending' ELSE 'unknown' END,
                detail='interrupted by restart; written requests must be reconciled' WHERE state='running';
            COMMIT;").map_err(failure)?;
        Ok(Self {
            db: Mutex::new(db),
            pump: tokio::sync::Mutex::new(()),
        })
    }

    fn db(&self) -> Result<MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| "Forgejo inbox mutex poisoned".into())
    }

    /// Returns false for a redelivery, including a different delivery UUID for
    /// the same comment. Identity is scoped to the canonical Forgejo API URL.
    pub fn enqueue(&self, cfg: &Config, event: &ForgejoEvent) -> Result<bool> {
        let ForgejoEvent::IssueComment {
            repo,
            number,
            repository_id,
            issue_id,
            comment_id,
            user_id,
            source_at,
            body,
            ..
        } = event
        else {
            return Err("expected Forgejo comment".into());
        };
        if !super::commands::valid_repository(repo) || *number <= 0 || body.len() > 65536 {
            return Err("invalid Forgejo comment repository, number or body length".into());
        }
        let positive = |value: &Option<i64>| {
            value
                .filter(|id| *id > 0)
                .ok_or("missing Forgejo comment identity")
        };
        let repository = positive(repository_id)?;
        let issue = positive(issue_id)?;
        let comment = positive(comment_id)?;
        positive(user_id)?;
        let source_at = source_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis())
            .filter(|t| *t > 0)
            .ok_or("missing Forgejo comment time")?;
        let provider = ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)
            .map_err(failure)?
            .api_base
            .to_string();
        let scope = json!([provider, repository, issue]).to_string();
        let key = hex::encode(Sha256::digest(
            json!([provider, repository, comment])
                .to_string()
                .as_bytes(),
        ));
        let event = serde_json::to_string(event).map_err(failure)?;
        let changed = self.db()?.execute(
            "INSERT OR IGNORE INTO comments(key,provider,scope,source_at,comment_id,event) VALUES(?1,?2,?3,?4,?5,?6)",
            params![key,provider,scope,source_at,comment,event]).map_err(failure)?;
        Ok(changed == 1)
    }

    pub async fn pump(self: &Arc<Self>, cfg: &Config, store: &Store) -> Result<usize> {
        let Ok(_guard) = self.pump.try_lock() else {
            return Ok(0);
        };
        let provider = ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token)
            .map_err(failure)?
            .api_base
            .to_string();
        let mut count = 0;
        // Bound each tick. One consumer serializes session checks and issue
        // read-modify-write operations, including jobs accepted during a tick.
        for _ in 0..100 {
            let row = {
                let mut db = self.db()?;
                let tx = db.transaction().map_err(failure)?;
                let row = tx.query_row("SELECT key,scope,source_at,comment_id,event,attempts FROM comments c
                    WHERE provider=?1 AND state='pending' AND next_at<=?2
                    AND NOT EXISTS(SELECT 1 FROM comments b WHERE b.scope=c.scope AND b.state IN ('unknown','running'))
                    ORDER BY source_at,comment_id LIMIT 1",
                    params![provider,chrono::Utc::now().timestamp_millis()],
                    |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?)))
                    .optional().map_err(failure)?;
                if let Some((key, ..)) = &row {
                    tx.execute(
                        "UPDATE comments SET state='running',attempts=attempts+1 WHERE key=?1",
                        [key],
                    )
                    .map_err(failure)?;
                }
                tx.commit().map_err(failure)?;
                row
            };
            let Some((key, scope, source_at, comment_id, event, attempts)) = row else {
                break;
            };
            let event: ForgejoEvent = serde_json::from_str(&event).map_err(failure)?;
            let active = Active {
                inbox: Arc::clone(self),
                key: key.clone(),
                scope,
                source_at,
                comment_id,
                attempt: attempts + 1,
            };
            let result = ACTIVE
                .scope(
                    active,
                    super::commands::handle_issue_comment(cfg, store, event),
                )
                .await;
            self.finish(&key, result.as_ref().err().map(String::as_str))?;
            count += 1;
        }
        Ok(count)
    }

    fn finish(&self, key: &str, error: Option<&str>) -> Result<()> {
        let db = self.db()?;
        let (wrote, failed, uncertain, attempts): (bool, bool, bool, i64) = db
            .query_row(
                "SELECT wrote,failed,uncertain,attempts FROM comments WHERE key=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(failure)?;
        let state = if uncertain {
            "unknown"
        } else if error.is_some() && !wrote && attempts < MAX_ATTEMPTS {
            "pending"
        } else if error.is_some() || failed {
            "failed"
        } else {
            "succeeded"
        };
        db.execute("UPDATE comments SET state=?2,event=CASE WHEN ?2='succeeded' THEN 'null' ELSE event END,next_at=?3,detail=CASE WHEN ?4<>'' THEN ?4 ELSE detail END WHERE key=?1",
            params![key,state,chrono::Utc::now().timestamp_millis()+5000*(1<<attempts.min(5)),error.map(failure).unwrap_or_default()]).map_err(failure)?;
        if state == "unknown" || state == "failed" {
            tracing::warn!(
                comment_key = key,
                state,
                "Forgejo comment requires attention; inspect trigger-state forgejo-inbox"
            );
        }
        Ok(())
    }

    pub fn status(&self) -> Result<Vec<Value>> {
        let db = self.db()?;
        let mut stmt = db
            .prepare("SELECT key,state,attempts,detail,comment_id,scope FROM comments ORDER BY source_at,comment_id")
            .map_err(failure)?;
        let rows=stmt.query_map([],|r| Ok(json!({"key":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"attempts":r.get::<_,i64>(2)?,"detail":r.get::<_,String>(3)?,"comment_id":r.get::<_,i64>(4)?,"scope":r.get::<_,String>(5)?}))).map_err(failure)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(failure)
    }

    /// An operator must inspect Forgejo before declaring a possibly sent job
    /// done. Retrying is permitted only after proving no write was applied.
    pub fn resolve(&self, key: &str, decision: &str, evidence: &str) -> Result<()> {
        let state = match decision {
            "done" => "succeeded",
            "not-sent" => "pending",
            _ => return Err("expected done or not-sent".into()),
        };
        if evidence.trim().is_empty() {
            return Err("reconciliation evidence is required".into());
        }
        let changed=self.db()?.execute("UPDATE comments SET state=?2,wrote=0,failed=0,uncertain=0,attempts=0,next_at=0,detail=?3 WHERE key=?1 AND state IN ('unknown','failed')",
            params![key,state,format!("operator {decision}: {}",failure(evidence))]).map_err(failure)?;
        if changed != 1 {
            return Err("no failed or unknown Forgejo comment with that key".into());
        }
        Ok(())
    }
}

pub(super) fn before_write() -> std::result::Result<(), ForgejoError> {
    ACTIVE
        .try_with(|a| {
            let db = a.inbox.db()?;
            let changed = db
                .execute(
                    "UPDATE comments SET wrote=1 WHERE key=?1 AND state='running' AND uncertain=0",
                    [&a.key],
                )
                .map_err(failure)?;
            if changed != 1 {
                return Err("Forgejo job has an unknown write; further writes paused".into());
            }
            Ok(())
        })
        .unwrap_or(Ok(()))
        .map_err(ForgejoError::InvalidResponse)
}

pub(super) fn after_write(
    result: &std::result::Result<Value, ForgejoError>,
) -> std::result::Result<(), ForgejoError> {
    let Err(error) = result else { return Ok(()) };
    let uncertain = matches!(
        error,
        ForgejoError::Transport(_)
            | ForgejoError::InvalidResponse(_)
            | ForgejoError::UnknownWriteOutcome(_)
    ) || matches!(error,ForgejoError::Api{status,..} if *status>=500);
    ACTIVE.try_with(|a| {
        a.inbox.db()?.execute("UPDATE comments SET failed=1,uncertain=MAX(uncertain,?2),detail=?3 WHERE key=?1",
            params![a.key,uncertain,failure(error)]).map_err(failure)?;
        Ok(())
    }).unwrap_or(Ok(())).map_err(ForgejoError::InvalidResponse)
}

pub(super) fn can_defer_session() -> bool {
    ACTIVE
        .try_with(|a| a.attempt < MAX_ATTEMPTS)
        .unwrap_or(false)
}

pub(super) fn status_is_current() -> Result<bool> {
    ACTIVE.try_with(|a| {
        let changed=a.inbox.db()?.execute("INSERT INTO statuses(scope,source_at,comment_id) VALUES(?1,?2,?3)
            ON CONFLICT(scope) DO UPDATE SET source_at=excluded.source_at,comment_id=excluded.comment_id
            WHERE (excluded.source_at,excluded.comment_id)>=(statuses.source_at,statuses.comment_id)",
            params![a.scope,a.source_at,a.comment_id]).map_err(failure)?;
        Ok(changed==1)
    }).unwrap_or(Ok(true))
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;
