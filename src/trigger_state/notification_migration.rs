//! Version 4 adds installation ownership without changing existing remote markers.
use super::*;

/// Inspect legacy table shape inside the migration transaction.
fn has_installation(db: &Connection, table: &str) -> Result<bool> {
    let mut statement = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|name| name == "installation"))
}

/// A legacy plan has no owner column. Only evidence for its snapshot or exact
/// source event can establish an owner; unrelated PR activity proves nothing.
fn migrate_plans(tx: &Connection) -> Result<()> {
    let plans = tx.prepare("SELECT key,plan FROM opened_plans WHERE json_extract(key,'$[0]') IN ('path-plan','path-event')")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (key, plan) in plans {
        let mut tuple: Vec<Value> = serde_json::from_str(&key)?;
        let snapshot = tuple.first().and_then(Value::as_str) == Some("path-plan");
        let legacy_len = if snapshot { 6 } else { 7 };
        if tuple.len() != legacy_len {
            continue;
        }
        let repo = tuple[1]
            .as_i64()
            .ok_or("invalid legacy path repository ID")?;
        let pr = tuple[2].as_i64().ok_or("invalid legacy path PR ID")?;
        let event = tuple[3].as_str().ok_or("invalid legacy path event")?;
        let (head, base, source_time) = if snapshot {
            (tuple[4].as_str(), tuple[5].as_str(), None)
        } else {
            (
                tuple[5].as_str(),
                tuple[6].as_str(),
                Some(tuple[4].as_str().ok_or("invalid legacy path source time")?),
            )
        };
        let scopes = tx.prepare("WITH evidence(context) AS (
            SELECT json_extract(spec,'$.context') FROM operations
            WHERE ?6 IS NULL AND (json_extract(spec,'$.kind')='path'
                OR json_extract(spec,'$.request.path_notification')=1)
            UNION ALL
            SELECT context FROM inbox WHERE json_extract(context,'$.event') || '.' || json_extract(context,'$.action')=?3
            )
            SELECT DISTINCT json_extract(context,'$.installation_id') FROM evidence
            WHERE json_extract(context,'$.repository_id')=?1 AND json_extract(context,'$.thread_id')=?2
                AND json_extract(context,'$.head_sha') IS ?4 AND json_extract(context,'$.base_sha') IS ?5
                AND (?6 IS NULL OR json_extract(context,'$.source_time')=?6)
                AND json_type(context,'$.installation_id')='integer'
                AND json_extract(context,'$.installation_id')>0 LIMIT 2")?
            .query_map(params![repo,pr,event,head,base,source_time], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if scopes.len() == 1 {
            tuple.push(serde_json::json!(scopes[0]));
            tx.execute(
                "INSERT OR IGNORE INTO opened_plans(key,plan) VALUES(?1,?2)",
                params![serde_json::to_string(&tuple)?, plan],
            )?;
        } else {
            tx.execute("INSERT OR IGNORE INTO path_notification_migration_blocks VALUES(?1,?2,'legacy path plan has no unambiguous installation')",params![repo,pr])?;
            tracing::warn!(repository=repo, pr, "legacy path plan ownership unresolved; restore the scoped notification ledger before CC");
        }
    }
    Ok(())
}

/// Atomically migrate all notification ownership. Legacy imports lack an
/// installation ID, so retain their evidence and require scoped attestation.
pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS path_notification_migration_blocks(
        repository INTEGER NOT NULL, pr INTEGER NOT NULL, reason TEXT NOT NULL,
        PRIMARY KEY(repository,pr));",
    )?;
    let version: i64 = tx.query_row("SELECT version FROM trigger_meta", [], |row| row.get(0))?;
    if !has_installation(&tx, "recipients")? {
        tx.execute_batch("ALTER TABLE recipients RENAME TO legacy_recipients;
            CREATE TABLE recipients(
                installation INTEGER NOT NULL CHECK(installation > 0), repository INTEGER NOT NULL,
                pr INTEGER NOT NULL, login TEXT NOT NULL, operation TEXT NOT NULL REFERENCES operations(key),
                committed INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(installation,repository,pr,login));
            INSERT INTO recipients(installation,repository,pr,login,operation,committed)
                SELECT json_extract(o.spec,'$.context.installation_id'),r.repository,r.pr,r.login,r.operation,r.committed
                FROM legacy_recipients r JOIN operations o ON o.key=r.operation
                WHERE json_type(o.spec,'$.context.installation_id')='integer'
                    AND json_extract(o.spec,'$.context.installation_id')>0
                    AND json_extract(o.spec,'$.context.repository_id')=r.repository
                    AND json_extract(o.spec,'$.context.thread_id')=r.pr;
            INSERT OR IGNORE INTO path_notification_migration_blocks
                SELECT r.repository,r.pr,'legacy recipient installation unknown; complete scoped restoration required'
                FROM legacy_recipients r WHERE NOT EXISTS(
                    SELECT 1 FROM recipients n WHERE n.operation=r.operation AND n.repository=r.repository AND n.pr=r.pr AND n.login=r.login);
        ")?;
    }
    if !has_installation(&tx, "path_notification_ledgers")? {
        tx.execute_batch("ALTER TABLE path_notification_ledgers RENAME TO legacy_path_notification_ledgers;
            CREATE TABLE path_notification_ledgers(
                installation INTEGER NOT NULL CHECK(installation > 0), repository INTEGER NOT NULL,
                pr INTEGER NOT NULL, verified_at INTEGER NOT NULL, evidence TEXT NOT NULL,
                PRIMARY KEY(installation,repository,pr));
            INSERT OR IGNORE INTO path_notification_migration_blocks
                SELECT repository,pr,'legacy completeness evidence lacks installation; scoped restoration required'
                FROM legacy_path_notification_ledgers;")?;
    }
    if version < 4 {
        migrate_plans(&tx)?;
    }
    tx.execute_batch("CREATE INDEX IF NOT EXISTS recipient_operation ON recipients(operation);
        CREATE INDEX IF NOT EXISTS installed_path_plan_scope ON opened_plans(
            json_extract(key,'$[0]'),json_extract(key,'$[1]'),json_extract(key,'$[2]'),json_extract(key,'$[6]'));
        UPDATE trigger_meta SET version=4;")?;
    tx.commit()?;
    Ok(())
}
