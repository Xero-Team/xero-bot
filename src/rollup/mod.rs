//! A durable, SHA-bound merge queue. GitHub is the sole integration writer;
//! authenticated Forgejo commands enter through an explicitly bound identity.
use crate::config::repository::{Domain, Problem, ReasonCode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

pub mod engine;
mod git;
pub mod ingress;
#[cfg(test)]
mod tests;
pub mod web;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const INTERNAL_PREFIX: &str = "automation/xero/";
pub fn internal_branch(branch: &str) -> bool {
    branch.starts_with(INTERNAL_PREFIX)
}
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub enabled: bool,
    pub public: bool,
    pub max_batch: usize,
    pub collect_seconds: i64,
    pub timeout_seconds: i64,
    pub required_checks: Vec<String>,
    pub forgejo_required_checks: Vec<String>,
    pub fallback_after_seconds: i64,
    /// Explicit Forgejo login -> GitHub login mapping, declared on BOTH sides.
    pub reviewers: BTreeMap<String, String>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            public: false,
            max_batch: 4,
            collect_seconds: 60,
            timeout_seconds: 7200,
            required_checks: vec![],
            forgejo_required_checks: vec![],
            fallback_after_seconds: 900,
            reviewers: BTreeMap::new(),
        }
    }
}
impl Policy {
    pub fn validate(self) -> Domain<Self> {
        if !(1..=16).contains(&self.max_batch)
            || !(0..=600).contains(&self.collect_seconds)
            || !(60..=86400).contains(&self.timeout_seconds)
            || !(60..=86400).contains(&self.fallback_after_seconds)
            || (self.enabled
                && (self.required_checks.is_empty() || self.forgejo_required_checks.is_empty()))
            || self
                .required_checks
                .iter()
                .any(|s| s.trim().is_empty() || s.len() > 200)
            || self
                .forgejo_required_checks
                .iter()
                .any(|s| s.trim().is_empty() || s.len() > 200)
            || self.reviewers.iter().any(|(a, b)| {
                !crate::commands::is_valid_login(a) || !crate::commands::is_valid_login(b)
            })
        {
            return Err(Problem::new(ReasonCode::InvalidRule, "invalid rollup policy: explicit required_checks and valid reviewer identities are required"));
        }
        let mut identities = std::collections::HashSet::new();
        let mut locals = std::collections::HashSet::new();
        if self.reviewers.iter().any(|(a, b)| {
            !identities.insert(b.to_ascii_lowercase()) || !locals.insert(a.to_ascii_lowercase())
        }) {
            return Err(Problem::new(
                ReasonCode::InvalidRule,
                "rollup reviewer mapping must be one-to-one",
            ));
        }
        Ok(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Origin {
    pub provider: String,
    pub repo: String,
    pub number: i64,
    pub login: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub number: i64,
    pub head: String,
    pub title: String,
    pub approved_by: String,
    pub origin: Origin,
    pub rollup: bool,
    pub try_only: bool,
    pub created: i64,
    pub decision_time: i64,
    pub decision_comment: i64,
    pub decision_provider: String,
    pub state: String,
    pub detail: String,
    pub batch: Option<u64>,
}
impl Entry {
    pub fn waiting(&self) -> bool {
        self.state == "queued" || self.state == "waiting"
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub id: u64,
    pub members: Vec<String>,
    pub heads: Vec<String>,
    pub approvals: Vec<Entry>,
    pub base: String,
    pub base_branch: String,
    pub branch: String,
    pub tip: String,
    pub state: String,
    pub created: i64,
    pub finished: Option<i64>,
    pub detail: String,
    pub try_only: bool,
    pub audit_pr: Option<i64>,
    pub audit_attempted: bool,
    pub cleaned: bool,
    pub github_ci: String,
    pub forgejo_ci: String,
    pub fallback_started: Option<i64>,
    pub passed_on: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub name: String,
    pub id: i64,
    pub installation: i64,
    pub entries: Vec<Entry>,
    pub batches: Vec<Batch>,
    pub active: Option<u64>,
    pub split: Vec<Vec<String>>,
    pub sequence: u64,
    pub commands: Vec<String>,
    pub public: bool,
    pub checked: i64,
    pub next_attempt: i64,
    pub failures: u32,
    pub detail: String,
}
impl Repository {
    pub fn new(name: &str, id: i64, installation: i64) -> Self {
        Self {
            name: name.into(),
            id,
            installation,
            entries: vec![],
            batches: vec![],
            active: None,
            split: vec![],
            sequence: 0,
            commands: vec![],
            public: false,
            checked: 0,
            next_attempt: 0,
            failures: 0,
            detail: String::new(),
        }
    }
    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.key == key)
    }
    pub fn remember(&mut self, key: String) {
        self.commands.push(key);
        if self.commands.len() > 10000 {
            self.commands.drain(..1000);
        }
    }
}
pub async fn lock(repo: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let lock = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("queue lock registry")
        .entry(repo.to_ascii_lowercase())
        .or_default()
        .clone();
    lock.lock_owned().await
}
