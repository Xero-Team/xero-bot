//! Public project discovery, independent of queue activity and repository TOML.
use super::command_reference::Provider;
use crate::{admission, config::Config, forgejo::ForgejoClient, github::Client};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use futures::{stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};

const TTL: Duration = Duration::from_secs(60);
const BACKOFF: Duration = Duration::from_secs(15);
const TIMEOUT: Duration = Duration::from_secs(20);
type Projects = Result<Vec<String>, ()>;

struct Snapshot {
    checked: Instant,
    projects: Projects,
}
#[derive(Default)]
struct Cache(Mutex<Option<Snapshot>>);
impl Cache {
    async fn get(&self, fetch: impl Future<Output = Projects>) -> Projects {
        // Coalesce simultaneous requests and never return expired names on failure.
        let mut cached = self.0.lock().await;
        if let Some(snapshot) = cached.as_ref() {
            let ttl = if snapshot.projects.is_ok() {
                TTL
            } else {
                BACKOFF
            };
            if snapshot.checked.elapsed() < ttl {
                return snapshot.projects.clone();
            }
        }
        let started = Instant::now();
        let projects = tokio::time::timeout(TIMEOUT, fetch)
            .await
            .unwrap_or(Err(()));
        *cached = Some(Snapshot {
            checked: if projects.is_ok() {
                started
            } else {
                Instant::now()
            },
            projects: projects.clone(),
        });
        projects
    }
}
#[derive(Clone)]
struct Directory {
    cfg: Config,
    github: Arc<Cache>,
    forgejo: Arc<Cache>,
}
#[derive(Default, Deserialize)]
struct Options {
    #[serde(default)]
    provider: Provider,
}
pub(super) fn routes<S: Clone + Send + Sync + 'static>(cfg: Config) -> Router<S> {
    Router::new()
        .route("/api/command-projects", get(data))
        .route("/assets/projects.js", get(script))
        .with_state(Directory {
            cfg,
            github: Arc::default(),
            forgejo: Arc::default(),
        })
}
fn public(meta: &Value) -> bool {
    meta["private"] == false && meta["archived"] == false
}
async fn github_projects(cfg: &Config, gh: &Client, installation: i64) -> Projects {
    admission::github_installation(cfg, installation).map_err(|_| ())?;
    let repos = gh
        .get_all("/installation/repositories?per_page=100")
        .await
        .map_err(|_| ())?;
    Ok(repos
        .into_iter()
        .filter(|meta| public(meta) && admission::github_metadata(cfg, installation, meta))
        .filter_map(|meta| meta["full_name"].as_str().map(str::to_owned))
        .collect())
}
async fn forgejo_projects(cfg: &Config, fj: &ForgejoClient, names: Vec<String>) -> Projects {
    let repos: Vec<Option<String>> = stream::iter(names)
        .map(|name| async move {
            // The candidates come from admission, but keep the no-unapproved-I/O boundary.
            if !admission::forgejo_scope(cfg, &name, None) {
                return Ok(None);
            }
            let meta = match fj.repository(&name).await {
                Ok(meta) => meta,
                Err(crate::forgejo::ForgejoError::Api {
                    status: 403 | 404, ..
                }) => return Ok(None),
                Err(_) => return Err(()),
            };
            let live_name = meta["full_name"].as_str().unwrap_or_default();
            Ok((public(&meta)
                && live_name.eq_ignore_ascii_case(&name)
                && admission::forgejo_metadata(cfg, &meta))
            .then(|| live_name.to_owned()))
        })
        .buffer_unordered(4)
        .try_collect()
        .await?;
    Ok(repos.into_iter().flatten().collect())
}
async fn discover(cfg: &Config, provider: Provider) -> Projects {
    let mut names = match provider {
        Provider::Github => {
            let groups: Vec<Vec<String>> = stream::iter(admission::github_installations(cfg))
                .map(|installation| async move {
                    let gh =
                        Client::installation(cfg, installation, &cfg.bot_name).map_err(|_| ())?;
                    github_projects(cfg, &gh, installation).await
                })
                .buffer_unordered(4)
                .try_collect()
                .await?;
            groups.into_iter().flatten().collect::<Vec<_>>()
        }
        Provider::Forgejo => {
            let names = admission::forgejo_repositories(cfg);
            if names.is_empty() {
                return Ok(Vec::new());
            }
            let fj =
                ForgejoClient::new(&cfg.forgejo_base_url, &cfg.forgejo_token).map_err(|_| ())?;
            forgejo_projects(cfg, &fj, names).await?
        }
    };
    names.sort_by_key(|name| name.to_ascii_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(names)
}
async fn data(State(s): State<Directory>, Query(q): Query<Options>) -> impl IntoResponse {
    let cache = match q.provider {
        Provider::Github => &s.github,
        Provider::Forgejo => &s.forgejo,
    };
    let (status, body) = match cache.get(discover(&s.cfg, q.provider)).await {
        Ok(names) => (
            StatusCode::OK,
            json!({"provider": q.provider, "repositories": names, "cache_seconds": TTL.as_secs()}),
        ),
        Err(()) => (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "Project discovery is temporarily unavailable. Retry shortly."}),
        ),
    };
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body))
}
async fn script() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        include_str!("projects.js"),
    )
}

#[cfg(test)]
#[path = "project_directory_tests.rs"]
mod tests;
