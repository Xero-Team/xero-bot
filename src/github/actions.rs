//! Actions API operations used by the idle workflow scheduler.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};

use super::{enc_seg, Client, GhError};

#[derive(Debug, Deserialize)]
pub struct Workflow {
    pub id: i64,
    pub path: String,
    pub state: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WorkflowRun {
    pub id: i64,
    pub workflow_id: i64,
    pub head_sha: String,
    pub head_branch: Option<String>,
    pub status: String,
    pub conclusion: Option<String>,
    pub event: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub run_attempt: u32,
    pub actor: Option<Actor>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub login: String,
}

impl WorkflowRun {
    pub fn active(&self) -> bool {
        self.status != "completed"
    }

    pub fn retryable(&self) -> bool {
        self.status == "completed"
            && matches!(
                self.conclusion.as_deref(),
                Some("failure" | "timed_out" | "startup_failure")
            )
    }
}

impl Client {
    pub async fn actions_workflow(&self, repo: &str, workflow: &str) -> Result<Workflow, GhError> {
        let value = self
            .get(&format!(
                "/repos/{repo}/actions/workflows/{}",
                enc_seg(workflow)
            ))
            .await?;
        serde_json::from_value(value).map_err(|e| GhError::BadShape(format!("workflow: {e}")))
    }

    pub async fn actions_run(&self, repo: &str, run: i64) -> Result<WorkflowRun, GhError> {
        let value = self
            .get(&format!("/repos/{repo}/actions/runs/{run}"))
            .await?;
        serde_json::from_value(value).map_err(|e| GhError::BadShape(format!("workflow run: {e}")))
    }

    /// The Actions search API caps filtered searches at 1000 results. An
    /// incomplete answer must not be interpreted as absence of a running build.
    pub async fn actions_runs(
        &self,
        repo: &str,
        workflow: Option<i64>,
        filters: &[(&str, String)],
    ) -> Result<Vec<WorkflowRun>, GhError> {
        let base = match workflow {
            Some(id) => format!("/repos/{repo}/actions/workflows/{id}/runs"),
            None => format!("/repos/{repo}/actions/runs"),
        };
        let query = filters
            .iter()
            .map(|(k, v)| format!("{}={}", enc_seg(k), enc_seg(v)))
            .collect::<Vec<_>>()
            .join("&");
        let mut runs = Vec::new();
        for page in 1..=10 {
            let value = self
                .get(&format!("{base}?per_page=100&page={page}&{query}"))
                .await?;
            let total = value["total_count"]
                .as_u64()
                .ok_or_else(|| GhError::BadShape("missing workflow run total_count".into()))?;
            if total > 1000 {
                return Err(GhError::BadShape(
                    "Actions search exceeds 1000 results; cannot establish complete run history"
                        .into(),
                ));
            }
            let batch: Vec<WorkflowRun> = serde_json::from_value(value["workflow_runs"].clone())
                .map_err(|e| GhError::BadShape(format!("workflow runs: {e}")))?;
            let count = batch.len();
            runs.extend(batch);
            if runs.len() as u64 >= total {
                return Ok(runs);
            }
            if count < 100 {
                return Err(GhError::BadShape("incomplete Actions search page".into()));
            }
        }
        Err(GhError::BadShape("incomplete Actions search".into()))
    }

    pub async fn actions_dispatch(
        &self,
        repo: &str,
        workflow: i64,
        branch: &str,
        inputs: &BTreeMap<String, Value>,
    ) -> Result<Option<i64>, GhError> {
        // On API 2022-11-28 this opts into the 200 response containing the ID.
        // Older servers may still return 204; the scheduler reconciles it.
        let value = self
            .actions_post(
                &format!("/repos/{repo}/actions/workflows/{workflow}/dispatches"),
                json!({"ref": branch, "inputs": inputs, "return_run_details": true}),
            )
            .await?;
        Ok(value["workflow_run_id"].as_i64())
    }

    pub async fn actions_rerun(&self, repo: &str, run: i64) -> Result<(), GhError> {
        self.actions_post(
            &format!("/repos/{repo}/actions/runs/{run}/rerun"),
            json!({}),
        )
        .await?;
        Ok(())
    }

    /// Rerun returns an empty 201; dispatch may return 200 JSON or empty 204.
    /// Do not deserialize an empty success as an error, and never replay POSTs
    /// in transport middleware (the shared client_builder disables retries).
    async fn actions_post(&self, route: &str, body: Value) -> Result<Value, GhError> {
        use http_body_util::BodyExt;
        let request = http::Request::builder()
            .method("POST")
            .uri(route)
            .header("Accept", "application/vnd.github+json")
            .header("Content-Type", "application/json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .body(serde_json::to_vec(&body).map_err(|e| GhError::BadShape(e.to_string()))?)
            .map_err(|e| GhError::BadShape(e.to_string()))?;
        let response = self
            .crab
            .execute(request)
            .await
            .map_err(super::classify_octo_error)?;
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| GhError::BadShape(e.to_string()))?
            .to_bytes();
        if !(200..300).contains(&status) {
            // Response bodies can echo workflow inputs. Report only the status.
            return Err(GhError::Api {
                status,
                message: "Actions request rejected".into(),
            });
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| GhError::BadShape(e.to_string()))
    }

    pub async fn actions_activity_snapshot(
        &self,
        repo: &str,
    ) -> Result<BTreeMap<String, String>, GhError> {
        let mut snapshot = BTreeMap::new();
        for branch in self
            .get_all(&format!("/repos/{repo}/branches?per_page=100"))
            .await?
        {
            let name = branch["name"]
                .as_str()
                .ok_or_else(|| GhError::BadShape("branch name missing".into()))?;
            let sha = branch["commit"]["sha"]
                .as_str()
                .ok_or_else(|| GhError::BadShape("branch SHA missing".into()))?;
            snapshot.insert(format!("branch:{name}"), sha.into());
        }
        for pr in self.open_prs(repo).await? {
            let number = pr["number"]
                .as_i64()
                .ok_or_else(|| GhError::BadShape("PR number missing".into()))?;
            let sha = pr["head"]["sha"]
                .as_str()
                .ok_or_else(|| GhError::BadShape("PR head SHA missing".into()))?;
            snapshot.insert(format!("pr:{number}"), sha.into());
        }
        Ok(snapshot)
    }
}
