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
use axum::extract::{Path, Query, Request as HttpRequest, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
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
        .route("/api/day", get(api_day))
        .route("/api/range", get(api_range))
        .route("/api/settings", get(api_settings).put(api_settings_put))
        .route("/api/pause", post(api_pause))
        .route("/api/resume", post(api_resume))
        .route("/api/export", get(api_export))
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

async fn api_status(State(ctx): State<Ctx>) -> Response {
    let mut v = ctx
        .web
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
        let dash_err = ctx.web.error.lock().ok().and_then(|e| e.clone());
        o.insert("dashboard_error".into(), json!(dash_err));
    }
    Json(v).into_response()
}

#[derive(Deserialize)]
struct DayQ {
    date: Option<String>,
}

async fn blocking<T: serde::Serialize + Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(f).await {
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

async fn api_day(State(ctx): State<Ctx>, Query(q): Query<DayQ>) -> Response {
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

async fn api_range(State(ctx): State<Ctx>, Query(q): Query<RangeQ>) -> Response {
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

async fn api_settings(State(ctx): State<Ctx>) -> Response {
    let css = std::fs::read_to_string(config::custom_css_path()).unwrap_or_default();
    Json(json!({
        "config": ctx.web.config(),
        "custom_css": css,
        "defaults": Config::default(),
        "config_path": config::config_path(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SettingsBody {
    config: Config,
    #[serde(default)]
    custom_css: String,
}

async fn api_settings_put(State(ctx): State<Ctx>, Json(body): Json<SettingsBody>) -> Response {
    if ctx.web.viewer {
        return err(StatusCode::FORBIDDEN, VIEWER_MSG);
    }
    let cfg = body.config.sanitized();
    let old = ctx.web.config();
    let mut redirect = None;
    if cfg.dashboard.port != old.dashboard.port {
        if let Err(e) = ctx.web.listen(cfg.dashboard.port).await {
            return err(
                StatusCode::CONFLICT,
                format!("Port {} is not available: {e}", cfg.dashboard.port),
            );
        }
        redirect = Some(format!(
            "http://{}/#/settings",
            canonical_host(cfg.dashboard.port)
        ));
    }
    if let Err(e) = cfg.save() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not save settings: {e:#}"),
        );
    }
    if body.custom_css.len() > 200_000 {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Custom CSS is limited to 200 KB",
        );
    }
    let _ = std::fs::write(config::custom_css_path(), &body.custom_css);
    let _ = ctx.web.tx.send(Event::ConfigChanged(Box::new(cfg)));
    Json(json!({"ok": true, "redirect": redirect})).into_response()
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

async fn api_pause(State(ctx): State<Ctx>, Json(b): Json<PauseBody>) -> Response {
    let until_ms = b.minutes.map(|m| util::now_ms() + m as i64 * 60_000);
    control(&ctx, Request::Pause { until_ms }).await
}

async fn api_resume(State(ctx): State<Ctx>) -> Response {
    control(&ctx, Request::Resume).await
}

#[derive(Deserialize)]
struct ExportQ {
    from: String,
    to: String,
    #[serde(default)]
    format: Option<String>,
}

async fn api_export(State(ctx): State<Ctx>, Query(q): Query<ExportQ>) -> Response {
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
