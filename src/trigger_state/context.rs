use super::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// An allowlist, never a raw webhook or installation credential. Body is needed
/// to reconstruct parsing; unknown payload fields and tokens are discarded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventContext {
    pub delivery: String,
    pub event: String,
    pub action: String,
    pub repository_id: i64,
    pub installation_id: i64,
    pub repo: String,
    pub thread_id: i64,
    pub number: i64,
    pub is_pr: bool,
    pub comment_id: Option<i64>,
    pub user_id: Option<i64>,
    pub source_time: String,
    pub body: Option<String>,
    pub login: Option<String>,
    pub user_type: Option<String>,
    pub via_app_id: Option<i64>,
    pub author: Option<String>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
}

impl EventContext {
    /// Selection covers inputs shared by #14–#17. Existing rebase/queue/label
    /// routes keep their own switches; no second route for those side effects.
    pub fn capture(event: &str, payload: &Value, delivery: Option<&str>) -> Result<Option<Self>> {
        let action = payload["action"].as_str().unwrap_or("");
        if !matches!(
            (event, action),
            ("issue_comment", "created")
                | ("issues", "opened")
                | ("pull_request", "opened" | "synchronize")
        ) {
            return Ok(None);
        }
        fn id(value: &Value) -> Result<i64> {
            value
                .as_i64()
                .filter(|n| *n > 0)
                .ok_or_else(|| "missing positive GitHub ID".into())
        }
        fn string(value: &Value) -> Option<String> {
            value.as_str().map(str::to_owned)
        }
        let thread = if event == "pull_request" {
            &payload["pull_request"]
        } else {
            &payload["issue"]
        };
        let source = if event == "issue_comment" {
            &payload["comment"]
        } else {
            thread
        };
        let source_time = source[if action == "synchronize" {
            "updated_at"
        } else {
            "created_at"
        }]
        .as_str()
        .ok_or("missing GitHub source time")?
        .to_owned();
        chrono::DateTime::parse_from_rfc3339(&source_time)?;
        let repo = crate::idle_workflows::config::repository_name(
            payload["repository"]["full_name"]
                .as_str()
                .ok_or("missing repository name")?,
        )?;
        let context = Self {
            delivery: delivery
                .filter(|s| !s.is_empty() && s.len() <= 200)
                .ok_or("missing/invalid X-GitHub-Delivery")?
                .into(),
            event: event.into(),
            action: action.into(),
            repository_id: id(&payload["repository"]["id"])?,
            installation_id: id(&payload["installation"]["id"])?,
            repo,
            thread_id: id(&thread["id"])?,
            number: id(&thread["number"])?,
            is_pr: event == "pull_request" || thread.get("pull_request").is_some(),
            comment_id: (event == "issue_comment")
                .then(|| id(&source["id"]))
                .transpose()?,
            user_id: Some(id(&source["user"]["id"])?),
            source_time,
            body: (event == "issue_comment")
                .then(|| crate::redact::scrub(source["body"].as_str().unwrap_or(""))),
            login: string(&source["user"]["login"]),
            user_type: string(&source["user"]["type"]),
            via_app_id: source["performed_via_github_app"]["id"].as_i64(),
            author: string(&thread["user"]["login"]),
            head_sha: string(&thread["head"]["sha"]),
            base_sha: string(&thread["base"]["sha"]),
        };
        Ok(Some(context))
    }

    /// Only the fields consumed by the existing comment parser/router.
    pub fn comment_payload(&self) -> Value {
        let mut issue =
            json!({"id":self.thread_id,"number":self.number,"user":{"login":self.author}});
        if self.is_pr {
            issue["pull_request"] = json!({});
        }
        json!({"action":self.action,"repository":{"id":self.repository_id,"full_name":self.repo},
            "installation":{"id":self.installation_id},"issue":issue,
            "comment":{"id":self.comment_id,"body":self.body,"created_at":self.source_time,
                "user":{"id":self.user_id,"login":self.login,"type":self.user_type},
                "performed_via_github_app":{"id":self.via_app_id}}})
    }
}

/// Consumers record this only after a real explicit mention passes current
/// applicability and authorization checks. No historical scan is performed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWake {
    pub repository_id: i64,
    pub thread_id: i64,
    pub user_id: i64,
    pub comment_id: i64,
    /// GitHub time in milliseconds, not webhook arrival time.
    pub source_at: i64,
}
