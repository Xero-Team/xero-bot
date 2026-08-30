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
        return GhError::Api {
            status: source.status_code.as_u16(),
            message: source.message.clone(),
        };
    }
    GhError::Http(e)
}

/// RFC 3986 unreserved set — everything else in a path gets percent-encoded.
/// Deliberately a deny-by-default set: an allow-list of "characters that seem
/// fine" is how escaping code gets it wrong, and this crate already carries one
/// hand-rolled encoder (`agent.rs::urlencode`, kept because GitHub's search
/// syntax needs `+`/`:`/`"` to survive).
const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Same, but `/` survives as the path separator.
const UNRESERVED_PATH: &percent_encoding::AsciiSet = &UNRESERVED.remove(b'/');

/// Percent-encode one path segment. `/` is encoded too, so a label named
/// `needs/rebase` addresses one segment instead of splitting into two.
fn enc_seg(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, UNRESERVED).to_string()
}

/// Percent-encode a repo-relative path, keeping `/` as the separator.
///
/// The failure this prevents is a path containing `?` ending the path early —
/// see [`Client::get_file_content`]. Non-ASCII is a weaker case than it looks:
/// `http::Uri` accepts high bytes, so `文档/说明.md` reached the wire as raw
/// UTF-8 in the request line. That isn't valid HTTP and only worked because
/// the server tolerated it; encoding makes it correct rather than lucky.
fn enc_path(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, UNRESERVED_PATH).to_string()
}

/// GET every page of a paginated route, on any octocrab instance.
///
/// `Page<Value>` deserializes both a bare JSON array and GitHub's wrapped
/// shapes (`{"repositories": [...]}`, `{"installations": [...]}`), so one
/// helper covers every paginated endpoint here. Errors go through
/// [`classify_octo_error`], which the hand-rolled `crab.get` calls this
/// replaces did not — so their 403/404 arrived as `GhError::Http` and every
/// `Api { status }` branch downstream quietly failed to match.
///
/// Against wiremock there is no `Link` header, so the first response is also
/// the last and no extra request is made.
pub async fn paginate(crab: &Octocrab, route: &str) -> Result<Vec<Value>, GhError> {
    let first: octocrab::Page<Value> = crab
        .get(route, None::<&()>)
        .await
        .map_err(classify_octo_error)?;
    crab.all_pages(first).await.map_err(classify_octo_error)
}

/// Normalize a GitHub login for comparison.
///
/// A GitHub App authors comments and reviews as `name[bot]`, so any equality
/// check against a configured bot name or app slug must strip that suffix —
/// otherwise it silently never matches.
pub fn normalize_login(login: &str) -> String {
    let lower = login.trim().to_lowercase();
    lower.strip_suffix("[bot]").unwrap_or(&lower).to_string()
}

/// Cache for [`resolve_app_slug`]. Resolving costs an API round trip and the
/// answer can't change for a given key, so do it once per process.
static APP_SLUG: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Fetch the App's own slug via `GET /app`, which only accepts an app JWT.
async fn app_slug_via_jwt(cfg: &Config) -> Result<String, GhError> {
    let app = Client::app_client(cfg)?;
    let v: Value = app
        .get("/app", None::<&()>)
        .await
        .map_err(classify_octo_error)?;
    v.get("slug")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(normalize_login)
        .ok_or_else(|| GhError::BadShape("no slug in GET /app".into()))
}

/// Resolve the login this App comments and reviews as, without the `[bot]`
/// suffix. Order: process cache, `APP_SLUG` override, `GET /app`, then
/// `BOT_NAME` as a last resort.
///
/// The override exists for serverless, where every invocation builds a fresh
/// process and the cache never warms — setting `APP_SLUG` skips the round trip.
pub async fn resolve_app_slug(cfg: &Config) -> String {
    if let Some(s) = APP_SLUG.get() {
        return s.clone();
    }
    if !cfg.app_slug.trim().is_empty() {
        let s = normalize_login(&cfg.app_slug);
        let _ = APP_SLUG.set(s.clone());
        return s;
    }
    let resolved = match app_slug_via_jwt(cfg).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "could not resolve app slug via GET /app ({e}); \
                 falling back to BOT_NAME — set APP_SLUG if they differ"
            );
            normalize_login(&cfg.bot_name)
        }
    };
    let _ = APP_SLUG.set(resolved.clone());
    resolved
}

fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build the RS256 signing key from the configured PEM.
///
/// GitHub App keys are RSA — PKCS#1 (`BEGIN RSA PRIVATE KEY`) as downloaded
/// from the App settings page, or PKCS#8 (`BEGIN PRIVATE KEY`) after an
/// `openssl pkcs8` conversion. `from_rsa_pem` accepts both; parsing as EC
/// rejects every real App key with `InvalidKeyFormat`.
fn encoding_key(cfg: &Config) -> Result<jsonwebtoken::EncodingKey, GhError> {
    let pem = cfg.pem().map_err(GhError::BadShape)?;
    jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(|e| {
        // Report shape, never key material, so a misconfigured key is
        // diagnosable from the logs alone.
        let head = pem.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        GhError::BadShape(format!(
            "bad PEM key: {e} (loaded {} bytes, first line {:?}) — expected an RSA \
             private key; check PRIVATE_KEY_B64 / PRIVATE_KEY_PATH",
            pem.len(),
            head.trim()
        ))
    })
}

impl Client {
    /// Build an app-level client (for listing installations).
    pub fn app_client(cfg: &Config) -> Result<Octocrab, GhError> {
        let key = encoding_key(cfg)?;
        let app_id: u64 = cfg
            .app_id
            .parse()
            .map_err(|_| GhError::BadShape(format!("bad APP_ID: {}", cfg.app_id)))?;
        Ok(OctocrabBuilder::new().app(AppId(app_id), key).build()?)
    }

    /// Build an installation-scoped client.
    ///
    /// `app_slug` identifies our own comments and reviews; pass it normalized
    /// (no `[bot]` suffix). Prefer [`Client::installation_resolved`], which
    /// looks the slug up instead of requiring callers to have it.
    pub fn installation(
        cfg: &Config,
        installation_id: i64,
        app_slug: &str,
    ) -> Result<Client, GhError> {
        let app = Self::app_client(cfg)?;
        let crab = app.installation(InstallationId(installation_id as u64))?;
        Ok(Client {
            crab,
            app_slug: normalize_login(app_slug),
        })
    }

    /// Build an installation-scoped client with the app slug resolved.
    ///
    /// Callers used to pass `installation.app_slug` from the webhook payload,
    /// but that property is GitHub's *simple installation* object — id and
    /// node_id only — so it was always empty, and every "is this mine?" check
    /// silently compared against "".
    pub async fn installation_resolved(
        cfg: &Config,
        installation_id: i64,
    ) -> Result<Client, GhError> {
        let slug = resolve_app_slug(cfg).await;
        Self::installation(cfg, installation_id, &slug)
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

    /// GET with query parameters serialized by octocrab.
    ///
    /// Prefer this over interpolating a value into the query of a format
    /// string: octocrab appends the parameters *after* the route, so a value
    /// containing `?` or `&` can no longer be read as part of the query.
    async fn get_with<P>(&self, route: &str, params: &P) -> Result<Value, GhError>
    where
        P: serde::Serialize + ?Sized,
    {
        self.crab
            .get(route, Some(params))
            .await
            .map_err(classify_octo_error)
    }

    /// GET every page of a paginated route. See [`paginate`].
    async fn get_all(&self, route: &str) -> Result<Vec<Value>, GhError> {
        paginate(&self.crab, route).await
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

    /// Fetch the current installation access token as a raw string (for git
    /// clone auth in subprocess engines). Builds a short-lived app JWT and
    /// exchanges it — independent of octocrab's internal cache.
    pub async fn installation_token(
        &self,
        cfg: &Config,
        installation_id: i64,
    ) -> Result<String, GhError> {
        // Build the app JWT manually (same claims as octocrab's flow)
        let now = chrono_now_secs();
        let claims = json!({
            "iat": now - 60,
            "exp": now + 9 * 60,
            "iss": cfg.app_id,
        });
        let key = encoding_key(cfg)?;
        let jwt = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &key,
        )
        .map_err(|e| GhError::BadShape(format!("jwt encode: {e}")))?;

        let http = reqwest::Client::new();
        let resp = http
            .post(format!(
                "https://api.github.com/app/installations/{installation_id}/access_tokens"
            ))
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|e| GhError::BadShape(e.to_string()))?;
        let status = resp.status().as_u16();
        let v: Value = resp
            .json()
            .await
            .map_err(|e| GhError::BadShape(e.to_string()))?;
        if status < 200 || status >= 300 {
            return Err(GhError::Api {
                status,
                message: v.to_string(),
            });
        }
        v.get("token")
            .and_then(|t| t.as_str())
            .map(String::from)
            .ok_or_else(|| GhError::BadShape("no token in response".into()))
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

    /// Labels on an issue or PR.
    ///
    /// `per_page` was absent, so GitHub applied its default of 30 — a repo with
    /// more labels than that on one PR would drop the rest, and `has_label`
    /// checks would read as false. A single page of 100 is enough; nothing here
    /// puts a hundred labels on one issue, and full pagination would cost a
    /// request per sweep for no benefit.
    pub async fn list_labels(&self, repo: &str, issue: i64) -> Result<Vec<String>, GhError> {
        let v = self
            .get(&format!("/repos/{repo}/issues/{issue}/labels?per_page=100"))
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

    /// Remove one label.
    ///
    /// The label name is a path segment and labels routinely contain spaces
    /// (`good first issue`) — unencoded that fails the `http::Uri` parse and
    /// surfaced in production as a bare `http error: Uri`.
    pub async fn remove_label(&self, repo: &str, issue: i64, label: &str) -> Result<(), GhError> {
        self.delete(&format!(
            "/repos/{repo}/issues/{issue}/labels/{}",
            enc_seg(label)
        ))
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
    ///
    /// A valid login can't contain anything needing escaping, and the parser
    /// validates one before building this call — the encoding is here so that
    /// stays true by construction rather than by a caller remembering to check.
    pub async fn collaborator_permission(&self, repo: &str, user: &str) -> Result<String, GhError> {
        let v = self
            .get(&format!(
                "/repos/{repo}/collaborators/{}/permission",
                enc_seg(user)
            ))
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
    ///
    /// GitHub caps this endpoint at 3000 files regardless of pagination, so a
    /// enormous PR is still truncated — that ceiling is theirs, not ours.
    pub async fn list_pr_files(&self, repo: &str, number: i64) -> Result<Vec<Value>, GhError> {
        self.get_all(&format!("/repos/{repo}/pulls/{number}/files?per_page=100"))
            .await
    }

    /// reviews on a PR
    ///
    /// Paginated in full: `own_previous_reviews` reads this to find the bot's
    /// last review, and on a long PR that review is on a later page — stopping
    /// at page one made `r-` answer "nothing to withdraw" and made incremental
    /// review start from scratch.
    pub async fn list_pr_reviews(&self, repo: &str, number: i64) -> Result<Vec<Value>, GhError> {
        self.get_all(&format!(
            "/repos/{repo}/pulls/{number}/reviews?per_page=100"
        ))
        .await
    }

    /// previous reviews left by this bot (incremental review memory)
    pub async fn own_previous_reviews(
        &self,
        repo: &str,
        number: i64,
    ) -> Result<Vec<Value>, GhError> {
        let reviews = self.list_pr_reviews(repo, number).await?;
        // `app_slug` is already normalized; the review author is `slug[bot]`, so
        // normalize that side too or this never matches and the incremental
        // review silently has no memory.
        let slug = normalize_login(&self.app_slug);
        if slug.is_empty() {
            tracing::warn!("app_slug is empty; cannot identify own reviews on {repo}#{number}");
            return Ok(Vec::new());
        }
        Ok(reviews
            .into_iter()
            .filter(|r| {
                r.get("user")
                    .and_then(|u| u.get("login"))
                    .and_then(|l| l.as_str())
                    .map(|l| normalize_login(l) == slug)
                    .unwrap_or(false)
            })
            .collect())
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

    /// commits on the PR (for incremental review: what's new since last review)
    ///
    /// GitHub caps this endpoint at 250 commits regardless of pagination; past
    /// that the list is theirs to truncate.
    pub async fn list_pr_commits(&self, repo: &str, number: i64) -> Result<Vec<Value>, GhError> {
        self.get_all(&format!(
            "/repos/{repo}/pulls/{number}/commits?per_page=100"
        ))
        .await
    }

    // -------------------------------------------------------------------
    // Repo content (agent tools)
    // -------------------------------------------------------------------

    /// Route for the contents API, with the path encoded as a path.
    ///
    /// `ref` is *not* in here: it goes through [`Client::get_with`] so octocrab
    /// appends it. Interpolating it into the format string was the worse half
    /// of the same bug — for `a?b.md` the route became
    /// `…/contents/a?b.md?ref=main`, whose query is the single key
    /// `b.md?ref`, so GitHub saw no `ref` at all and served the default
    /// branch. That failure *succeeds*: the agent gets a file, just the wrong
    /// revision of it, and nothing anywhere reports a problem.
    fn contents_route(repo: &str, path: &str) -> String {
        if path.is_empty() {
            format!("/repos/{repo}/contents")
        } else {
            format!("/repos/{repo}/contents/{}", enc_path(path))
        }
    }

    /// file content at ref (decoded from base64), or None if it's a directory
    pub async fn get_file_content(
        &self,
        repo: &str,
        path: &str,
        reference: &str,
    ) -> Result<Option<String>, GhError> {
        let v = self
            .get_with(
                &Self::contents_route(repo, path),
                &[("ref", reference)] as &[(&str, &str)],
            )
            .await?;
        // A directory comes back as a JSON *array*, so `get("type")` is None and
        // this fell through to `BadShape("no content")` — a plain "is it a
        // directory?" question answered as a malformed response.
        if v.is_array() {
            return Ok(None);
        }
        if v.get("type").and_then(|t| t.as_str()) == Some("dir") {
            return Ok(None);
        }
        // Over 1 MB, GitHub returns `content: ""` with `encoding: "none"`. That
        // decodes to an empty string, so the file read as empty and the agent
        // reasoned from that. Anything but base64 is a refusal, not a file.
        let encoding = v.get("encoding").and_then(|e| e.as_str()).unwrap_or("");
        if encoding != "base64" {
            let size = v.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            return Err(GhError::BadShape(format!(
                "{path}: contents API returned encoding {encoding:?} ({size} bytes) — \
                 too large to read inline, not empty"
            )));
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
        let v = self
            .get_with(
                &Self::contents_route(repo, path),
                &[("ref", reference)] as &[(&str, &str)],
            )
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    // -------------------------------------------------------------------
    // Code scanning (CodeQL reports)
    // -------------------------------------------------------------------

    /// open code-scanning alerts; Err(Api{403/404}) when not enabled
    ///
    /// Paginated in full, and the most important of the set to get right: a
    /// truncated alert list makes the report claim a changed file has no
    /// findings when it has some, which is worse than no report at all.
    pub async fn code_scanning_alerts(&self, repo: &str) -> Result<Vec<Value>, GhError> {
        self.get_all(&format!(
            "/repos/{repo}/code-scanning/alerts?state=open&per_page=100"
        ))
        .await
    }

    // -------------------------------------------------------------------
    // Installations / repositories (sweep)
    // -------------------------------------------------------------------

    /// Every repository reachable via an installation client.
    ///
    /// The response wraps its list in `{"repositories": [...]}`, which
    /// `Page<Value>` unwraps — so an org past 100 repos no longer has its tail
    /// silently skipped by the sweep.
    pub async fn installation_repositories_via(client: &Client) -> Result<Vec<String>, GhError> {
        let repos = client
            .get_all("/installation/repositories?per_page=100")
            .await?;
        Ok(repos
            .iter()
            .filter_map(|r| {
                r.get("full_name")
                    .and_then(|f| f.as_str())
                    .map(String::from)
            })
            .collect())
    }

    /// All open PRs for a repo.
    pub async fn open_prs(&self, repo: &str) -> Result<Vec<Value>, GhError> {
        self.get_all(&format!("/repos/{repo}/pulls?state=open&per_page=100"))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{enc_path, enc_seg};

    /// Pins the escaping set. The two functions must differ in exactly one
    /// character — `/` — because that is the whole reason both exist.
    #[test]
    fn encoders_escape_everything_but_the_unreserved_set() {
        // Unreserved characters pass through untouched (RFC 3986 §2.3).
        for s in ["needs-rebase", "a_b", "v1.2.3", "a~b", "Zz09"] {
            assert_eq!(enc_seg(s), s, "{s} should not be escaped");
            assert_eq!(enc_path(s), s, "{s} should not be escaped");
        }

        // The production failure: a label with a space failed the Uri parse.
        assert_eq!(enc_seg("good first issue"), "good%20first%20issue");

        // `?` must not survive, or it ends the path and turns the rest of the
        // route into a query string.
        assert_eq!(enc_seg("a?b.md"), "a%3Fb.md");
        assert_eq!(enc_path("a?b.md"), "a%3Fb.md");

        // Non-ASCII is percent-encoded per UTF-8 byte.
        assert_eq!(enc_seg("说明"), "%E8%AF%B4%E6%98%8E");

        // The one difference: a segment escapes `/`, a path keeps it.
        assert_eq!(enc_seg("docs/a.md"), "docs%2Fa.md");
        assert_eq!(enc_path("docs/a.md"), "docs/a.md");

        // Nothing an encoder does should ever be re-encodable into a different
        // string — % itself has to be escaped for that to hold.
        assert_eq!(enc_seg("100%"), "100%25");
    }
}
