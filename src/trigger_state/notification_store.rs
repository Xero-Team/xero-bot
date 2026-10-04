//! Lifetime ledger integrity and fenced notification body preparation.
use super::*;

impl Store {
    /// A newly created database proves nothing about PRs predating it. An old
    /// PR needs a restored backup or an explicit, evidence-backed complete ledger.
    pub fn notification_ledger_ready(
        &self,
        installation: i64,
        repository: i64,
        pr: i64,
        created_at: i64,
    ) -> Result<bool> {
        if installation <= 0 || repository <= 0 || pr <= 0 {
            return Err(
                "notification scope requires positive installation/repository/PR IDs".into(),
            );
        }
        Ok(self.db()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM path_notification_ledgers WHERE installation=?1 AND repository=?2 AND pr=?3)
            OR (?4 > initialized_at AND NOT EXISTS(SELECT 1 FROM path_notification_migration_blocks WHERE repository=?2 AND pr=?3))
            FROM path_notification_epoch WHERE singleton=1",
            params![installation,repository,pr,created_at], |r| r.get(0))?)
    }

    /// Offline attestation of a COMPLETE ledger, including deleted comments and
    /// uncertain sends. Import is additive: it never releases existing recipients.
    pub fn restore_notification_ledger(
        &self,
        installation: i64,
        repository: i64,
        pr: i64,
        users: &[String],
        evidence: &str,
        now: i64,
    ) -> Result<()> {
        if installation <= 0
            || repository <= 0
            || pr <= 0
            || evidence.trim().is_empty()
            || users.len() > 10
        {
            return Err(
                "positive installation/repository/PR IDs, <=10 logins and completeness evidence required".into(),
            );
        }
        let mut logins = std::collections::BTreeSet::new();
        for user in users {
            super::super::recipient_key(installation, repository, pr, user)?;
            logins.insert(user.to_ascii_lowercase());
        }
        let evidence = crate::redact::scrub(evidence);
        let key = serde_json::json!(["path-cc-restored", repository, pr, installation]).to_string();
        let context = EventContext {
            delivery: key.clone(),
            event: "ledger_restore".into(),
            action: "restore".into(),
            repository_id: repository,
            installation_id: installation,
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
            tx.execute("INSERT INTO recipients(installation,repository,pr,login,operation,committed) VALUES(?1,?2,?3,?4,?5,1) ON CONFLICT(installation,repository,pr,login) DO UPDATE SET operation=excluded.operation,committed=1",
                params![installation,repository,pr,login,key])?;
        }
        tx.execute("INSERT INTO path_notification_ledgers VALUES(?1,?2,?3,?4,?5) ON CONFLICT(installation,repository,pr) DO UPDATE SET verified_at=excluded.verified_at,evidence=excluded.evidence",
            params![installation,repository,pr,now,evidence])?;
        tx.execute("INSERT INTO admin_audit(operation,decision,evidence,created_at) VALUES(?1,'restore-complete-notification-ledger',?2,?3)",params![key,evidence,now])?;
        tx.commit()?;
        Ok(())
    }

    /// New operations include installation in their key. Reuse an owned legacy
    /// key during migration so its remote marker and receipt remain stable.
    pub(in crate::trigger_state) fn path_operation_key(
        &self,
        ctx: &EventContext,
        legacy: &str,
    ) -> Result<String> {
        let mut tuple: Vec<Value> = serde_json::from_str(legacy)?;
        tuple.push(serde_json::json!(ctx.installation_id));
        let scoped = serde_json::to_string(&tuple)?;
        let db = self.db()?;
        if Self::operation(&db, &scoped)?.is_some() {
            return Ok(scoped);
        }
        if let Some(old) = Self::operation(&db, legacy)? {
            let owner = &old.spec.context;
            if (owner.installation_id, owner.repository_id, owner.thread_id)
                == (ctx.installation_id, ctx.repository_id, ctx.thread_id)
            {
                return Ok(legacy.into());
            }
        }
        Ok(scoped)
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
