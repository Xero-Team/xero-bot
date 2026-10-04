//! Lifetime ledger integrity and fenced notification body preparation.
use super::*;

impl Store {
    /// A newly created database proves nothing about PRs predating it. An old
    /// PR needs a restored backup or an explicit, evidence-backed complete ledger.
    pub fn notification_ledger_ready(
        &self,
        repository: i64,
        pr: i64,
        created_at: i64,
    ) -> Result<bool> {
        Ok(self.db()?.query_row(
            "SELECT ?3 > initialized_at OR EXISTS(SELECT 1 FROM path_notification_ledgers WHERE repository=?1 AND pr=?2) FROM path_notification_epoch WHERE singleton=1",
            params![repository,pr,created_at], |r| r.get(0))?)
    }

    /// Offline attestation of a COMPLETE ledger, including deleted comments and
    /// uncertain sends. Import is additive: it never releases existing recipients.
    pub fn restore_notification_ledger(
        &self,
        repository: i64,
        pr: i64,
        users: &[String],
        evidence: &str,
        now: i64,
    ) -> Result<()> {
        if repository <= 0 || pr <= 0 || evidence.trim().is_empty() || users.len() > 10 {
            return Err(
                "positive repository/PR IDs, <=10 logins and completeness evidence required".into(),
            );
        }
        let mut logins = std::collections::BTreeSet::new();
        for user in users {
            super::super::recipient_key(repository, pr, user)?;
            logins.insert(user.to_ascii_lowercase());
        }
        let evidence = crate::redact::scrub(evidence);
        let key = serde_json::json!(["path-cc-restored", repository, pr]).to_string();
        let context = EventContext {
            delivery: key.clone(),
            event: "ledger_restore".into(),
            action: "restore".into(),
            repository_id: repository,
            installation_id: 0,
            repo: String::new(),
            thread_id: pr,
            number: 0,
            is_pr: true,
            comment_id: None,
            user_id: None,
            source_time: now.to_string(),
            body: None,
            login: None,
            user_type: None,
            via_app_id: None,
            author: None,
            head_sha: None,
            base_sha: None,
        };
        let spec = OperationSpec {
            key: key.clone(),
            parent: None,
            kind: "notification_ledger".into(),
            context,
            config_sha: None,
            request: serde_json::json!({}),
        };
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT OR IGNORE INTO operations(key,delivery,spec,state,updated_at) VALUES(?1,?1,?2,'succeeded',?3)",
            params![key,serde_json::to_string(&spec)?,now])?;
        for login in logins {
            tx.execute("INSERT INTO recipients(repository,pr,login,operation,committed) VALUES(?1,?2,?3,?4,1) ON CONFLICT(repository,pr,login) DO UPDATE SET operation=excluded.operation,committed=1",
                params![repository,pr,login,key])?;
        }
        tx.execute("INSERT INTO path_notification_ledgers VALUES(?1,?2,?3,?4) ON CONFLICT(repository,pr) DO UPDATE SET verified_at=excluded.verified_at,evidence=excluded.evidence",
            params![repository,pr,now,evidence])?;
        tx.execute("INSERT INTO admin_audit(operation,decision,evidence,created_at) VALUES(?1,'restore-complete-notification-ledger',?2,?3)",params![key,evidence,now])?;
        tx.commit()?;
        Ok(())
    }

    /// Only a fenced, definitely unsent attempt may render a body from its
    /// newly reserved recipients. A retry cannot replay mentions whose slots
    /// were released and since taken by a different plan.
    pub(in crate::trigger_state) fn prepare_notification(
        &self,
        claim: &Claim,
        body: &str,
        now: i64,
    ) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut op =
            Self::operation(&tx, &claim.operation.spec.key)?.ok_or("missing notification")?;
        if op.state != State::Running
            || op.sent
            || op.attempts != claim.attempt
            || op.spec.kind != "comment"
        {
            return Err("stale notification body lease".into());
        }
        op.spec.request["body"] = serde_json::json!({"body":body});
        tx.execute(
            "UPDATE operations SET spec=?2,updated_at=?3 WHERE key=?1",
            params![op.spec.key, serde_json::to_string(&op.spec)?, now],
        )?;
        tx.commit()?;
        Ok(())
    }
}
