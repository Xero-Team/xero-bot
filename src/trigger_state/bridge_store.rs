//! Durable object mappings and non-idempotent bridge write reservations.
use super::*;

/// Bridge mapping primitives are additive and do not affect trigger lifecycle.
impl Store {
    /// Return the mapped peer number for one side of a bridge object.
    pub fn bridge_peer(
        &self,
        scope: &str,
        kind: &str,
        side: &str,
        number: i64,
    ) -> Result<Option<i64>> {
        if scope.is_empty() || number <= 0 || !matches!(kind, "issue" | "pull") {
            return Err("invalid bridge object identity".into());
        }
        Ok(self
            .db()?
            .query_row(
                "SELECT peer FROM forgejo_bridge_objects WHERE scope=?1 AND kind=?2 AND side=?3 AND number=?4",
                params![scope, kind, side, number],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Persist both directions atomically after a peer has been created.
    #[allow(clippy::too_many_arguments)]
    pub fn bridge_link(
        &self,
        scope: &str,
        kind: &str,
        source_side: &str,
        source: i64,
        peer_side: &str,
        peer: i64,
        revision: &str,
        now: i64,
    ) -> Result<()> {
        if scope.is_empty()
            || source <= 0
            || peer <= 0
            || !matches!(kind, "issue" | "pull")
            || !matches!(source_side, "github" | "forgejo")
            || !matches!(peer_side, "github" | "forgejo")
            || source_side == peer_side
        {
            return Err("invalid bridge link".into());
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (side, number, other) in [(source_side, source, peer), (peer_side, peer, source)] {
            let previous: Option<i64> = tx
                .query_row(
                    "SELECT peer FROM forgejo_bridge_objects WHERE scope=?1 AND kind=?2 AND side=?3 AND number=?4",
                    params![scope, kind, side, number],
                    |row| row.get(0),
                )
                .optional()?;
            if previous.is_some_and(|current| current != other) {
                return Err("bridge object is already linked to another peer".into());
            }
            tx.execute(
                "INSERT INTO forgejo_bridge_objects(scope,kind,side,number,peer,revision,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(scope,kind,side,number) DO UPDATE SET revision=excluded.revision,updated_at=excluded.updated_at",
                params![scope, kind, side, number, other, revision, now],
            )?;
        }
        tx.execute("DELETE FROM forgejo_bridge_creates WHERE scope=?1 AND kind=?2 AND side=?3 AND number=?4",
            params![scope, kind, source_side, source])?;
        tx.commit()?;
        Ok(())
    }

    /// Return the recorded source revision for echo/idempotency checks.
    pub fn bridge_revision(
        &self,
        scope: &str,
        kind: &str,
        side: &str,
        number: i64,
    ) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT revision FROM forgejo_bridge_objects WHERE scope=?1 AND kind=?2 AND side=?3 AND number=?4",
                params![scope, kind, side, number],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Find an already-relayed comment and reserve the source comment identity.
    /// The peer comment ID is filled after a successful write; a crash after the
    /// write therefore leaves a recoverable row rather than a blind duplicate.
    pub fn bridge_comment_begin(
        &self,
        scope: &str,
        side: &str,
        source_id: i64,
        peer_item: i64,
        now: i64,
    ) -> Result<bool> {
        if scope.is_empty()
            || source_id <= 0
            || peer_item <= 0
            || !matches!(side, "github" | "forgejo")
        {
            return Err("invalid bridge comment identity".into());
        }
        Ok(self.db()?.execute(
            "INSERT OR IGNORE INTO forgejo_bridge_comments(scope,side,source_id,peer_item,updated_at) VALUES(?1,?2,?3,?4,?5)",
            params![scope, side, source_id, peer_item, now],
        )? == 1)
    }

    pub fn bridge_comment_finish(
        &self,
        scope: &str,
        side: &str,
        source_id: i64,
        peer_comment: i64,
        now: i64,
    ) -> Result<()> {
        let changed = self.db()?.execute(
            "UPDATE forgejo_bridge_comments SET peer_comment=?4,updated_at=?5 WHERE scope=?1 AND side=?2 AND source_id=?3",
            params![scope, side, source_id, peer_comment, now],
        )?;
        if changed != 1 {
            return Err("bridge comment mapping is missing".into());
        }
        Ok(())
    }
}

impl Store {
    /// Reserve before sending a create. A missing receipt after restart is an
    /// unknown outcome, never permission to repeat the POST.
    pub fn bridge_create_begin(
        &self,
        scope: &str,
        kind: &str,
        side: &str,
        number: i64,
    ) -> Result<bool> {
        if scope.is_empty()
            || number <= 0
            || !matches!(kind, "issue" | "pull")
            || !matches!(side, "github" | "forgejo")
        {
            return Err("invalid bridge create identity".into());
        }
        Ok(self.db()?.execute(
            "INSERT OR IGNORE INTO forgejo_bridge_creates(scope,kind,side,number,updated_at) VALUES(?1,?2,?3,?4,?5)",
            params![scope,kind,side,number,crate::github::chrono_now_secs()],
        )? == 1)
    }

    pub fn bridge_comment_status(
        &self,
        scope: &str,
        side: &str,
        source_id: i64,
    ) -> Result<Option<Option<i64>>> {
        Ok(self.db()?.query_row(
            "SELECT peer_comment FROM forgejo_bridge_comments WHERE scope=?1 AND side=?2 AND source_id=?3",
            params![scope,side,source_id], |row| row.get(0),
        ).optional()?)
    }

    /// Offline inspection exposes identities and receipts, never user bodies.
    pub fn bridge_status(&self) -> Result<Vec<Value>> {
        let db = self.db()?;
        let mut result = Vec::new();
        let mut creates = db.prepare(
            "SELECT scope,kind,side,number FROM forgejo_bridge_creates ORDER BY updated_at",
        )?;
        for row in creates.query_map([], |row| {
            Ok(serde_json::json!({
                "scope":row.get::<_,String>(0)?, "kind":row.get::<_,String>(1)?,
                "side":row.get::<_,String>(2)?, "source":row.get::<_,i64>(3)?, "state":"unknown"
            }))
        })? {
            result.push(row?);
        }
        let mut comments = db.prepare("SELECT scope,side,source_id,peer_item FROM forgejo_bridge_comments WHERE peer_comment IS NULL ORDER BY updated_at")?;
        for row in comments.query_map([], |row| {
            Ok(serde_json::json!({
                "scope":row.get::<_,String>(0)?, "kind":"comment", "side":row.get::<_,String>(1)?,
                "source":row.get::<_,i64>(2)?, "peer_item":row.get::<_,i64>(3)?, "state":"unknown"
            }))
        })? {
            result.push(row?);
        }
        Ok(result)
    }

    /// `peer=0` is an operator-confirmed non-write. Positive peer records the
    /// remotely verified object/comment receipt, without issuing any request.
    #[allow(clippy::too_many_arguments)]
    pub fn bridge_resolve(
        &self,
        scope: &str,
        kind: &str,
        side: &str,
        source: i64,
        peer: i64,
        evidence: &str,
    ) -> Result<()> {
        if evidence.trim().len() < 8
            || peer < 0
            || source <= 0
            || !matches!(side, "github" | "forgejo")
            || !matches!(kind, "issue" | "pull" | "comment")
        {
            return Err(
                "bridge resolution needs a valid identity, peer >= 0 and external evidence".into(),
            );
        }
        let now = crate::github::chrono_now_secs();
        if kind == "comment" {
            let mut db = self.db()?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = if peer == 0 {
                tx.execute("DELETE FROM forgejo_bridge_comments WHERE scope=?1 AND side=?2 AND source_id=?3 AND peer_comment IS NULL",params![scope,side,source])?
            } else {
                tx.execute("UPDATE forgejo_bridge_comments SET peer_comment=?4,updated_at=?5 WHERE scope=?1 AND side=?2 AND source_id=?3 AND peer_comment IS NULL",params![scope,side,source,peer,now])?
            };
            if changed != 1 {
                return Err("unknown bridge comment not found".into());
            }
            tx.execute("INSERT INTO admin_audit(operation,decision,evidence,created_at) VALUES(?1,?2,?3,?4)",params![serde_json::json!([scope,kind,side,source]).to_string(),format!("bridge-peer:{peer}"),crate::redact::scrub(evidence),now])?;
            tx.commit()?;
        } else {
            // The CLI holds Store's process-wide exclusive lock throughout.
            let exists: bool = self.db()?.query_row("SELECT EXISTS(SELECT 1 FROM forgejo_bridge_creates WHERE scope=?1 AND kind=?2 AND side=?3 AND number=?4)",params![scope,kind,side,source], |r| r.get(0))?;
            if !exists {
                return Err("unknown bridge create not found".into());
            }
            if peer > 0 {
                self.bridge_link(
                    scope,
                    kind,
                    side,
                    source,
                    if side == "github" {
                        "forgejo"
                    } else {
                        "github"
                    },
                    peer,
                    "",
                    now,
                )?;
            } else {
                self.db()?.execute("DELETE FROM forgejo_bridge_creates WHERE scope=?1 AND kind=?2 AND side=?3 AND number=?4",params![scope,kind,side,source])?;
            }
            self.db()?.execute("INSERT INTO admin_audit(operation,decision,evidence,created_at) VALUES(?1,?2,?3,?4)",params![serde_json::json!([scope,kind,side,source]).to_string(),format!("bridge-peer:{peer}"),crate::redact::scrub(evidence),now])?;
        }
        Ok(())
    }
}

impl Store {
    pub fn bridge_enqueue(&self, key: &str, lane: &str, payload: &Value) -> Result<bool> {
        Ok(self.db()?.execute(
            "INSERT OR IGNORE INTO forgejo_bridge_deliveries(key,lane,payload) VALUES(?1,?2,?3)",
            params![key, lane, serde_json::to_string(payload)?],
        )? == 1)
    }
    pub fn bridge_claim_delivery(&self) -> Result<Option<(String, Value)>> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String,String)> = tx.query_row("SELECT key,payload FROM forgejo_bridge_deliveries d WHERE state='pending' AND NOT EXISTS(SELECT 1 FROM forgejo_bridge_deliveries b WHERE b.lane=d.lane AND b.state IN ('blocked','running')) ORDER BY rowid LIMIT 1",[], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((key, payload)) = row else {
            return Ok(None);
        };
        let value = serde_json::from_str(&payload)?;
        tx.execute(
            "UPDATE forgejo_bridge_deliveries SET state='running' WHERE key=?1",
            [&key],
        )?;
        tx.commit()?;
        Ok(Some((key, value)))
    }
    pub fn bridge_finish_delivery(&self, key: &str, done: bool, detail: &str) -> Result<()> {
        let changed = self.db()?.execute("UPDATE forgejo_bridge_deliveries SET state=?2,detail=?3,payload=CASE WHEN ?2='done' THEN '{}' ELSE payload END WHERE key=?1 AND state='running'",params![key,if done {"done"} else {"blocked"},crate::redact::scrub(detail)])?;
        if changed != 1 {
            return Err("bridge delivery is not running".into());
        }
        Ok(())
    }
    pub fn bridge_deliveries(&self) -> Result<Vec<Value>> {
        let db = self.db()?;
        let mut stmt = db.prepare("SELECT key,lane,state,detail FROM forgejo_bridge_deliveries WHERE state != 'done' ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| Ok(serde_json::json!({"key":r.get::<_,String>(0)?,"lane":r.get::<_,String>(1)?,"state":r.get::<_,String>(2)?,"detail":r.get::<_,String>(3)?})))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn bridge_retry_delivery(&self, key: &str) -> Result<()> {
        let changed = self.db()?.execute(
            "UPDATE forgejo_bridge_deliveries SET state='pending' WHERE key=?1 AND state='blocked'",
            [key],
        )?;
        if changed != 1 {
            return Err("blocked bridge delivery not found".into());
        }
        // Create/comment reservations remain intact. A retry never authorizes
        // another uncertain non-idempotent write.
        Ok(())
    }
}
