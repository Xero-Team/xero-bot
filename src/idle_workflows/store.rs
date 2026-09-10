use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::Result;

/// Small, synchronous transactions only. Never hold this mutex over an await.
pub struct Store(Mutex<Connection>);

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RepositoryState {
    pub last_activity: i64,
    pub last_scan: i64,
    pub config_hash: String,
    pub snapshot: BTreeMap<String, String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TargetState {
    pub builds: BTreeMap<String, Build>,
    pub pending: Option<Pending>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Build {
    pub terminal: Option<String>,
    pub attempts: u32,
    pub run_id: Option<i64>,
    pub last_attempt_at: i64,
}

/// Written *before* a request. A crash/timeout must leave an uncertain request,
/// not turn it back into a fresh dispatch on the next tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub sha: String,
    pub run_id: Option<i64>,
    pub previous_run_attempt: u32,
    pub sent_at: i64,
    pub known_run_ids: Vec<i64>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_millis(250))?;
        // One scheduler owns this database for its lifetime, including between
        // transactions. A second process sharing /data fails to open it rather
        // than racing a dispatch. Separate copies of /data are not coordinated.
        db.execute_batch(
            "PRAGMA locking_mode=EXCLUSIVE;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS scheduler_state (
                 key TEXT PRIMARY KEY, value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS scheduler_deliveries (
                 id TEXT PRIMARY KEY, received_at INTEGER NOT NULL
             );",
        )?;
        Ok(Self(Mutex::new(db)))
    }

    fn read<T: DeserializeOwned + Default>(db: &Connection, key: &str) -> Result<T> {
        let text: Option<String> = db
            .query_row(
                "SELECT value FROM scheduler_state WHERE key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        match text {
            Some(text) => Ok(serde_json::from_str(&text)?),
            None => Ok(T::default()),
        }
    }

    fn write<T: Serialize>(db: &Connection, key: &str, value: &T) -> Result<()> {
        db.execute(
            "INSERT INTO scheduler_state(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    fn repo_key(repo: &str) -> String {
        format!("repository:{}", repo.to_ascii_lowercase())
    }

    pub fn target_key(repo: &str, workflow: i64, branch: &str) -> String {
        // Names containing punctuation cannot collide with a delimiter.
        serde_json::to_string(&(repo.to_ascii_lowercase(), workflow, branch)).unwrap()
    }

    pub fn target(&self, key: &str) -> Result<TargetState> {
        let db = self
            .0
            .lock()
            .map_err(|_| "scheduler database mutex poisoned")?;
        Self::read(&db, key)
    }

    pub fn save_target(&self, key: &str, target: &TargetState) -> Result<()> {
        let db = self
            .0
            .lock()
            .map_err(|_| "scheduler database mutex poisoned")?;
        Self::write(&db, key, target)
    }

    pub fn activity(&self, repo: &str, now: i64, delivery: Option<&str>) -> Result<()> {
        let mut db = self
            .0
            .lock()
            .map_err(|_| "scheduler database mutex poisoned")?;
        let tx = db.transaction()?;
        if let Some(delivery) = delivery {
            let added = tx.execute(
                "INSERT OR IGNORE INTO scheduler_deliveries(id, received_at) VALUES (?1, ?2)",
                params![delivery, now],
            )?;
            if added == 0 {
                return Ok(());
            }
        }
        let key = Self::repo_key(repo);
        let mut state: RepositoryState = Self::read(&tx, &key)?;
        state.last_activity = state.last_activity.max(now);
        Self::write(&tx, &key, &state)?;
        tx.execute(
            "DELETE FROM scheduler_deliveries WHERE received_at < ?1",
            [now - 7 * 86400],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn configure(&self, repo: &str, hash: &str, now: i64) -> Result<()> {
        let db = self
            .0
            .lock()
            .map_err(|_| "scheduler database mutex poisoned")?;
        let key = Self::repo_key(repo);
        let mut state: RepositoryState = Self::read(&db, &key)?;
        if state.config_hash != hash {
            state.config_hash = hash.into();
            state.last_activity = state.last_activity.max(now);
            Self::write(&db, &key, &state)?;
        }
        Ok(())
    }

    pub fn observe(
        &self,
        repo: &str,
        snapshot: BTreeMap<String, String>,
        now: i64,
        max_gap: i64,
    ) -> Result<()> {
        let db = self
            .0
            .lock()
            .map_err(|_| "scheduler database mutex poisoned")?;
        let key = Self::repo_key(repo);
        let mut state: RepositoryState = Self::read(&db, &key)?;
        let changed = snapshot
            .iter()
            .any(|(k, v)| state.snapshot.get(k) != Some(v));
        // Ignore PR/branch deletion alone. Merges change a branch tip and the
        // merged PR webhook also records activity. Comments/reviews never enter
        // this snapshot; updated_at would incorrectly reset the timer for them.
        if changed || state.last_scan == 0 || now.saturating_sub(state.last_scan) > max_gap {
            state.last_activity = state.last_activity.max(now);
        }
        state.snapshot = snapshot;
        state.last_scan = now;
        Self::write(&db, &key, &state)
    }

    pub fn idle(&self, repos: &[String], now: i64, boot: i64, idle_secs: i64) -> Result<bool> {
        let db = self
            .0
            .lock()
            .map_err(|_| "scheduler database mutex poisoned")?;
        for repo in repos {
            let state: RepositoryState = Self::read(&db, &Self::repo_key(repo))?;
            if state.last_scan == 0 || now.saturating_sub(state.last_activity.max(boot)) < idle_secs
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn observe_branch(&self, repo: &str, branch: &str, sha: &str, now: i64) -> Result<()> {
        let db = self
            .0
            .lock()
            .map_err(|_| "scheduler database mutex poisoned")?;
        let key = Self::repo_key(repo);
        let mut state: RepositoryState = Self::read(&db, &key)?;
        let branch_key = format!("branch:{branch}");
        if state.snapshot.get(&branch_key).map(String::as_str) != Some(sha) {
            state.snapshot.insert(branch_key, sha.into());
            state.last_activity = state.last_activity.max(now);
            Self::write(&db, &key, &state)?;
        }
        Ok(())
    }
}
