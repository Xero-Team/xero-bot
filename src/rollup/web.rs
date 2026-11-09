//! Minimal, project-scoped, read-only queue and generated command reference.
use crate::{config::Config, lang::Lang, trigger_state::Store};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
#[path = "command_reference.rs"]
mod command_reference;
#[path = "project_directory.rs"]
mod project_directory;
use command_reference::{command_data, commands, project_command_data, project_commands};
#[derive(Clone)]
pub struct WebState {
    pub cfg: Config,
    pub store: Arc<Store>,
}
#[derive(Default, Deserialize)]
struct Language {
    lang: Option<String>,
}
impl Language {
    fn get(&self) -> Lang {
        if self.lang.as_deref() == Some("zh") {
            Lang::Zh
        } else {
            Lang::En
        }
    }
}
fn code(lang: Lang) -> &'static str {
    lang.pick("en", "zh")
}
pub fn routes<S: Clone + Send + Sync + 'static>(state: WebState) -> Router<S> {
    let directory = project_directory::routes(state.cfg.clone());
    Router::new()
        .route("/", get(projects))
        .route("/queue", get(projects))
        .route("/queue/{owner}/{repo}", get(project))
        .route("/commands", get(commands))
        .route("/commands/{owner}/{repo}", get(project_commands))
        .route("/api/queue", get(data))
        .route("/api/queue/{owner}/{repo}", get(project_data))
        .route("/api/commands", get(command_data))
        .route("/api/commands/{owner}/{repo}", get(project_command_data))
        .route("/assets/queue.js", get(script))
        .with_state(state)
        .merge(directory)
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn page(title: &str, body: &str, path: &str, lang: Lang) -> impl IntoResponse {
    let locale = code(lang);
    let language_path = format!(
        "{}{}lang=",
        escape(path),
        if path.contains('?') { "&amp;" } else { "?" }
    );
    let html = format!(
        r#"<!doctype html><html lang="{locale}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{} · Xero Bot</title><style>{}</style></head><body><nav><a href="/?lang={locale}">Xero Bot</a> | <a href="/queue?lang={locale}">{}</a> | <a href="/commands?lang={locale}">{}</a><span class="languages"><a href="{language_path}en" lang="en">English</a> | <a href="{language_path}zh" lang="zh">中文</a></span></nav><main>{body}</main><footer>Xero Bot v{} · {}</footer></body></html>"#,
        escape(title),
        include_str!("style.css"),
        lang.pick("Projects", "项目"),
        lang.pick("Commands", "指令"),
        env!("CARGO_PKG_VERSION"),
        lang.pick(
            "Read-only dashboard. Queue updates every 10 seconds.",
            "只读看板，队列每 10 秒更新。"
        )
    );
    ([(header::CACHE_CONTROL,"no-store"),(header::CONTENT_SECURITY_POLICY,"default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'"),(header::X_CONTENT_TYPE_OPTIONS,"nosniff")],Html(html))
}
async fn projects(Query(q): Query<Language>) -> impl IntoResponse {
    let lang = q.get();
    page(
        lang.pick("Projects", "项目"),
        &format!(
            r#"<h1>{}</h1><div class="toolbar"><span id="updated" role="status">{}</span><button id="refresh">{}</button></div><div id="queues" data-project=""></div><script src="/assets/queue.js" defer></script>"#,
            lang.pick("Projects", "项目"),
            lang.pick("Loading…", "加载中…"),
            lang.pick("Refresh", "刷新")
        ),
        "/queue",
        lang,
    )
}
async fn project(
    Path((owner, repo)): Path<(String, String)>,
    Query(q): Query<Language>,
) -> impl IntoResponse {
    let name = format!("{owner}/{repo}");
    let lang = q.get();
    let title = format!("{} - {name}", lang.pick("Rollup queue", "Rollup 队列"));
    let path = format!(
        "/queue/{}/{}",
        crate::github::enc_seg(&owner),
        crate::github::enc_seg(&repo)
    );
    page(
        &title,
        &format!(
            r#"<h1>{}</h1><p><a href="/commands/{}/{}?lang={}">{}</a></p><div class="toolbar"><span id="summary"></span><span id="updated" role="status">{}</span><button id="refresh">{}</button></div><div id="queues" data-project="{}"></div><script src="/assets/queue.js" defer></script>"#,
            escape(&title),
            crate::github::enc_seg(&owner),
            crate::github::enc_seg(&repo),
            code(lang),
            lang.pick(
                "View project commands and @ requirements",
                "查看项目可用指令及 @ 规则"
            ),
            lang.pick("Loading…", "加载中…"),
            lang.pick("Refresh", "刷新"),
            escape(&name)
        ),
        &path,
        lang,
    )
}
pub(crate) fn public_snapshot(repos: Vec<super::Repository>) -> Value {
    let repos=repos.into_iter().filter(|r|r.public && super::now()-r.checked<=120).map(|r|json!({
        "repository":r.name,"updated_at":r.checked,"next_attempt":r.next_attempt,"waiting_for_api":!r.detail.is_empty(),
        "entries":r.entries.iter().filter(|e|e.state != "superseded").map(|e|json!({"number":e.number,"title":e.title,"head":e.head,"approved_by":e.approved_by,"source":e.origin.provider,"rollup":e.rollup,"try_only":e.try_only,"state":e.state,"batch":e.batch,"created":e.created})).collect::<Vec<_>>(),
        "batches":r.batches.iter().rev().take(50).map(|b|json!({"id":b.id,"state":b.state,"tip":b.tip,"base":b.base,"members":b.approvals.iter().map(|e|e.number).collect::<Vec<_>>(),"github_ci":b.github_ci,"forgejo_ci":b.forgejo_ci,"passed_on":b.passed_on,"audit_pr":b.audit_pr,"created":b.created,"finished":b.finished})).collect::<Vec<_>>()
    })).collect::<Vec<_>>();
    json!({"updated_at":super::now(),"repositories":repos})
}
fn response(store: &Store, project: Option<&str>) -> impl IntoResponse {
    match store.rollup_all() {
        Ok(repos) => {
            let repos = repos
                .into_iter()
                .filter(|r| project.is_none_or(|p| r.name.eq_ignore_ascii_case(p)))
                .collect();
            (
                StatusCode::OK,
                [(header::CACHE_CONTROL, "no-store")],
                Json(public_snapshot(repos)),
            )
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({"error":"queue temporarily unavailable"})),
        ),
    }
}
async fn data(State(s): State<WebState>) -> impl IntoResponse {
    response(&s.store, None)
}
async fn project_data(
    State(s): State<WebState>,
    Path((owner, repo)): Path<(String, String)>,
) -> impl IntoResponse {
    response(&s.store, Some(&format!("{owner}/{repo}")))
}
async fn script() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("queue.js"),
    )
}
