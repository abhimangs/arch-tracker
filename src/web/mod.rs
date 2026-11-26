//! Local dashboard: axum on 127.0.0.1 only, embedded assets, JSON API.
//! It answers as http://arch-tracker.localhost:<port>/ (old 127.0.0.1 links are redirected there);
//! other Host headers are refused, which stops DNS-rebinding pages from reading your data.
//! Everything but static assets needs the session cookie (see auth.rs), and every write also
//! needs the per-run CSRF token.

pub mod auth;

use crate::config::{self, Config};
use crate::daemon::{Event, SharedRef};
use crate::export;
use crate::ipc::Request;
use crate::report;
use crate::util;
use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request as HttpRequest, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, mpsc::UnboundedSender, oneshot};

#[derive(rust_embed::Embed)]
#[folder = "web-ui/"]
struct Assets;

pub struct Web {
    shared: SharedRef,
    tx: UnboundedSender<Event>,
    token: String,
    db: std::path::PathBuf,
    /// Read-only dashboard (arch-tracker serve): no settings writes, no pause.
    viewer: bool,
    current: Mutex<Option<(u16, oneshot::Sender<()>)>>,
    /// Why the configured port could not be served, if it couldn't.
    error: Mutex<Option<String>>,
    /// App id -> its icon file (None: none found), and when that was looked up.
    icons:
        Mutex<std::collections::HashMap<String, (std::time::Instant, Option<std::path::PathBuf>)>>,
}

#[derive(Clone)]
struct Ctx {
    web: Arc<Web>,
    port: u16,
}

impl Web {
    pub fn new(
        shared: SharedRef,
        tx: UnboundedSender<Event>,
        db: std::path::PathBuf,
        viewer: bool,
    ) -> Result<Arc<Web>> {
        Ok(Arc::new(Web {
            shared,
            tx,
            token: auth::random_hex(24)?,
            db,
            viewer,
            current: Mutex::new(None),
            error: Mutex::new(None),
            icons: Mutex::new(Default::default()),
        }))
    }

    pub fn set_error(&self, e: Option<String>) {
        if let Ok(mut x) = self.error.lock() {
            *x = e;
        }
    }

    /// One-time sign-in URL for the dashboard, opening at a hash route ("today", "settings").
    pub fn login_url(&self, route: &str) -> Result<String> {
        auth::login_url(self.port().unwrap_or(0), route)
    }

    pub fn port(&self) -> Option<u16> {
        self.current
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|(p, _)| *p))
    }

    /// Serves on port; an existing listener on another port stays up for 5 s so the page that
    /// asked for the change can follow the redirect.
    pub async fn listen(self: &Arc<Self>, port: u16) -> Result<()> {
        if self.port() == Some(port) {
            return Ok(());
        }
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let app = router(Ctx {
            web: self.clone(),
            port,
        });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stop_rx.await;
                })
                .await;
        });
        let old = self
            .current
            .lock()
            .ok()
            .and_then(|mut c| c.replace((port, stop_tx)));
        if let Some((_, old_stop)) = old {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let _ = old_stop.send(());
            });
        }
        auth::set_port(port);
        tracing::info!("dashboard on http://{}/", canonical_host(port));
        Ok(())
    }

    fn config(&self) -> Config {
        self.shared
            .lock()
            .map(|s| s.config.clone())
            .unwrap_or_default()
    }
}

fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/login", get(login))
        .route("/assets/{file}", get(asset))
        .route("/custom.css", get(custom_css))
        .route("/api/status", get(api_status))
        .route("/api/live", get(api_live))
        .route("/api/day", get(api_day))
        .route("/api/range", get(api_range))
        .route("/api/browsers", get(api_browsers))
        .route("/api/browser", get(api_browser))
        .route("/api/browser/day", get(api_browser_day))
        .route("/api/agents", get(api_agents))
        .route("/api/agent", get(api_agent))
        .route("/api/agent/day", get(api_agent_day))
        .route("/api/icon/{app}", get(api_icon))
        .route("/api/claude", get(api_claude))
        .route("/api/codex", get(api_codex))
        .route("/api/hermes", get(api_hermes))
        .route("/api/limits", get(api_limits))
        .route("/api/week", get(api_week))
        .route("/api/settings", get(api_settings).put(api_settings_put))
        .route("/api/appearance", axum::routing::put(api_appearance))
        .route("/api/rule", post(api_rule))
        .route("/api/plan", post(api_plan))
        .route("/api/pause", post(api_pause))
        .route("/api/recompute", post(api_recompute))
        .route("/api/notify-test", post(api_notify_test))
        .route("/api/folders", get(api_folders))
        .route("/api/resume", post(api_resume))
        .route("/api/export", get(api_export))
        .fallback(|| async { err(StatusCode::NOT_FOUND, "Not found.") })
        .layer(middleware::from_fn_with_state(ctx.clone(), guard))
        .with_state(ctx)
}

/// The dashboard's own origin host ("arch-tracker.localhost:7600").
fn canonical_host(port: u16) -> String {
    format!("{}:{port}", auth::HOST)
}

fn header_str(req: &HttpRequest, name: header::HeaderName) -> &str {
    req.headers()
        .get(name)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
}

/// Host check, then the session cookie, then the CSRF token on every non-GET request.
async fn guard(State(ctx): State<Ctx>, req: HttpRequest, next: Next) -> Response {
    let host = header_str(&req, header::HOST);
    let canonical = canonical_host(ctx.port);
    let loopback = [
        format!("127.0.0.1:{}", ctx.port),
        format!("localhost:{}", ctx.port),
        format!("[::1]:{}", ctx.port),
    ];
    if loopback.iter().any(|h| h == host) {
        // Old links and bookmarks: move to the dashboard's own host name, where its cookie is
        // not shared with other local servers.
        if req.method() == axum::http::Method::GET {
            let pq = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/");
            return Redirect::temporary(&format!("http://{canonical}{pq}")).into_response();
        }
        return err(
            StatusCode::FORBIDDEN,
            format!("Use http://{canonical}/ for the dashboard."),
        );
    }
    if host != canonical {
        return (
            StatusCode::FORBIDDEN,
            format!("Blocked: this dashboard only answers as http://{canonical}/"),
        )
            .into_response();
    }
    let path = req.uri().path();
    let public = path.starts_with("/assets/") || path == "/login";
    if !public && !auth::cookie_ok(header_str(&req, header::COOKIE)) {
        if path == "/" {
            return locked_page(&ctx, None);
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": LOCKED_MSG, "locked": true})),
        )
            .into_response();
    }
    if req.method() != axum::http::Method::GET {
        let token = req
            .headers()
            .get("x-arch-tracker-token")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let origin_ok = match req
            .headers()
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
        {
            None => true,
            Some(o) => o == format!("http://{canonical}"),
        };
        if token != ctx.web.token || !origin_ok {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Missing or wrong dashboard token. Reload the page."})),
            )
                .into_response();
        }
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             font-src 'self'; connect-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    resp
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({"error": msg.to_string()}))).into_response()
}

const LOCKED_MSG: &str =
    "The dashboard is locked. Open it from the arch-tracker tray icon, or run: arch-tracker open";

/// Fills the theme attributes of a page (no flash of the wrong theme before app.js runs).
fn themed(html: &str, cfg: &Config) -> String {
    let d = &cfg.dashboard;
    html.replace("__THEME__", &d.theme)
        .replace("__PALETTE__", &d.palette)
        .replace("__DARK__", &d.dark_flavor)
        .replace("__ACCENT__", &d.accent)
        .replace(
            "__THEMECATS__",
            if d.theme_categories { "on" } else { "off" },
        )
        .replace("__DENSITY__", &d.density)
        .replace("__MOTION__", &d.motion)
        .replace("__VERSION__", env!("CARGO_PKG_VERSION"))
}

/// What a browser without the session cookie sees.
fn locked_page(ctx: &Ctx, note: Option<&str>) -> Response {
    let Some(f) = Assets::get("locked.html") else {
        return err(StatusCode::UNAUTHORIZED, LOCKED_MSG);
    };
    let note = note
        .map(|n| format!("<p class=\"locked-note\">{n}</p>"))
        .unwrap_or_default();
    let html =
        themed(&String::from_utf8_lossy(&f.data), &ctx.web.config()).replace("__NOTE__", &note);
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

#[derive(Deserialize)]
struct LoginQ {
    code: Option<String>,
    to: Option<String>,
}

/// Trades a one-time code for the session cookie, then opens the requested page.
async fn login(State(ctx): State<Ctx>, Query(q): Query<LoginQ>, req: HttpRequest) -> Response {
    let route: String =
        q.to.unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-'))
            .take(64)
            .collect();
    let to = format!("/#/{}", if route.is_empty() { "today" } else { &route });
    let fresh = q.code.as_deref().map(auth::redeem).unwrap_or(false);
    if fresh {
        return match auth::set_cookie() {
            Ok(c) => (
                StatusCode::SEE_OTHER,
                [
                    (header::LOCATION, to),
                    (header::SET_COOKIE, c),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
            )
                .into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        };
    }
    if auth::cookie_ok(header_str(&req, header::COOKIE)) {
        return Redirect::to(&to).into_response();
    }
    locked_page(
        &ctx,
        Some("That sign-in link has expired or was already used. Open the dashboard again."),
    )
}

/// An error about one input, named so the page can point at it.
fn err_field(status: StatusCode, msg: impl ToString, field: &str) -> Response {
    (
        status,
        Json(json!({"error": msg.to_string(), "field": field})),
    )
        .into_response()
}

/// Json<T> whose rejections are JSON as well, naming the field that could not be read.
struct JsonBody<T>(T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(req: HttpRequest, state: &S) -> Result<Self, Response> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(JsonBody(v)),
            Err(r) => Err(rejection(r.status(), &r.body_text())),
        }
    }
}

/// Query<T> whose rejections are JSON as well.
struct Params<T>(T);

impl<S, T> FromRequestParts<S> for Params<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Response> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Params(v)),
            Err(r) => Err(rejection(r.status(), &r.body_text())),
        }
    }
}

/// Turns axum's plain-text rejection, e.g. "Failed to deserialize the JSON body into the target
/// type: config.dashboard.port: invalid value: integer `70000`, expected u16 at line 1 column 9",
/// into {"error": "Invalid value for dashboard.port: …", "field": "dashboard.port"}.
fn rejection(status: StatusCode, text: &str) -> Response {
    match explain_rejection(text) {
        (msg, Some(field)) => err_field(status, msg, &field),
        (msg, None) => err(status, msg),
    }
}

fn explain_rejection(text: &str) -> (String, Option<String>) {
    let detail = match text.split_once(": ") {
        Some((head, rest)) if head.starts_with("Failed to") => rest,
        _ => text,
    };
    let detail = detail.rfind(" at line ").map_or(detail, |i| &detail[..i]);
    let path = detail.split_once(": ").filter(|(p, _)| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '[' | ']'))
    });
    match path {
        Some((p, why)) => {
            let field = p.strip_prefix("config.").unwrap_or(p);
            (
                format!("Invalid value for {field}: {why}."),
                Some(field.to_string()),
            )
        }
        None => (format!("The request could not be read: {detail}."), None),
    }
}

/// 400 for requests the caller got wrong, 500 for everything else.
fn report_err(e: anyhow::Error) -> Response {
    if e.downcast_ref::<report::BadRequest>().is_some() {
        err(StatusCode::BAD_REQUEST, format!("{e:#}"))
    } else {
        tracing::warn!("dashboard request failed: {e:#}");
        err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

async fn index(State(ctx): State<Ctx>) -> Response {
    let Some(f) = Assets::get("index.html") else {
        return err(StatusCode::NOT_FOUND, "missing index.html");
    };
    let html = themed(&String::from_utf8_lossy(&f.data), &ctx.web.config())
        .replace("__TOKEN__", &ctx.web.token);
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

async fn asset(Path(file): Path<String>) -> Response {
    let Some(f) = Assets::get(&file) else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    let ct = match file.rsplit('.').next() {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        f.data.into_owned(),
    )
        .into_response()
}

async fn custom_css() -> Response {
    let css = std::fs::read_to_string(config::custom_css_path()).unwrap_or_default();
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        css,
    )
        .into_response()
}

/// The tracker's status plus what only the web server knows.
fn status_value(web: &Web) -> serde_json::Value {
    let mut v = web
        .shared
        .lock()
        .map(|s| s.status.clone())
        .unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("today".into(), json!(util::local_date(util::now_ms())));
        o.insert(
            "extension".into(),
            json!({"id": crate::setup::EXTENSION_ID, "dir": crate::setup::extension_dir()}),
        );
        let dash_err = web.error.lock().ok().and_then(|e| e.clone());
        o.insert("dashboard_error".into(), json!(dash_err));
        o.insert("server_ms".into(), json!(util::now_ms()));
    }
    v
}

async fn api_status(State(ctx): State<Ctx>) -> Response {
    Json(status_value(&ctx.web)).into_response()
}

/// Server-sent events: the status now, then again every time the tracker republishes it
/// (at once on a focus change, otherwise at most once a second).
async fn api_live(State(ctx): State<Ctx>) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    let Ok(rx) = ctx.web.shared.lock().map(|s| s.live.subscribe()) else {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "status unavailable");
    };
    let web = ctx.web.clone();
    let stream = futures_util::stream::unfold((rx, web, true), |(mut rx, web, first)| async move {
        if !first && rx.changed().await.is_err() {
            return None;
        }
        let data = status_value(&web).to_string();
        let ev: Result<SseEvent, std::convert::Infallible> = Ok(SseEvent::default().data(data));
        Some((ev, (rx, web, false)))
    });
    (
        [(header::CACHE_CONTROL, "no-store")],
        Sse::new(stream).keep_alive(KeepAlive::default()),
    )
        .into_response()
}

#[derive(Deserialize)]
struct DayQ {
    date: Option<String>,
}

async fn blocking<T: serde::Serialize + Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Response {
    // A report allocates a lot for a moment; give it back once it is sent.
    let job = move || {
        let r = f();
        crate::util::trim_heap();
        r
    };
    match tokio::task::spawn_blocking(job).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => report_err(e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// The tracker's own dashboard opens read-write when it can (the rollup cache is written); a
/// read-only viewer (arch-tracker serve) never opens the database for writing.
fn db(path: &std::path::Path, viewer: bool) -> Result<rusqlite::Connection> {
    if viewer {
        return crate::store::open_readonly(path);
    }
    let c = match rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    ) {
        Ok(c) => c,
        Err(_) => crate::store::open_readonly(path)?,
    };
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(c)
}

async fn api_day(State(ctx): State<Ctx>, Params(q): Params<DayQ>) -> Response {
    let cfg = ctx.web.config();
    let date = q.date.unwrap_or_else(|| util::local_date(util::now_ms()));
    if let Err(e) = report::parse_date(&date) {
        return err_field(StatusCode::BAD_REQUEST, format!("{e:#}"), "date");
    }
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    blocking(move || report::build_day(&db(&path, viewer)?, &cfg, &date)).await
}

#[derive(Deserialize)]
struct RangeQ {
    from: String,
    to: String,
}

async fn api_range(State(ctx): State<Ctx>, Params(q): Params<RangeQ>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    let cache = if viewer {
        report::Cache::ReadOnly
    } else {
        report::Cache::ReadWrite
    };
    blocking(move || report::range(&db(&path, viewer)?, &cfg, &q.from, &q.to, cache)).await
}

fn cache_mode(viewer: bool) -> report::Cache {
    if viewer {
        report::Cache::ReadOnly
    } else {
        report::Cache::ReadWrite
    }
}

/// Browsers for the menu: linked or with site history, most used first.
async fn api_browsers(State(ctx): State<Ctx>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    let today = util::local_date(util::now_ms());
    blocking(move || {
        report::browsers::browser_list(&db(&path, viewer)?, &cfg, &today, cache_mode(viewer))
    })
    .await
}

/// An app's icon from the icon theme (the browsers' menu shows them). Only the icon its
/// desktop entry names is served, never an arbitrary file.
async fn api_icon(State(ctx): State<Ctx>, Path(app): Path<String>) -> Response {
    let ok = !app.is_empty()
        && app.len() <= 200
        && app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !ok {
        return err(StatusCode::BAD_REQUEST, "not an app id");
    }
    const RECHECK: std::time::Duration = std::time::Duration::from_secs(600);
    let known = ctx.web.icons.lock().ok().and_then(|m| {
        m.get(&app)
            .filter(|(at, p)| p.is_some() || at.elapsed() < RECHECK)
            .map(|(_, p)| p.clone())
    });
    let path = match known {
        Some(p) => p,
        None => {
            let id = app.clone();
            let found = tokio::task::spawn_blocking(move || {
                let idx = crate::desktop::DesktopIndex::load();
                let named = idx
                    .by_id(&id)
                    .or_else(|| idx.by_wmclass(&id))
                    .map(|e| e.icon.clone())
                    .filter(|i| !i.is_empty());
                named
                    .and_then(|i| crate::desktop::icon_file(&i))
                    .or_else(|| crate::desktop::icon_file(&id))
            })
            .await
            .ok()
            .flatten();
            if let Ok(mut m) = ctx.web.icons.lock() {
                m.insert(app, (std::time::Instant::now(), found.clone()));
            }
            found
        }
    };
    let Some(path) = path else {
        return err(StatusCode::NOT_FOUND, "no icon");
    };
    let svg = path.extension().is_some_and(|e| e == "svg");
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) if b.len() <= 2 << 20 => b,
        _ => return err(StatusCode::NOT_FOUND, "no icon"),
    };
    let mut res = (
        [
            (
                header::CONTENT_TYPE,
                if svg { "image/svg+xml" } else { "image/png" },
            ),
            (header::CACHE_CONTROL, "private, max-age=86400"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response();
    // An SVG opened on its own would run in the dashboard's origin: let it do nothing.
    res.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    res
}

#[derive(Deserialize)]
struct BrowserQ {
    id: String,
    from: String,
    to: String,
}

/// One browser (or "all") over a range of days.
async fn api_browser(State(ctx): State<Ctx>, Params(q): Params<BrowserQ>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    blocking(move || {
        let conn = db(&path, viewer)?;
        report::browsers::browser_report(&conn, &cfg, &q.id, &q.from, &q.to, cache_mode(viewer))
    })
    .await
}

#[derive(Deserialize)]
struct BrowserDayQ {
    id: String,
    date: Option<String>,
}

/// Agents for the menu: Claude Code, Codex and Hermes when installed or seen.
async fn api_agents(State(ctx): State<Ctx>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    let today = util::local_date(util::now_ms());
    blocking(move || {
        report::agents::agent_list(&db(&path, viewer)?, &cfg, &today, cache_mode(viewer))
    })
    .await
}

/// One agent (or "all") over a range of days.
async fn api_agent(State(ctx): State<Ctx>, Params(q): Params<BrowserQ>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    blocking(move || {
        let conn = db(&path, viewer)?;
        report::agents::agent_report(&conn, &cfg, &q.id, &q.from, &q.to, cache_mode(viewer))
    })
    .await
}

/// One agent's (or every agent's) sessions and stretches of work on a day.
async fn api_agent_day(State(ctx): State<Ctx>, Params(q): Params<BrowserDayQ>) -> Response {
    let cfg = ctx.web.config();
    let date = q.date.unwrap_or_else(|| util::local_date(util::now_ms()));
    if let Err(e) = report::parse_date(&date) {
        return err_field(StatusCode::BAD_REQUEST, format!("{e:#}"), "date");
    }
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    blocking(move || report::agents::agent_day(&db(&path, viewer)?, &cfg, &q.id, &date)).await
}

/// One browser's (or every browser's) visits on a day.
async fn api_browser_day(State(ctx): State<Ctx>, Params(q): Params<BrowserDayQ>) -> Response {
    let cfg = ctx.web.config();
    let date = q.date.unwrap_or_else(|| util::local_date(util::now_ms()));
    if let Err(e) = report::parse_date(&date) {
        return err_field(StatusCode::BAD_REQUEST, format!("{e:#}"), "date");
    }
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    blocking(move || report::browsers::browser_day(&db(&path, viewer)?, &cfg, &q.id, &date)).await
}

/// What Claude Code's tokens in a range would cost at API prices (totals only).
async fn api_claude(State(ctx): State<Ctx>, Params(q): Params<RangeQ>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    // Still on the first read of Claude Code's history: the page says the totals will grow.
    let reading = !viewer
        && cfg.agents.claude_cost
        && ctx
            .web
            .shared
            .lock()
            .map(|s| s.claude_scan.is_none_or(|p| !p.done))
            .unwrap_or(false);
    let stats = cfg.agents.claude_cost && !viewer;
    blocking(move || {
        let conn = db(&path, viewer)?;
        let mut r = report::cost::claude(&conn, &cfg, &q.from, &q.to)?;
        r.reading = reading;
        // Older than the transcripts: Claude Code's own statistics, when the range reaches there.
        if stats {
            r.earlier = report::cost::earlier(&conn, &cfg, &report::cost::stats_paths())?
                // A lump without days: only for a range that reaches back over all of it.
                .filter(|e| q.from.as_str() <= e.from.as_str());
        }
        Ok(r)
    })
    .await
}

/// What Codex's tokens in a range would cost at OpenAI's list prices, and its plan's limits.
async fn api_codex(State(ctx): State<Ctx>, Params(q): Params<RangeQ>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    blocking(move || report::cost::codex(&db(&path, viewer)?, &cfg, &q.from, &q.to)).await
}

async fn api_hermes(State(ctx): State<Ctx>, Params(q): Params<RangeQ>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    blocking(move || report::cost::hermes(&db(&path, viewer)?, &cfg, &q.from, &q.to)).await
}

#[derive(Deserialize)]
struct WeekQ {
    /// Any day of the week wanted; today if left out.
    date: Option<String>,
}

/// A week against the one before.
async fn api_week(State(ctx): State<Ctx>, Params(q): Params<WeekQ>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    let today = util::local_date(util::now_ms());
    let date = q
        .date
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| today.clone());
    blocking(move || {
        report::week::week(&db(&path, viewer)?, &cfg, &date, &today, cache_mode(viewer))
    })
    .await
}

/// Codex's plan limits as it last reported them, and the levels the alerts use.
async fn api_limits(State(ctx): State<Ctx>) -> Response {
    let cfg = ctx.web.config();
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    blocking(move || {
        let conn = db(&path, viewer)?;
        let n = &cfg.notifications;
        Ok(json!({
            "limits": crate::sources::codex::load_limits(&conn).unwrap_or(None),
            "on": n.plan_limits && cfg.agents.codex_cost,
            "five": n.limit_5h_percent,
            "week": n.limit_week_percent,
        }))
    })
    .await
}

async fn api_settings(State(ctx): State<Ctx>) -> Response {
    let css = std::fs::read_to_string(config::custom_css_path()).unwrap_or_default();
    Json(json!({
        "config": ctx.web.config(),
        "custom_css": css,
        "defaults": Config::default(),
        "config_path": config::config_path(),
        "palettes": config::PALETTES,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SettingsBody {
    config: Config,
    // Missing means "leave custom.css alone", so an older client cannot erase it.
    #[serde(default)]
    custom_css: Option<String>,
}

/// Larger custom.css files are refused.
const CSS_LIMIT: usize = 200_000;

async fn api_settings_put(
    State(ctx): State<Ctx>,
    JsonBody(body): JsonBody<SettingsBody>,
) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    // Everything is checked before anything changes.
    if let Err(e) = body.config.validate() {
        return err_field(StatusCode::BAD_REQUEST, e.message, e.field);
    }
    if body
        .custom_css
        .as_ref()
        .is_some_and(|css| css.len() > CSS_LIMIT)
    {
        return err_field(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Custom CSS is limited to 200 KB.",
            "custom_css",
        );
    }
    let cfg = body.config.sanitized();
    let port = cfg.dashboard.port;
    let moving = ctx.web.port() != Some(port);
    if moving && let Err(e) = TcpListener::bind(("127.0.0.1", port)).await {
        return err_field(
            StatusCode::CONFLICT,
            format!("Port {port} is not available: {e}"),
            "dashboard.port",
        );
    }
    if let Some(css) = &body.custom_css
        && let Err(e) = std::fs::write(config::custom_css_path(), css)
    {
        return err_field(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not save custom.css: {e}"),
            "custom_css",
        );
    }
    if let Err(e) = cfg.save() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not save settings: {e:#}"),
        );
    }
    let mut redirect = None;
    if moving {
        match ctx.web.listen(port).await {
            Ok(()) => redirect = Some(format!("http://{}/#/settings", canonical_host(port))),
            Err(e) => {
                // Taken since the check: keep this port, and keep the config saying so.
                let mut kept = cfg;
                kept.dashboard.port = ctx.web.port().unwrap_or(ctx.port);
                let _ = kept.save();
                let _ = ctx.web.tx.send(Event::ConfigChanged(Box::new(kept)));
                return err_field(
                    StatusCode::CONFLICT,
                    format!("Port {port} is not available: {e}. Your other changes were saved."),
                    "dashboard.port",
                );
            }
        }
    }
    let _ = ctx.web.tx.send(Event::ConfigChanged(Box::new(cfg)));
    Json(json!({"ok": true, "redirect": redirect})).into_response()
}

#[derive(Deserialize)]
struct AppearanceBody {
    theme: Option<String>,
    palette: Option<String>,
    dark_flavor: Option<String>,
    accent: Option<String>,
    theme_categories: Option<bool>,
    time_format: Option<String>,
    show_ms: Option<bool>,
    density: Option<String>,
    motion: Option<String>,
}

/// Saves only the look (from the header's theme picker) without touching other settings.
async fn api_appearance(State(ctx): State<Ctx>, JsonBody(b): JsonBody<AppearanceBody>) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    // The file is the newest truth: two quick changes must not undo each other.
    // Refuse rather than write the running config over a file the user is fixing.
    let mut cfg = match Config::load() {
        Ok(c) => c,
        // No file at all (removed by hand): the running settings are the newest truth.
        Err(_) if !config::config_path().exists() => ctx.web.config(),
        // The parse error itself is in the dashboard's notices; the picker only needs the gist.
        Err(_) => {
            return err(
                StatusCode::CONFLICT,
                "config.toml has an error, so this look wasn't saved. Fix the file, or save Settings to replace it.",
            );
        }
    };
    let d = &mut cfg.dashboard;
    if let Some(v) = b.palette {
        if v != d.palette {
            // A new palette starts from its own default flavour and accent.
            d.dark_flavor.clear();
            d.accent.clear();
        }
        d.palette = v;
    }
    if let Some(v) = b.theme {
        d.theme = v;
    }
    if let Some(v) = b.dark_flavor {
        d.dark_flavor = v;
    }
    if let Some(v) = b.accent {
        d.accent = v;
    }
    if let Some(v) = b.theme_categories {
        d.theme_categories = v;
    }
    if let Some(v) = b.time_format {
        d.time_format = v;
    }
    if let Some(v) = b.show_ms {
        d.show_ms = v;
    }
    if let Some(v) = b.density {
        d.density = v;
    }
    if let Some(v) = b.motion {
        d.motion = v;
    }
    let cfg = cfg.sanitized();
    if let Err(e) = cfg.save() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not save settings: {e:#}"),
        );
    }
    let dashboard = cfg.dashboard.clone();
    let _ = ctx.web.tx.send(Event::ConfigChanged(Box::new(cfg)));
    Json(json!({"ok": true, "dashboard": dashboard})).into_response()
}

/// A one-click rule from a dashboard row's menu. The file is the newest truth, as for the look.
async fn api_rule(State(ctx): State<Ctx>, JsonBody(rule): JsonBody<config::Rule>) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    let mut cfg = match Config::load() {
        Ok(c) => c,
        Err(_) if !config::config_path().exists() => ctx.web.config(),
        Err(_) => {
            return err(
                StatusCode::CONFLICT,
                "config.toml has an error, so this rule wasn't saved. Fix the file, or save Settings to replace it.",
            );
        }
    };
    if let Err(e) = cfg.apply_rule(&rule) {
        return err_field(StatusCode::BAD_REQUEST, e.message, e.field);
    }
    if let Err(e) = cfg.validate() {
        return err_field(StatusCode::BAD_REQUEST, e.message, e.field);
    }
    let cfg = cfg.sanitized();
    if let Err(e) = cfg.save() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not save settings: {e:#}"),
        );
    }
    let _ = ctx.web.tx.send(Event::ConfigChanged(Box::new(cfg)));
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct PlanBody {
    agent: String,
    /// A plan id from config::PLANS; empty for none.
    plan: String,
}

/// Saves the plan you pay for an agent (the plan picker on its page).
async fn api_plan(State(ctx): State<Ctx>, JsonBody(b): JsonBody<PlanBody>) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    let mut cfg = match Config::load() {
        Ok(c) => c,
        Err(_) if !config::config_path().exists() => ctx.web.config(),
        Err(_) => {
            return err(
                StatusCode::CONFLICT,
                "config.toml has an error, so the plan wasn't saved. Fix the file, or save Settings to replace it.",
            );
        }
    };
    if b.plan.is_empty() {
        cfg.agents.plans.remove(&b.agent);
    } else {
        cfg.agents.plans.insert(b.agent, b.plan);
    }
    if let Err(e) = cfg.validate() {
        return err_field(StatusCode::BAD_REQUEST, e.message, e.field);
    }
    let cfg = cfg.sanitized();
    if let Err(e) = cfg.save() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not save settings: {e:#}"),
        );
    }
    let _ = ctx.web.tx.send(Event::ConfigChanged(Box::new(cfg)));
    Json(json!({"ok": true})).into_response()
}

const VIEWER_MSG: &str = "This is a read-only view (arch-tracker serve). Change settings in the tracker's own dashboard.";

async fn control(ctx: &Ctx, req: Request) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    let (otx, orx) = oneshot::channel();
    if ctx.web.tx.send(Event::Control(req, otx)).is_err() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "Daemon is stopping");
    }
    match orx.await {
        Ok(v) => Json(v).into_response(),
        Err(_) => err(StatusCode::SERVICE_UNAVAILABLE, "Daemon is stopping"),
    }
}

#[derive(Deserialize)]
struct PauseBody {
    /// None = until resumed.
    minutes: Option<u64>,
}

async fn api_pause(State(ctx): State<Ctx>, JsonBody(b): JsonBody<PauseBody>) -> Response {
    if b.minutes.is_some_and(|m| !(1..=7 * 24 * 60).contains(&m)) {
        return err_field(
            StatusCode::BAD_REQUEST,
            "A pause lasts between 1 minute and 7 days.",
            "minutes",
        );
    }
    let until_ms = b.minutes.map(|m| util::now_ms() + m as i64 * 60_000);
    control(&ctx, Request::Pause { until_ms }).await
}

async fn api_resume(State(ctx): State<Ctx>) -> Response {
    control(&ctx, Request::Resume).await
}

/// Clears the day cache, so Trends rebuilds every past day from the raw data on next view.
async fn api_recompute(State(ctx): State<Ctx>) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    let path = ctx.web.db.clone();
    blocking(move || {
        let conn = db(&path, false)?;
        let cleared = conn.execute("DELETE FROM rollups", [])?;
        Ok(json!({"ok": true, "cleared": cleared}))
    })
    .await
}

/// Shows a sample notification, so Settings can prove notifications reach the desktop.
async fn api_notify_test(State(ctx): State<Ctx>) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    let Some(tx) = ctx.web.shared.lock().ok().and_then(|s| s.alerts.clone()) else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Notifications are not running.",
        );
    };
    let (otx, orx) = oneshot::channel();
    if tx.send(crate::alerts::Cmd::Test(otx)).is_err() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Notifications are not running.",
        );
    }
    match orx.await {
        Ok(Ok(())) => Json(json!({"ok": true})).into_response(),
        Ok(Err(e)) => err(
            StatusCode::BAD_GATEWAY,
            format!("The desktop did not show it: {e}"),
        ),
        Err(_) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Notifications are not running.",
        ),
    }
}

#[derive(Deserialize)]
struct FoldersQ {
    path: Option<String>,
    #[serde(default)]
    suggest: bool,
}

/// Folders inside the home folder, for choosing project folders in Settings. A page never learns
/// real paths from the browser's own folder picker, so the tracker lists them itself.
async fn api_folders(State(ctx): State<Ctx>, Params(q): Params<FoldersQ>) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    let cfg = ctx.web.config();
    let db_path = ctx.web.db.clone();
    blocking(move || {
        let home = util::home();
        let want = q.path.unwrap_or_else(|| "~".into());
        let mut v = serde_json::to_value(list_folders(&want, &home)?)?;
        if q.suggest {
            let conn = crate::store::open_readonly(&db_path)?;
            v["suggested"] = json!(suggested_folders(&conn, &home, &cfg.projects)?);
        }
        Ok(v)
    })
    .await
}

/// At most this many folders are listed from one folder.
const MAX_FOLDERS: usize = 1000;

#[derive(Serialize, Debug)]
struct FolderEntry {
    name: String,
    path: String,
    git: bool,
}

#[derive(Serialize, Debug)]
struct Listing {
    path: String,
    parent: Option<String>,
    dirs: Vec<FolderEntry>,
    truncated: bool,
}

/// "~/x" form of a path inside home (canonical forms on both sides).
fn tilde(p: &std::path::Path, home: &std::path::Path) -> String {
    match p.strip_prefix(home) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

/// The folder a "~/x" or absolute path names, if it is the home folder or inside it.
fn home_folder(want: &str, home: &std::path::Path) -> Result<std::path::PathBuf> {
    let want = want.trim();
    let raw = match want.strip_prefix('~') {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => std::path::PathBuf::from(want),
    };
    let outside =
        || report::BadRequest("Only folders inside your home folder can be chosen.".into());
    let home = home.canonicalize()?;
    let dir = raw
        .canonicalize()
        .map_err(|_| report::BadRequest(format!("{want} is not a folder.")))?;
    if !dir.starts_with(&home) || !dir.is_dir() {
        return Err(outside().into());
    }
    Ok(dir)
}

/// The visible subfolders of a folder inside home. Symlinks are not followed, so nothing outside
/// home is listed.
fn list_folders(want: &str, home: &std::path::Path) -> Result<Listing> {
    let dir = home_folder(want, home)?;
    let home = home.canonicalize()?;
    let mut dirs = vec![];
    for e in std::fs::read_dir(&dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        dirs.push(FolderEntry {
            git: e.path().join(".git").exists(),
            path: tilde(&e.path(), &home),
            name,
        });
    }
    dirs.sort_by_cached_key(|d| d.name.to_lowercase());
    let truncated = dirs.len() > MAX_FOLDERS;
    dirs.truncate(MAX_FOLDERS);
    Ok(Listing {
        path: tilde(&dir, &home),
        parent: (dir != home).then(|| tilde(dir.parent().unwrap_or(&home), &home)),
        dirs,
        truncated,
    })
}

/// Terminal folders with the most focused time in the last 90 days that don't count as projects
/// yet and still exist inside home: the likely candidates.
fn suggested_folders(
    conn: &rusqlite::Connection,
    home: &std::path::Path,
    projects: &config::Projects,
) -> Result<Vec<serde_json::Value>> {
    let since = util::now_ms() - 90 * 86_400_000;
    let mut st = conn.prepare(
        "SELECT s.project, SUM(sp.end_ms - sp.start_ms) FROM spans sp
         JOIN subjects s ON s.id = sp.subject_id
         WHERE sp.kind = ?1 AND sp.start_ms >= ?2 AND s.project != ''
         GROUP BY s.project ORDER BY 2 DESC LIMIT 200",
    )?;
    let rows = st.query_map(
        rusqlite::params![crate::model::Kind::Focus as i64, since],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )?;
    let mut out = vec![];
    for row in rows {
        let (path, ms) = row?;
        if out.len() >= 20 {
            break;
        }
        if report::counts_as_project(projects, &path) || home_folder(&path, home).is_err() {
            continue;
        }
        out.push(json!({"path": path, "secs": ms / 1000}));
    }
    Ok(out)
}

#[derive(Deserialize)]
struct ExportQ {
    from: String,
    to: String,
    #[serde(default)]
    format: Option<String>,
}

async fn api_export(State(ctx): State<Ctx>, Params(q): Params<ExportQ>) -> Response {
    let format = match export::Format::parse(q.format.as_deref().unwrap_or("csv")) {
        Ok(f) => f,
        Err(e) => return err_field(StatusCode::BAD_REQUEST, format!("{e:#}"), "format"),
    };
    let range = match export::bounds(&q.from, &q.to) {
        Ok(r) => r,
        Err(e) => return err_field(StatusCode::BAD_REQUEST, format!("{e:#}"), "from"),
    };
    let path = ctx.web.db.clone();
    let viewer = ctx.web.viewer;
    // Rows travel from a blocking task to the response through a small bounded channel, so a
    // year of data is never held in memory (the unit's MemoryMax is 64 MB).
    let (tx, rx) = mpsc::channel::<std::io::Result<Bytes>>(4);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
    tokio::task::spawn_blocking(move || {
        let conn = match db(&path, viewer) {
            Ok(c) => {
                let _ = ready_tx.send(Ok(()));
                c
            }
            Err(e) => {
                let _ = ready_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        let mut out = ChunkWriter {
            tx: tx.clone(),
            buf: Vec::with_capacity(CHUNK),
        };
        if let Err(e) = export::write(&conn, range, format, &mut out) {
            // Ends the download as failed (unless the browser already went away).
            let _ = tx.blocking_send(Err(std::io::Error::other(format!("{e:#}"))));
        }
    });
    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "The export stopped."),
    }
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    (
        [
            (header::CONTENT_TYPE, format.content_type().to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", format.file_name()),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

const CHUNK: usize = 64 * 1024;

/// io::Write that hands 64 KiB chunks to the response stream, waiting while the channel is full.
struct ChunkWriter {
    tx: mpsc::Sender<std::io::Result<Bytes>>,
    buf: Vec<u8>,
}

impl ChunkWriter {
    fn send(&mut self) -> std::io::Result<()> {
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        self.tx
            .blocking_send(Ok(Bytes::from(chunk)))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "download cancelled"))
    }
}

impl std::io::Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.send()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.send()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_list_only_visible_subfolders_inside_home() {
        let home = std::env::temp_dir().join(format!("at-folders-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("projects/app/.git")).unwrap();
        std::fs::create_dir_all(home.join("projects/Notes")).unwrap();
        std::fs::create_dir_all(home.join("projects/.cache")).unwrap();
        std::fs::write(home.join("projects/readme.txt"), "x").unwrap();
        std::os::unix::fs::symlink("/", home.join("projects/root")).unwrap();
        let l = list_folders("~/projects", &home).unwrap();
        let names: Vec<&str> = l.dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["app", "Notes"],
            "no hidden folders, files or symlinks"
        );
        assert!(l.dirs[0].git && !l.dirs[1].git);
        assert_eq!(l.dirs[0].path, "~/projects/app");
        assert_eq!(l.path, "~/projects");
        assert_eq!(l.parent.as_deref(), Some("~"));
        let top = list_folders("~", &home).unwrap();
        assert_eq!(top.parent, None);
        assert!(list_folders("/", &home).is_err(), "outside home");
        assert!(
            list_folders("~/projects/root", &home).is_err(),
            "a symlink out of home"
        );
        assert!(list_folders("~/../", &home).is_err());
        assert!(list_folders("~/missing", &home).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn rejections_name_the_field() {
        let (msg, field) = explain_rejection(
            "Failed to deserialize the JSON body into the target type: config.dashboard.port: \
             invalid value: integer `70000`, expected u16 at line 1 column 512",
        );
        assert_eq!(field.as_deref(), Some("dashboard.port"));
        assert_eq!(
            msg,
            "Invalid value for dashboard.port: invalid value: integer `70000`, expected u16."
        );
        let (msg, field) = explain_rejection(
            "Failed to deserialize the JSON body into the target type: config.idle.threshold_secs: \
             invalid type: floating point `1.5`, expected u64 at line 1 column 40",
        );
        assert_eq!(field.as_deref(), Some("idle.threshold_secs"));
        assert!(msg.contains("floating point"));
        let (msg, field) = explain_rejection(
            "Failed to parse the request body as JSON: expected value at line 1 column 1",
        );
        assert_eq!(field, None);
        assert_eq!(msg, "The request could not be read: expected value.");
        let (msg, field) =
            explain_rejection("Expected request with `Content-Type: application/json`");
        assert_eq!(field, None);
        assert!(msg.contains("Content-Type: application/json"));
        let (_, field) =
            explain_rejection("Failed to deserialize query string: missing field `from`");
        assert_eq!(field, None);
    }
}
