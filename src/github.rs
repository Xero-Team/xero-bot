//! GitHub API access via octocrab.
//!
//! One `Client` wraps an installation-scoped octocrab instance (token caching
//! handled by octocrab). Typed methods where convenient, generic
//! routes elsewhere (inline review comments, collaborator permission,
//! code-scanning alerts, diff fetching).

use octocrab::models::{AppId, InstallationId};
use octocrab::{Octocrab, OctocrabBuilder};
use serde_json::{json, Value};

use crate::config::Config;

pub struct Client {
    pub crab: Octocrab,
    /// bot login for filtering own reviews (e.g. "xero-review[bot]")
    pub app_slug: String,
}

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("http error: {0}")]
    Http(#[from] octocrab::Error),
    #[error("github api returned {status}: {message}")]
    Api { status: u16, message: String },
    #[error("bad response shape: {0}")]
    BadShape(String),
}

/// Convert an octocrab error into an Api error carrying the HTTP status when
/// the failure came from GitHub (404/403 detection relies on this).
fn classify_octo_error(e: octocrab::Error) -> GhError {
    if let octocrab::Error::GitHub { source, .. } = &e {
        // try to extract the status code from the inner GitHubError
        return GhError::Api {
            status: 0,
            message: source.message.clone(),
        };
    }
    GhError::Http(e)
}

fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Client {
    /// Build an app-level client (for listing installations).
    pub fn app_client(cfg: &Config) -> Result<Octocrab, GhError> {
        let key = jsonwebtoken::EncodingKey::from_ec_pem(
            cfg.pem().map_err(GhError::BadShape)?.as_bytes(),
        )
        .map_err(|e| GhError::BadShape(format!("bad PEM key: {e}")))?;
        let app_id: u64 = cfg
            .app_id
            .parse()
            .map_err(|_| GhError::BadShape(format!("bad APP_ID: {}", cfg.app_id)))?;
        Ok(OctocrabBuilder::new().app(AppId(app_id), key).build()?)
    }

    /// Build an installation-scoped client.
    pub fn installation(
        cfg: &Config,
        installation_id: i64,
        app_slug: &str,
    ) -> Result<Client, GhError> {
        let app = Self::app_client(cfg)?;
        let crab = app.installation(InstallationId(installation_id as u64))?;
        Ok(Client {
            crab,
            app_slug: app_slug.to_string(),
        })
    }

    // -------------------------------------------------------------------
    // Generic routes
    // -------------------------------------------------------------------

    pub async fn get(&self, route: &str) -> Result<Value, GhError> {
        self.crab
            .get(route, None::<&()>)
            .await
            .map_err(classify_octo_error)
    }

    pub async fn post(&self, route: &str, body: Option<Value>) -> Result<Value, GhError> {
        self.crab
            .post(route, body.as_ref())
            .await
            .map_err(classify_octo_error)
    }

    pub async fn patch(&self, route: &str, body: Option<Value>) -> Result<Value, GhError> {
        self.crab
            .patch(route, body.as_ref())
            .await
            .map_err(classify_octo_error)
    }

    pub async fn put(&self, route: &str, body: Option<Value>) -> Result<Value, GhError> {
        self.crab
            .put(route, body.as_ref())
            .await
            .map_err(classify_octo_error)
    }

    pub async fn delete(&self, route: &str) -> Result<(), GhError> {
        self.crab
            .delete::<Value, _, _>(route, None::<&Value>)
            .await
            .map(|_| ())
            .map_err(classify_octo_error)
    }

    /// DELETE with a JSON body (remove assignees).
    pub async fn delete_with_body(&self, route: &str, body: Value) -> Result<(), GhError> {
        self.crab
            .delete::<Value, _, _>(route, Some(&body))
            .await
            .map(|_| ())
            .map_err(classify_octo_error)
    }

    /// GET returning raw text with a custom Accept header (diffs).
    pub async fn get_raw(&self, route: &str, accept: &str) -> Result<String, GhError> {
        use http_body_util::BodyExt;
        let uri: http::Uri = route
            .parse()
            .map_err(|_| GhError::BadShape(format!("bad route: {route}")))?;
        let req = http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("Accept", accept)
            .body(Vec::new())
            .map_err(|e| GhError::BadShape(e.to_string()))?;
        let resp = self.crab.execute(req).await.map_err(classify_octo_error)?;
        let status = resp.status().as_u16();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| GhError::BadShape(e.to_string()))?
            .to_bytes();
        let text = String::from_utf8_lossy(&body).into_owned();
        if status >= 200 && status < 300 {
            Ok(text)
        } else {
            Err(GhError::Api {
                status,
                message: text.chars().take(300).collect(),
            })
        }
    }

    // -------------------------------------------------------------------
    // Comments / labels / assignees
    // -------------------------------------------------------------------

    pub async fn post_issue_comment(
        &self,
        repo: &str,
        issue: i64,
        body: &str,
    ) -> Result<(), GhError> {
        self.post(
            &format!("/repos/{repo}/issues/{issue}/comments"),
            Some(json!({"body": body})),
        )
        .await?;
        Ok(())
    }

    pub async fn list_labels(&self, repo: &str, issue: i64) -> Result<Vec<String>, GhError> {
        let v = self
            .get(&format!("/repos/{repo}/issues/{issue}/labels"))
            .await?;
        Ok(v.as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn add_labels(
        &self,
        repo: &str,
        issue: i64,
        labels: &[String],
    ) -> Result<(), GhError> {
        self.post(
            &format!("/repos/{repo}/issues/{issue}/labels"),
            Some(json!({ "labels": labels })),
        )
        .await?;
        Ok(())
    }

    pub async fn remove_label(&self, repo: &str, issue: i64, label: &str) -> Result<(), GhError> {
        self.delete(&format!("/repos/{repo}/issues/{issue}/labels/{label}"))
            .await
    }

    pub async fn add_assignees(
        &self,
        repo: &str,
        issue: i64,
        users: &[String],
    ) -> Result<(), GhError> {
        self.post(
            &format!("/repos/{repo}/issues/{issue}/assignees"),
            Some(json!({ "assignees": users })),
        )
        .await?;
        Ok(())
    }

    pub async fn remove_assignees(
        &self,
        repo: &str,
        issue: i64,
        users: &[String],
    ) -> Result<(), GhError> {
        self.delete_with_body(
            &format!("/repos/{repo}/issues/{issue}/assignees"),
            json!({ "assignees": users }),
        )
        .await
    }

    /// GET /repos/{repo}/collaborators/{user}/permission → permission field.
    pub async fn collaborator_permission(&self, repo: &str, user: &str) -> Result<String, GhError> {
        let v = self
            .get(&format!("/repos/{repo}/collaborators/{user}/permission"))
            .await?;
        v.get("permission")
            .and_then(|p| p.as_str())
            .map(String::from)
            .ok_or_else(|| GhError::BadShape("no permission field".into()))
    }

    // -------------------------------------------------------------------
    // Pull requests
    // -------------------------------------------------------------------

    pub async fn get_pr(&self, repo: &str, number: i64) -> Result<Value, GhError> {
        self.get(&format!("/repos/{repo}/pulls/{number}")).await
    }

    /// unified diff text
    pub async fn get_pr_diff(&self, repo: &str, number: i64) -> Result<String, GhError> {
        self.get_raw(
            &format!("/repos/{repo}/pulls/{number}"),
            "application/vnd.github.v3.diff",
        )
        .await
    }

    /// files changed in the PR: [{filename, status, additions, ...}]
    pub async fn list_pr_files(&self, repo: &str, number: i64) -> Result<Vec<Value>, GhError> {
        let v = self
            .get(&format!("/repos/{repo}/pulls/{number}/files?per_page=100"))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// reviews on a PR
    pub async fn list_pr_reviews(&self, repo: &str, number: i64) -> Result<Vec<Value>, GhError> {
        let v = self
            .get(&format!(
                "/repos/{repo}/pulls/{number}/reviews?per_page=100"
            ))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// previous reviews left by this bot (incremental review memory)
    pub async fn own_previous_reviews(
        &self,
        repo: &str,
        number: i64,
    ) -> Result<Vec<Value>, GhError> {
        let reviews = self.list_pr_reviews(repo, number).await?;
        let slug = self.app_slug.to_lowercase();
        Ok(reviews
            .into_iter()
            .filter(|r| {
                r.get("user")
                    .and_then(|u| u.get("login"))
                    .and_then(|l| l.as_str())
                    .map(|l| l.to_lowercase() == slug)
                    .unwrap_or(false)
            })
            .collect())
    }

    /// commits on the PR (for incremental review: what's new since last review)
    pub async fn list_pr_commits(&self, repo: &str, number: i64) -> Result<Vec<Value>, GhError> {
        let v = self
            .get(&format!(
                "/repos/{repo}/pulls/{number}/commits?per_page=100"
            ))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// post a review (COMMENT event) with optional inline comments;
    /// falls back to no-inline retry and finally a plain comment (Python bot behavior)
    pub async fn post_review(
        &self,
        repo: &str,
        number: i64,
        body: &str,
        inline: Vec<Value>,
    ) -> Result<(), GhError> {
        let route = format!("/repos/{repo}/pulls/{number}/reviews");
        let payload = json!({
            "body": body,
            "event": "COMMENT",
            "comments": inline,
        });
        match self.post(&route, Some(payload.clone())).await {
            Ok(_) => return Ok(()),
            Err(e) => tracing::warn!("review post failed: {e}"),
        }
        if !inline.is_empty() {
            let payload = json!({
                "body": body,
                "event": "COMMENT",
                "comments": [],
            });
            match self.post(&route, Some(payload)).await {
                Ok(_) => return Ok(()),
                Err(e) => tracing::warn!("review post (no inline) failed: {e}"),
            }
        }
        // last resort: plain issue comment
        self.post_issue_comment(repo, number, body).await
    }

    /// post an APPROVE review on behalf of a human (r+ command)
    pub async fn post_approve_review(
        &self,
        repo: &str,
        number: i64,
        body: &str,
    ) -> Result<Value, GhError> {
        self.post(
            &format!("/repos/{repo}/pulls/{number}/reviews"),
            Some(json!({"body": body, "event": "APPROVE"})),
        )
        .await
    }

    /// dismiss a review by id (for r-)
    pub async fn dismiss_review(
        &self,
        repo: &str,
        number: i64,
        review_id: i64,
        message: &str,
    ) -> Result<(), GhError> {
        self.put(
            &format!("/repos/{repo}/pulls/{number}/reviews/{review_id}/dismissals"),
            Some(json!({"message": message})),
        )
        .await?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // Repo content (agent tools)
    // -------------------------------------------------------------------

    /// file content at ref (decoded from base64), or None if it's a directory
    pub async fn get_file_content(
        &self,
        repo: &str,
        path: &str,
        reference: &str,
    ) -> Result<Option<String>, GhError> {
        let route = format!("/repos/{repo}/contents/{path}?ref={reference}");
        let v = self.get(&route).await?;
        if v.get("type").and_then(|t| t.as_str()) == Some("dir") {
            return Ok(None);
        }
        let b64 = v
            .get("content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| GhError::BadShape("no content".into()))?;
        let b64 = b64.replace('\n', "");
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| GhError::BadShape(format!("bad base64: {e}")))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// directory listing at ref: [{name, type, path}]
    pub async fn list_dir(
        &self,
        repo: &str,
        path: &str,
        reference: &str,
    ) -> Result<Vec<Value>, GhError> {
        let route = if path.is_empty() {
            format!("/repos/{repo}/contents?ref={reference}")
        } else {
            format!("/repos/{repo}/contents/{path}?ref={reference}")
        };
        let v = self.get(&route).await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    // -------------------------------------------------------------------
    // Code scanning (CodeQL reports)
    // -------------------------------------------------------------------

    /// open code-scanning alerts; Err(Api{403/404}) when not enabled
    pub async fn code_scanning_alerts(&self, repo: &str) -> Result<Vec<Value>, GhError> {
        let v = self
            .get(&format!(
                "/repos/{repo}/code-scanning/alerts?state=open&per_page=100"
            ))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    // -------------------------------------------------------------------
    // Installations / repositories (sweep)
    // -------------------------------------------------------------------

    /// All installation repositories reachable via an installation client
    /// (GET /installation/repositories), first page (100 repos).
    pub async fn installation_repositories_via(client: &Client) -> Result<Vec<String>, GhError> {
        let v: Value = client
            .crab
            .get("/installation/repositories?per_page=100", None::<&()>)
            .await?;
        Ok(v.get("repositories")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        r.get("full_name")
                            .and_then(|f| f.as_str())
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// All open PRs for a repo.
    pub async fn open_prs(&self, repo: &str) -> Result<Vec<Value>, GhError> {
        let v = self
            .get(&format!("/repos/{repo}/pulls?state=open&per_page=100"))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }
}
