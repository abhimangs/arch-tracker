//! Local dashboard: axum on 127.0.0.1 only, embedded assets, JSON API.
//! Every write needs the per-run CSRF token; Host headers other than localhost are refused
//! (stops DNS-rebinding pages from reading your data).

use crate::config::{self, Config};
use crate::daemon::{Event, SharedRef};
use crate::ipc::Request;
use crate::report;
use crate::util;
use anyhow::Result;
use axum::extract::{Path, Query, Request as HttpRequest, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

#[derive(rust_embed::Embed)]
#[folder = "web-ui/"]
struct Assets;

pub struct Web {
    shared: SharedRef,
    tx: UnboundedSender<Event>,
    token: String,
    current: Mutex<Option<(u16, oneshot::Sender<()>)>>,
}

#[derive(Clone)]
struct Ctx {
    web: Arc<Web>,
    port: u16,
}

fn random_token() -> String {
    let mut b = [0u8; 24];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Web {
    pub fn new(shared: SharedRef, tx: UnboundedSender<Event>) -> Arc<Web> {
        Arc::new(Web {
            shared,
            tx,
            token: random_token(),
            current: Mutex::new(None),
        })
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
        tracing::info!("dashboard on http://127.0.0.1:{port}/");
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

/// Host-header allowlist + CSRF token on every non-GET request.
async fn guard(State(ctx): State<Ctx>, req: HttpRequest, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let ok_hosts = [
        format!("127.0.0.1:{}", ctx.port),
        format!("localhost:{}", ctx.port),
        format!("[::1]:{}", ctx.port),
    ];
    if !ok_hosts.iter().any(|h| h == host) {
        return (
            StatusCode::FORBIDDEN,
            "Blocked: this dashboard only answers on 127.0.0.1",
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
            Some(o) => ok_hosts.iter().any(|h| o == format!("http://{h}")),
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

async fn index(State(ctx): State<Ctx>) -> Response {
    let Some(f) = Assets::get("index.html") else {
        return err(StatusCode::NOT_FOUND, "missing index.html");
    };
    let html = String::from_utf8_lossy(&f.data)
        .replace("__TOKEN__", &ctx.web.token)
        .replace("__THEME__", &ctx.web.config().dashboard.theme)
        .replace("__VERSION__", env!("CARGO_PKG_VERSION"));
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
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

fn db() -> Result<rusqlite::Connection> {
    let c = rusqlite::Connection::open(util::db_path())?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(c)
}

async fn api_day(State(ctx): State<Ctx>, Query(q): Query<DayQ>) -> Response {
    let cfg = ctx.web.config();
    let date = q.date.unwrap_or_else(|| util::local_date(util::now_ms()));
    blocking(move || report::day_cached(&db()?, &cfg, &date)).await
}

#[derive(Deserialize)]
struct RangeQ {
    from: String,
    to: String,
}

async fn api_range(State(ctx): State<Ctx>, Query(q): Query<RangeQ>) -> Response {
    let cfg = ctx.web.config();
    blocking(move || report::range(&db()?, &cfg, &q.from, &q.to)).await
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
            "http://127.0.0.1:{}/#/settings",
            cfg.dashboard.port
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

async fn control(ctx: &Ctx, req: Request) -> Response {
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

async fn api_export(Query(q): Query<ExportQ>) -> Response {
    let format = q.format.unwrap_or_else(|| "csv".into());
    let res =
        tokio::task::spawn_blocking(move || crate::export::export(&db()?, &q.from, &q.to, &format))
            .await;
    match res {
        Ok(Ok((ct, body))) => {
            let name = if ct.contains("csv") {
                "arch-tracker.csv"
            } else {
                "arch-tracker.json"
            };
            (
                [
                    (header::CONTENT_TYPE, ct.to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{name}\""),
                    ),
                ],
                body,
            )
                .into_response()
        }
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
