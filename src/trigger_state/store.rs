use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{retry_delay, EventContext, Result, SessionWake};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
    Superseded,
}
impl State {
    /// Map state variants to their stable SQL representation.
    fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationSpec {
    pub key: String,
    pub parent: Option<String>,
    /// `command`, `comment`, `review`, `ensure_labels`, or `other`.
    pub kind: String,
    pub context: EventContext,
    pub config_sha: Option<String>,
    /// Method, route and scrubbed body for one external write. No headers/token.
    pub request: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub spec: OperationSpec,
    pub state: State,
    pub attempts: u32,
    /// Delivery that owns the current attempt; the original spec remains immutable.
    pub lease_delivery: String,
    pub sent: bool,
    pub result: Option<Value>,
    pub detail: String,
    pub recipients: Vec<String>,
}

/// A fencing token. A late completion from an expired attempt cannot commit.
#[derive(Debug, Clone)]
pub struct Claim {
    pub operation: Operation,
    pub attempt: u32,
}
#[derive(Debug, Clone)]
pub struct InboxItem {
    pub context: EventContext,
    pub attempt: u32,
}

/// Raw SQL fields before fallible JSON decoding into an operation.
struct OperationRow {
    spec: String,
    state: String,
    attempts: u32,
    sent: bool,
    result: Option<String>,
    detail: String,
    lease_delivery: String,
}

pub struct Store(Mutex<Connection>);
impl Store {
    /// SQLite's lifetime exclusive lock is released by the OS even on SIGKILL.
    /// DDL forces lock acquisition before startup succeeds, including on an
    /// already initialized volume. No idle scheduler switch participates here.
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let db = Connection::open(data_dir.join("command-triggers.sqlite"))?;
        db.busy_timeout(std::time::Duration::from_millis(250))?;
        db.execute_batch("PRAGMA locking_mode=EXCLUSIVE;
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=FULL;
            PRAGMA foreign_keys=ON;
            BEGIN EXCLUSIVE;
            CREATE TABLE IF NOT EXISTS trigger_meta(version INTEGER NOT NULL);
            INSERT INTO trigger_meta SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM trigger_meta);
            CREATE TABLE IF NOT EXISTS inbox(
                delivery TEXT PRIMARY KEY, context TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
                next_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL, detail TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS operations(
                key TEXT PRIMARY KEY, parent TEXT, delivery TEXT NOT NULL, spec TEXT NOT NULL,
                lease_delivery TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
                sent INTEGER NOT NULL DEFAULT 0, result TEXT, detail TEXT NOT NULL DEFAULT '',
                next_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS operation_parent ON operations(parent);
            CREATE INDEX IF NOT EXISTS operation_delivery_state ON operations(delivery,state);
            CREATE INDEX IF NOT EXISTS inbox_due ON inbox(state,next_at);
            CREATE INDEX IF NOT EXISTS inbox_retention ON inbox(state,updated_at);
            CREATE INDEX IF NOT EXISTS operation_running_age ON operations(state,updated_at);
            CREATE TABLE IF NOT EXISTS recipients(
                repository INTEGER NOT NULL, pr INTEGER NOT NULL, login TEXT NOT NULL,
                operation TEXT NOT NULL REFERENCES operations(key), committed INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(repository,pr,login)
            );
            CREATE TABLE IF NOT EXISTS sessions(
                repository INTEGER NOT NULL, thread INTEGER NOT NULL, user INTEGER NOT NULL,
                comment INTEGER NOT NULL, source_at INTEGER NOT NULL,
                PRIMARY KEY(repository,thread,user,comment)
            );
            CREATE INDEX IF NOT EXISTS session_source ON sessions(repository,thread,user,source_at,comment);
            CREATE TABLE IF NOT EXISTS admin_audit(
                id INTEGER PRIMARY KEY, operation TEXT NOT NULL, decision TEXT NOT NULL,
                evidence TEXT NOT NULL, created_at INTEGER NOT NULL
            );
            COMMIT;")?;
        let version: i64 = db.query_row("SELECT version FROM trigger_meta", [], |r| r.get(0))?;
        if !matches!(version, 1 | 2) {
            return Err("unsupported trigger database version".into());
        }
        if version == 1 {
            let has_owner = db
                .prepare("PRAGMA table_info(operations)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?
                .iter()
                .any(|name| name == "lease_delivery");
            db.execute_batch("BEGIN IMMEDIATE;")?;
            if !has_owner {
                db.execute_batch(
                    "ALTER TABLE operations ADD COLUMN lease_delivery TEXT NOT NULL DEFAULT '';",
                )?;
            }
            db.execute_batch("UPDATE operations SET lease_delivery=delivery WHERE lease_delivery=''; UPDATE trigger_meta SET version=2; COMMIT;")?;
        }
        db.execute_batch(
            "CREATE INDEX IF NOT EXISTS operation_lease ON operations(lease_delivery,state);",
        )?;
        let store = Self(Mutex::new(db));
        // All attempts from the previous owner have lost their authority. First
        // make uncertainty explicit; only reconciliation can make them runnable.
        store.recover_running(i64::MAX, crate::github::chrono_now_secs())?;
        Ok(store)
    }

    /// Acquire the connection for a short synchronous transaction only.
    fn db(&self) -> Result<MutexGuard<'_, Connection>> {
        self.0
            .lock()
            .map_err(|_| "trigger database mutex poisoned".into())
    }

    /// Commit a recovery envelope once; reject conflicting reuse of a delivery ID.
    pub fn enqueue(&self, context: &EventContext, now: i64) -> Result<bool> {
        let db = self.db()?;
        let text = serde_json::to_string(context)?;
        let added = db.execute(
            "INSERT OR IGNORE INTO inbox(delivery,context,updated_at) VALUES(?1,?2,?3)",
            params![context.delivery, text, now],
        )?;
        if added == 0 {
            let previous: String = db.query_row(
                "SELECT context FROM inbox WHERE delivery=?1",
                [&context.delivery],
                |r| r.get(0),
            )?;
            if previous != text {
                return Err("delivery ID reused with different recovery context".into());
            }
        }
        Ok(added == 1)
    }

    /// Return the first bounded page of inbox diagnostics.
    pub fn inbox_status(&self) -> Result<Vec<Value>> {
        self.inbox_status_page(None, 100)
    }

    /// Page in stable delivery-ID order; pass the last delivery as the cursor.
    pub fn inbox_status_page(&self, after: Option<&str>, limit: usize) -> Result<Vec<Value>> {
        let db = self.db()?;
        let mut stmt = db.prepare("SELECT delivery,state,attempts,next_at,detail FROM inbox WHERE (?1 IS NULL OR delivery>?1) ORDER BY delivery LIMIT ?2")?;
        let rows = stmt.query_map(params![after,limit.clamp(1,500) as i64], |r| Ok(serde_json::json!({"delivery":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"attempts":r.get::<_,u32>(2)?,"next_at":r.get::<_,i64>(3)?,"detail":r.get::<_,String>(4)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        Ok(rows)
    }

    /// Retain 30 days of completed envelopes. Never delete failed/recoverable
    /// work, operations, sessions, or lifetime recipient/budget records.
    pub fn cleanup_inbox(&self, now: i64) -> Result<usize> {
        Ok(self.db()?.execute("DELETE FROM inbox WHERE delivery IN (
            SELECT i.delivery FROM inbox i WHERE i.state='succeeded' AND i.updated_at<?1
            AND NOT EXISTS(SELECT 1 FROM operations o WHERE (o.delivery=i.delivery OR o.lease_delivery=i.delivery)
                AND o.state NOT IN ('succeeded','superseded'))
            ORDER BY i.updated_at,i.delivery LIMIT 500)",[now.saturating_sub(30*86400)])?)
    }

    /// Atomically claim one due envelope without holding a lock over execution.
    pub fn claim_inbox(&self, now: i64) -> Result<Option<InboxItem>> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String,u32)> = tx.query_row("SELECT context,attempts FROM inbox WHERE state='pending' AND next_at<=?1 ORDER BY next_at,delivery LIMIT 1", [now], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((context, attempts)) = row else {
            return Ok(None);
        };
        let context: EventContext = serde_json::from_str(&context)?;
        tx.execute("UPDATE inbox SET state='running',attempts=attempts+1,updated_at=?2 WHERE delivery=?1 AND state='pending'", params![context.delivery,now])?;
        tx.commit()?;
        Ok(Some(InboxItem {
            context,
            attempt: attempts + 1,
        }))
    }

    /// Inbox work is planning only; all side effects must be separate operations.
    /// Replaying a planner after a crash does not replay successful operations.
    pub fn finish_inbox(&self, item: &InboxItem, retry: Option<&str>, now: i64) -> Result<()> {
        let db = self.db()?;
        let count = db.execute("UPDATE inbox SET state=?3,next_at=?4,detail=?5,updated_at=?6 WHERE delivery=?1 AND attempts=?2 AND state='running'",
            params![item.context.delivery,item.attempt,if retry.is_some(){"pending"}else{"succeeded"},now+retry_delay(item.attempt),retry.unwrap_or(""),now])?;
        if count != 1 {
            return Err("stale inbox lease".into());
        }
        Ok(())
    }

    /// Defer a previously interrupted envelope using its fenced inbox attempt.
    pub fn defer_interrupted_inbox(&self, item: &InboxItem, now: i64) -> Result<()> {
        self.db()?.execute("UPDATE inbox SET state='pending',next_at=?3,detail='worker interrupted' WHERE delivery=?1 AND attempts=?2 AND state='unknown'",params![item.context.delivery,item.attempt,now+retry_delay(item.attempt)])?;
        Ok(())
    }

    /// Requeue interrupted planners; their external writes remain independently fenced.
    pub fn resume_inbox(&self, now: i64) -> Result<usize> {
        Ok(self.db()?.execute(
            "UPDATE inbox SET state='pending',next_at=?1 WHERE state='unknown'",
            [now],
        )?)
    }

    /// First mark abandoned/expired claims unknown, never directly pending.
    /// Call a timed recovery only after cancelling the expired worker; fencing
    /// rejects its local commit but cannot cancel an already sent HTTP request.
    pub fn recover_running(&self, before: i64, now: i64) -> Result<usize> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE inbox SET state='unknown',detail='planner interrupted',updated_at=?2 WHERE state='running' AND updated_at<=?1", params![before,now])?;
        let count = tx.execute("UPDATE operations SET state='unknown',detail='owner lost or worker timed out',updated_at=?2 WHERE state='running' AND updated_at<=?1", params![before,now])?;
        tx.commit()?;
        if count > 0 {
            tracing::error!(
                count,
                "trigger operations are unknown; reconcile or use trigger-state admin CLI"
            );
        }
        Ok(count)
    }

    /// Called only after this delivery's future has completed or been dropped.
    /// Other deliveries, including a prior owner of the same business key, are
    /// unaffected. Initial process recovery still uses recover_running.
    pub fn recover_delivery(&self, delivery: &str, now: i64) -> Result<usize> {
        let count = self.db()?.execute("UPDATE operations SET state='unknown',detail='delivery worker ended without a receipt',updated_at=?2 WHERE lease_delivery=?1 AND state='running'",params![delivery,now])?;
        if count > 0 {
            tracing::error!(
                delivery,
                count,
                "interrupted trigger writes require reconciliation"
            );
        }
        Ok(count)
    }

    /// Read the immutable intent, current lease and recipient reservations together.
    fn operation(db: &Connection, key: &str) -> Result<Option<Operation>> {
        let row: Option<OperationRow> = db.query_row(
            "SELECT spec,state,attempts,sent,result,detail,lease_delivery FROM operations WHERE key=?1",
            [key], |r| Ok(OperationRow {spec:r.get(0)?,state:r.get(1)?,attempts:r.get(2)?,sent:r.get(3)?,result:r.get(4)?,detail:r.get(5)?,lease_delivery:r.get(6)?})
        ).optional()?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut stmt =
            db.prepare("SELECT login FROM recipients WHERE operation=?1 ORDER BY login")?;
        let recipients = stmt
            .query_map([key], |r| r.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(Some(Operation {
            spec: serde_json::from_str(&row.spec)?,
            state: serde_json::from_value(Value::String(row.state))?,
            attempts: row.attempts,
            lease_delivery: row.lease_delivery,
            sent: row.sent,
            result: row.result.map(|s| serde_json::from_str(&s)).transpose()?,
            detail: row.detail,
            recipients,
        }))
    }
    /// Look up one business operation without changing its state.
    pub fn get(&self, key: &str) -> Result<Option<Operation>> {
        Self::operation(&*self.db()?, key)
    }

    /// Return operation audit records, optionally restricted to one state.
    pub fn list(&self, state: Option<State>) -> Result<Vec<Operation>> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT key FROM operations WHERE (?1 IS NULL OR state=?1) ORDER BY updated_at,key",
        )?;
        let keys = stmt
            .query_map([state.map(State::name)], |r| r.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        keys.iter()
            .map(|key| Self::operation(&db, key)?.ok_or_else(|| "missing operation".into()))
            .collect()
    }

    /// Discard only unsent descendants of a superseded planner. Uncertain
    /// writes must be reconciled first and retain their notification budget.
    pub fn supersede_unsent_children(&self, parent: &str, now: i64) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE operations SET state='superseded',detail='parent snapshot superseded',updated_at=?2 WHERE parent=?1 AND state='pending' AND sent=0",params![parent,now])?;
        tx.execute("DELETE FROM recipients WHERE committed=0 AND operation IN (SELECT key FROM operations WHERE parent=?1 AND state='superseded' AND sent=0)",[parent])?;
        tx.commit()?;
        Ok(())
    }

    /// Detect work belonging to either this original delivery or its current lease.
    pub fn unresolved(&self, delivery: &str) -> Result<bool> {
        Ok(self.db()?.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE (delivery=?1 OR lease_delivery=?1) AND state IN ('pending','running','unknown'))",[delivery],|r|r.get(0))?)
    }

    /// Read independently checkpointed writes belonging to a command planner.
    pub fn children(&self, parent: &str) -> Result<Vec<Operation>> {
        let db = self.db()?;
        let mut stmt = db.prepare("SELECT key FROM operations WHERE parent=?1 ORDER BY key")?;
        let keys = stmt
            .query_map([parent], |r| r.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        keys.iter()
            .map(|key| Self::operation(&db, key)?.ok_or_else(|| "missing child".into()))
            .collect()
    }

    /// Atomically acquire a pending operation and bind the attempt to this delivery.
    pub fn claim(&self, spec: &OperationSpec, now: i64) -> Result<Option<Claim>> {
        self.claim_with_recipients(spec, &[], None, now)
    }

    /// Claim and PR-wide budget reservation are one transaction. All rules and
    /// heads share this ledger; neither inbox cleanup nor reopen resets it.
    pub fn claim_notification(
        &self,
        spec: &OperationSpec,
        users: &[String],
        max: u8,
        now: i64,
    ) -> Result<Option<Claim>> {
        if max > 10 || spec.kind != "comment" {
            return Err("notification requires a comment and budget <= 10".into());
        }
        self.claim_with_recipients(spec, users, Some(max), now)
    }

    /// Claim an operation and reserve any requested notification budget atomically.
    fn claim_with_recipients(
        &self,
        spec: &OperationSpec,
        users: &[String],
        max: Option<u8>,
        now: i64,
    ) -> Result<Option<Claim>> {
        let mut normalized = std::collections::BTreeSet::new();
        for user in users {
            super::recipient_key(spec.context.repository_id, spec.context.thread_id, user)?;
            normalized.insert(user.to_ascii_lowercase());
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO operations(key,parent,delivery,spec,updated_at) VALUES(?1,?2,?3,?4,?5)",
            params![spec.key, spec.parent, spec.context.delivery, serde_json::to_string(spec)?, now],
        )?;
        let changed = tx.execute("UPDATE operations SET state='running',attempts=attempts+1,sent=0,updated_at=?2,lease_delivery=?3 WHERE key=?1 AND state='pending' AND next_at<=?2", params![spec.key,now,spec.context.delivery])?;
        if changed == 0 {
            tx.commit()?;
            return Ok(None);
        }
        if let Some(max) = max {
            let ctx = &spec.context;
            let count: u32 = tx.query_row(
                "SELECT count(*) FROM recipients WHERE repository=?1 AND pr=?2",
                params![ctx.repository_id, ctx.thread_id],
                |r| r.get(0),
            )?;
            let mut remaining = (max as u32).saturating_sub(count);
            for user in normalized {
                if remaining == 0 {
                    break;
                }
                let inserted = tx.execute("INSERT OR IGNORE INTO recipients(repository,pr,login,operation) VALUES(?1,?2,?3,?4)", params![ctx.repository_id,ctx.thread_id,user,spec.key])?;
                remaining -= inserted as u32;
            }
        }
        let operation = Self::operation(&tx, &spec.key)?.ok_or("claim disappeared")?;
        tx.commit()?;
        Ok(Some(Claim {
            attempt: operation.attempts,
            operation,
        }))
    }

    /// Persist the uncertainty boundary BEFORE touching the network.
    pub fn mark_sent(&self, claim: &Claim, now: i64) -> Result<()> {
        let n = self.db()?.execute("UPDATE operations SET sent=1,updated_at=?3 WHERE key=?1 AND attempts=?2 AND state='running'", params![claim.operation.spec.key,claim.attempt,now])?;
        if n != 1 {
            return Err("stale operation lease before send".into());
        }
        Ok(())
    }

    /// Commit a fenced result and update reservations without releasing unknown writes.
    pub fn finish(
        &self,
        claim: &Claim,
        state: State,
        result: Option<&Value>,
        detail: &str,
        now: i64,
    ) -> Result<()> {
        if !matches!(
            state,
            State::Succeeded | State::Failed | State::Unknown | State::Superseded
        ) {
            return Err("invalid completion state".into());
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if state == State::Superseded
            && Self::operation(&tx, &claim.operation.spec.key)?
                .ok_or("missing operation")?
                .sent
        {
            return Err("sent operation must be reconciled before superseding".into());
        }
        let n = tx.execute("UPDATE operations SET state=?3,result=?4,detail=?5,updated_at=?6 WHERE key=?1 AND attempts=?2 AND state='running'", params![claim.operation.spec.key,claim.attempt,state.name(),result.map(serde_json::to_string).transpose()?,crate::redact::scrub(detail),now])?;
        if n != 1 {
            return Err("stale operation lease at completion".into());
        }
        if state == State::Succeeded {
            tx.execute(
                "UPDATE recipients SET committed=1 WHERE operation=?1",
                [&claim.operation.spec.key],
            )?;
        }
        // Failed is allowed only for a proven rejection/no send; unknown keeps
        // its reservation. Superseding a sent write must first reconcile it.
        if state == State::Failed
            || (state == State::Superseded
                && !Self::operation(&tx, &claim.operation.spec.key)?
                    .ok_or("missing operation")?
                    .sent)
        {
            tx.execute(
                "DELETE FROM recipients WHERE operation=?1 AND committed=0",
                [&claim.operation.spec.key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read-only reconciliation supplies evidence. Absence of a marker is NOT
    /// evidence of no send. Only explicit `not_sent` may release reservations.
    pub fn resolve_unknown(
        &self,
        expected: &Operation,
        outcome: &str,
        result: Option<&Value>,
        evidence: &str,
        now: i64,
    ) -> Result<()> {
        if !matches!(outcome, "succeeded" | "not_sent" | "retry_idempotent") {
            return Err("invalid reconciliation outcome".into());
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = expected.spec.key.as_str();
        let op = Self::operation(&tx, key)?.ok_or("unknown operation key")?;
        if op.attempts != expected.attempts {
            return Err("stale reconciliation attempt".into());
        }
        if expected.state != State::Unknown || op.state != State::Unknown {
            return Err("operation is not unknown".into());
        }
        if outcome == "retry_idempotent" && op.spec.kind != "ensure_labels" {
            return Err("only ensure_labels can retry an uncertain write".into());
        }
        tx.execute("UPDATE operations SET state=?2,sent=?3,result=?4,detail=?5,next_at=?6,updated_at=?7 WHERE key=?1 AND state='unknown'", params![key,if outcome=="succeeded" {"succeeded"}else{"pending"},outcome=="succeeded",result.map(serde_json::to_string).transpose()?,crate::redact::scrub(evidence),if outcome=="retry_idempotent" {now+retry_delay(op.attempts)}else{now},now])?;
        if outcome == "succeeded" {
            tx.execute(
                "UPDATE recipients SET committed=1 WHERE operation=?1",
                [key],
            )?;
        }
        if outcome == "not_sent" {
            tx.execute(
                "DELETE FROM recipients WHERE operation=?1 AND committed=0",
                [key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Offline, exclusive-owner management path. Retry requires recorded human
    /// evidence and is never offered as an automatic response to a timeout.
    pub fn administer(&self, key: &str, decision: &str, evidence: &str, now: i64) -> Result<()> {
        self.administer_with_receipt(key, decision, evidence, None, now)
    }

    /// A confirmed write may need a response (for example the actual assignee
    /// list) to resume its planner truthfully. Only a scrubbed receipt is stored.
    pub fn administer_with_receipt(
        &self,
        key: &str,
        decision: &str,
        evidence: &str,
        receipt: Option<&Value>,
        now: i64,
    ) -> Result<()> {
        if evidence.trim().is_empty() {
            return Err("operator evidence is required".into());
        }
        if !matches!(decision, "confirm-success" | "confirm-not-sent" | "retry") {
            return Err("unknown administrative decision".into());
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let op = Self::operation(&tx, key)?.ok_or("unknown operation key")?;
        if !matches!(op.state, State::Unknown | State::Failed) {
            return Err("only failed/unknown operations can be managed".into());
        }
        if decision == "retry" && op.state == State::Unknown {
            return Err("unknown writes need confirm-success or confirm-not-sent first".into());
        }
        if decision == "confirm-success"
            && op.spec.request["route"]
                .as_str()
                .is_some_and(|route| route.ends_with("/assignees"))
            && receipt.and_then(|r| r["assignees"].as_array()).is_none()
        {
            return Err(
                "confirmed assignment requires a verified assignees response receipt".into(),
            );
        }
        if receipt.is_some() && decision != "confirm-success" {
            return Err("receipt is only valid for confirm-success".into());
        }
        let receipt = receipt.map(super::runtime::receipt);
        let state = if decision == "confirm-success" {
            "succeeded"
        } else {
            "pending"
        };
        tx.execute(
            "UPDATE operations SET state=?2,sent=0,detail=?3,next_at=?4,updated_at=?4,result=CASE WHEN ?2='succeeded' THEN ?5 ELSE NULL END WHERE key=?1",
            params![key, state, crate::redact::scrub(evidence), now,receipt.as_ref().map(serde_json::to_string).transpose()?],
        )?;
        if decision == "confirm-success" {
            tx.execute(
                "UPDATE recipients SET committed=1 WHERE operation=?1",
                [key],
            )?;
        }
        if decision == "confirm-not-sent" {
            tx.execute(
                "DELETE FROM recipients WHERE operation=?1 AND committed=0",
                [key],
            )?;
        }
        tx.execute(
            "INSERT INTO admin_audit(operation,decision,evidence,created_at) VALUES(?1,?2,?3,?4)",
            params![key, decision, crate::redact::scrub(evidence), now],
        )?;
        tx.execute(
            "UPDATE operations SET state='pending',next_at=?2 WHERE key=?1 AND state='failed'",
            params![op.spec.parent, now],
        )?;
        // Replay planning with current config/permissions, not a stored request.
        tx.execute(
            "UPDATE inbox SET state='pending',next_at=?2 WHERE delivery=?1",
            params![op.spec.context.delivery, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Insert an authorized mention source once, so edits cannot renew its source time.
    pub fn record_wake(&self, wake: &SessionWake) -> Result<()> {
        if [
            wake.repository_id,
            wake.thread_id,
            wake.user_id,
            wake.comment_id,
        ]
        .iter()
        .any(|v| *v <= 0)
        {
            return Err("session IDs must be positive".into());
        }
        self.db()?.execute("INSERT OR IGNORE INTO sessions(repository,thread,user,comment,source_at) VALUES(?1,?2,?3,?4,?5)",params![wake.repository_id,wake.thread_id,wake.user_id,wake.comment_id,wake.source_at])?;
        Ok(())
    }

    /// Find an earlier wake in this user/thread scope within the supplied TTL.
    pub fn session_before(&self, call: &SessionWake, ttl_days: u16) -> Result<Option<SessionWake>> {
        if ttl_days == 0 {
            return Ok(None);
        }
        let oldest = call
            .source_at
            .saturating_sub(i64::from(ttl_days) * 86_400_000);
        Ok(self.db()?.query_row("SELECT comment,source_at FROM sessions WHERE repository=?1 AND thread=?2 AND user=?3 AND source_at>?4 AND (source_at<?5 OR (source_at=?5 AND comment<?6)) ORDER BY source_at DESC,comment DESC LIMIT 1",params![call.repository_id,call.thread_id,call.user_id,oldest,call.source_at,call.comment_id],|r| Ok(SessionWake{comment_id:r.get(0)?,source_at:r.get(1)?,..call.clone()})).optional()?)
    }
}
