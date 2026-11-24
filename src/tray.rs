//! System-tray icon: a StatusNotifierItem (the freedesktop/KDE tray protocol) with a
//! com.canonical.dbusmenu menu, served on the daemon's session-bus connection.
//! Tooltip: today's screen time. Click: the dashboard, signed in, in the default browser.
//! Middle click or the menu: pause / resume.

use crate::daemon::{Event, SharedRef};
use crate::ipc::Request;
use crate::web::Web;
use anyhow::Result;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use zbus::object_server::SignalEmitter;
use zbus::wire::{OwnedVariant, Variant};
use zbus::{Connection, OwnedObjectPath};

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
/// How long a computed "today" total is reused.
const TODAY_TTL_MS: i64 = 55_000;

type Pixmap = (i32, i32, Vec<u8>);

struct State {
    conn: Connection,
    shared: SharedRef,
    tx: UnboundedSender<Event>,
    web: Arc<Web>,
    db: PathBuf,
    /// (computed at, screen seconds today)
    today: Mutex<(i64, Option<i64>)>,
    /// XDG activation token from the tray host, so the browser may take focus on Wayland.
    token: Mutex<Option<String>>,
    revision: AtomicU32,
    /// What the menu and icon last showed: (paused, label).
    shown: Mutex<(bool, String)>,
}

impl State {
    fn paused(&self) -> (bool, Option<i64>) {
        self.shared
            .lock()
            .map(|s| {
                (
                    s.status["paused"].as_bool().unwrap_or(false),
                    s.status["paused_until_ms"].as_i64(),
                )
            })
            .unwrap_or((false, None))
    }

    async fn today_secs(&self) -> Option<i64> {
        let now = crate::util::now_ms();
        if let Ok(t) = self.today.lock()
            && now - t.0 < TODAY_TTL_MS
            && t.1.is_some()
        {
            return t.1;
        }
        let cfg = self
            .shared
            .lock()
            .map(|s| s.config.clone())
            .unwrap_or_default();
        let db = self.db.clone();
        let secs = tokio::task::spawn_blocking(move || -> Result<i64> {
            let conn = crate::store::open_readonly(&db)?;
            let date = crate::util::local_date(crate::util::now_ms());
            Ok(crate::report::build_day(&conn, &cfg, &date)?.totals.screen)
        })
        .await
        .ok()
        .and_then(|r| r.ok());
        if let Ok(mut t) = self.today.lock() {
            *t = (now, secs);
        }
        secs
    }

    async fn headline(&self) -> String {
        let (paused, until) = self.paused();
        let today = match self.today_secs().await {
            Some(s) => format!("{} on screen today", fmt_dur(s)),
            None => "arch-tracker".into(),
        };
        if paused {
            match until.and_then(clock) {
                Some(t) => format!("{today} · paused until {t}"),
                None => format!("{today} · paused"),
            }
        } else {
            today
        }
    }

    async fn control(&self, req: Request) {
        let (otx, orx) = oneshot::channel();
        if self.tx.send(Event::Control(req, otx)).is_ok() {
            let _ = orx.await;
        }
    }

    async fn pause(&self, minutes: Option<i64>) {
        let until_ms = minutes.map(|m| crate::util::now_ms() + m * 60_000);
        self.control(Request::Pause { until_ms }).await;
    }

    async fn open(&self, route: &str) {
        let token = self.token.lock().ok().and_then(|mut t| t.take());
        open_dashboard(&self.conn, &self.web, route, token).await;
    }
}

/// Opens the dashboard, signed in, through the desktop portal, so the browser starts in its own
/// session scope instead of inside this service (and its memory limit). `token` is an XDG
/// activation token (from the tray host or a clicked notification) that lets it take focus.
pub async fn open_dashboard(conn: &Connection, web: &Web, route: &str, token: Option<String>) {
    let url = match web.login_url(route) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("cannot open the dashboard: {e:#}");
            return;
        }
    };
    let mut opts: HashMap<&str, Variant<'_>> = HashMap::new();
    if let Some(t) = token.as_deref() {
        opts.insert("activation_token", Variant::from(t));
    }
    let portal = conn
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "OpenURI",
            &("", url.as_str(), opts),
        )
        .await;
    if let Err(e) = portal {
        tracing::info!("portal OpenURI failed ({e}); using systemd-run xdg-open");
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.args(["--user", "--collect", "--quiet", "--", "xdg-open", &url]);
        if let Some(t) = token {
            cmd.env("XDG_ACTIVATION_TOKEN", t);
        }
        match cmd.status().await {
            Ok(s) if s.success() => {}
            other => tracing::warn!("could not open the dashboard: {other:?}"),
        }
    }
}

/// "2h 05m", "12m", "40s".
pub(crate) fn fmt_dur(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// Local "HH:MM" for a unix-ms time.
fn clock(ms: i64) -> Option<String> {
    let ts = jiff::Timestamp::from_millisecond(ms).ok()?;
    Some(
        ts.to_zoned_datetime(jiff::tz::TimeZone::system())
            .strftime("%H:%M")
            .to_string(),
    )
}

// ---------- icon ----------

#[derive(Clone, Copy)]
struct Shape {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    rgb: u32,
}

/// The app icon (web-ui/icon.svg), in its 32-unit grid. Paused: grey bars, no "now" line.
fn shapes(paused: bool) -> Vec<Shape> {
    let s = |x, y, w, h, r, rgb| Shape { x, y, w, h, r, rgb };
    let bar = |rgb: u32| if paused { 0x6b7392 } else { rgb };
    let mut v = vec![
        s(0.0, 0.0, 32.0, 32.0, 7.0, 0x121a33),
        s(5.0, 17.0, 3.0, 9.0, 1.0, bar(0x6f9ee8)),
        s(10.0, 11.0, 3.0, 15.0, 1.0, bar(0x6f9ee8)),
        s(15.0, 14.0, 3.0, 12.0, 1.0, bar(0xb49be8)),
        s(20.0, 8.0, 3.0, 18.0, 1.0, bar(0xef6f5e)),
        s(25.0, 19.0, 3.0, 7.0, 1.0, bar(0x7cc59a)),
    ];
    if !paused {
        v.push(s(18.6, 4.0, 1.2, 24.0, 0.0, 0xe5533d));
    }
    v
}

fn inside(sh: &Shape, px: f32, py: f32) -> bool {
    if px < sh.x || py < sh.y || px > sh.x + sh.w || py > sh.y + sh.h {
        return false;
    }
    let r = sh.r.min(sh.w / 2.0).min(sh.h / 2.0);
    let cx = px.clamp(sh.x + r, sh.x + sh.w - r);
    let cy = py.clamp(sh.y + r, sh.y + sh.h - r);
    (px - cx).powi(2) + (py - cy).powi(2) <= r * r
}

/// ARGB32 in network byte order, as StatusNotifierItem wants; 4x4 supersampled.
fn render(size: i32, paused: bool) -> Pixmap {
    const SS: i32 = 4;
    let shapes = shapes(paused);
    let scale = 32.0 / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut a, mut r, mut g, mut b) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = (x as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let py = (y as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    if let Some(sh) = shapes.iter().rev().find(|sh| inside(sh, px, py)) {
                        a += 1.0;
                        r += ((sh.rgb >> 16) & 0xff) as f32;
                        g += ((sh.rgb >> 8) & 0xff) as f32;
                        b += (sh.rgb & 0xff) as f32;
                    }
                }
            }
            let n = (SS * SS) as f32;
            if a == 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                // Straight (non-premultiplied) alpha.
                out.extend_from_slice(&[
                    (a / n * 255.0).round() as u8,
                    (r / a).round() as u8,
                    (g / a).round() as u8,
                    (b / a).round() as u8,
                ]);
            }
        }
    }
    (size, size, out)
}

fn pixmaps(paused: bool) -> Vec<Pixmap> {
    [16, 22, 24, 32, 48, 64]
        .iter()
        .map(|&s| render(s, paused))
        .collect()
}

// ---------- StatusNotifierItem ----------

struct Item {
    st: Arc<State>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "arch-tracker"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "arch-tracker"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        pixmaps(self.st.paused().0)
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Vec<Pixmap> {
        vec![]
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<Pixmap> {
        vec![]
    }

    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    async fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        (
            String::new(),
            vec![],
            "arch-tracker".into(),
            self.st.headline().await,
        )
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(MENU_PATH).expect("valid object path")
    }

    async fn activate(&self, _x: i32, _y: i32) {
        self.st.open("today").await;
    }

    async fn secondary_activate(&self, _x: i32, _y: i32) {
        if self.st.paused().0 {
            self.st.control(Request::Resume).await;
        } else {
            self.st.pause(None).await;
        }
        refresh(&self.st, true).await;
    }

    async fn context_menu(&self, _x: i32, _y: i32) {}

    async fn scroll(&self, _delta: i32, _orientation: &str) {}

    async fn provide_xdg_activation_token(&self, token: String) {
        if let Ok(mut t) = self.st.token.lock() {
            *t = Some(token);
        }
    }

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

// ---------- com.canonical.dbusmenu ----------

const M_HEAD: i32 = 1;
const M_OPEN: i32 = 3;
const M_PAUSE30: i32 = 5;
const M_PAUSE60: i32 = 6;
const M_PAUSE: i32 = 7;
const M_RESUME: i32 = 8;
const M_SETTINGS: i32 = 10;

type Props = HashMap<String, OwnedVariant>;
type Node = (i32, Props, Vec<OwnedVariant>);

fn ov<'a>(v: impl Into<Variant<'a>>) -> OwnedVariant {
    OwnedVariant::try_from(v.into()).expect("plain values convert")
}

/// The menu items: (id, properties) in display order.
fn items(paused: bool, head: &str) -> Vec<(i32, Props)> {
    let item = |label: &str, icon: &str, visible: bool| -> Props {
        let mut p = Props::new();
        p.insert("label".into(), ov(label.to_string()));
        if !icon.is_empty() {
            p.insert("icon-name".into(), ov(icon.to_string()));
        }
        p.insert("visible".into(), ov(visible));
        p
    };
    let sep = || -> Props {
        let mut p = Props::new();
        p.insert("type".into(), ov("separator".to_string()));
        p
    };
    let mut headline = item(head, "", true);
    headline.insert("enabled".into(), ov(false));
    vec![
        (M_HEAD, headline),
        (2, sep()),
        (M_OPEN, item("Open dashboard", "view-statistics", true)),
        (4, sep()),
        (
            M_PAUSE30,
            item("Pause for 30 minutes", "media-playback-pause", !paused),
        ),
        (
            M_PAUSE60,
            item("Pause for 1 hour", "media-playback-pause", !paused),
        ),
        (
            M_PAUSE,
            item("Pause until I resume", "media-playback-pause", !paused),
        ),
        (
            M_RESUME,
            item("Resume tracking", "media-playback-start", paused),
        ),
        (9, sep()),
        (M_SETTINGS, item("Settings", "configure", true)),
    ]
}

fn filter(p: &Props, names: &[String]) -> Props {
    if names.is_empty() {
        return p.clone();
    }
    p.iter()
        .filter(|(k, _)| names.iter().any(|n| n == *k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

struct Menu {
    st: Arc<State>,
}

impl Menu {
    fn current(&self) -> Vec<(i32, Props)> {
        let (paused, head) = self
            .st
            .shown
            .lock()
            .map(|s| (s.0, s.1.clone()))
            .unwrap_or_default();
        items(paused, &head)
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        vec![]
    }

    async fn get_layout(
        &self,
        parent_id: i32,
        _recursion_depth: i32,
        property_names: Vec<String>,
    ) -> (u32, Node) {
        let rev = self.st.revision.load(Ordering::Relaxed);
        let all = self.current();
        if parent_id != 0 {
            let props = all
                .iter()
                .find(|(id, _)| *id == parent_id)
                .map(|(_, p)| filter(p, &property_names))
                .unwrap_or_default();
            return (rev, (parent_id, props, vec![]));
        }
        let children = all
            .iter()
            .map(|(id, p)| {
                let node: Node = (*id, filter(p, &property_names), vec![]);
                ov(node)
            })
            .collect();
        let mut root = Props::new();
        root.insert("children-display".into(), ov("submenu".to_string()));
        (rev, (0, root, children))
    }

    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, Props)> {
        self.current()
            .into_iter()
            .filter(|(id, _)| ids.is_empty() || ids.contains(id))
            .map(|(id, p)| (id, filter(&p, &property_names)))
            .collect()
    }

    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedVariant> {
        self.current()
            .into_iter()
            .find(|(i, _)| *i == id)
            .and_then(|(_, p)| p.get(&name).cloned())
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no property {name} on {id}")))
    }

    async fn event(&self, id: i32, event_id: String, _data: OwnedVariant, _timestamp: u32) {
        if event_id == "clicked" {
            click(&self.st, id).await;
        }
    }

    async fn event_group(&self, events: Vec<(i32, String, OwnedVariant, u32)>) -> Vec<i32> {
        for (id, ev, _, _) in events {
            if ev == "clicked" {
                click(&self.st, id).await;
            }
        }
        vec![]
    }

    async fn about_to_show(&self, _id: i32) -> bool {
        refresh(&self.st, false).await
    }

    async fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        let changed = refresh(&self.st, false).await;
        (if changed { vec![0] } else { vec![] }, vec![])
    }

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

async fn click(st: &Arc<State>, id: i32) {
    match id {
        M_OPEN | M_HEAD => st.open("today").await,
        M_SETTINGS => st.open("settings").await,
        M_PAUSE30 => st.pause(Some(30)).await,
        M_PAUSE60 => st.pause(Some(60)).await,
        M_PAUSE => st.pause(None).await,
        M_RESUME => st.control(Request::Resume).await,
        _ => return,
    }
    refresh(st, true).await;
}

/// Re-reads pause state and today's total; tells the tray host what changed.
/// Returns whether the menu changed. `tooltip` also re-announces the tooltip.
async fn refresh(st: &Arc<State>, tooltip: bool) -> bool {
    let paused = st.paused().0;
    let head = st.headline().await;
    let (menu_changed, icon_changed) = match st.shown.lock() {
        Ok(mut s) => {
            let icon = s.0 != paused;
            let menu = icon || s.1 != head;
            *s = (paused, head);
            (menu, icon)
        }
        Err(_) => (false, false),
    };
    let server = st.conn.object_server();
    if menu_changed {
        let rev = st.revision.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(m) = server.interface::<_, Menu>(MENU_PATH).await {
            let _ = Menu::layout_updated(m.signal_emitter(), rev, 0).await;
        }
    }
    if let Ok(i) = server.interface::<_, Item>(ITEM_PATH).await {
        if icon_changed {
            let _ = Item::new_icon(i.signal_emitter()).await;
        }
        if tooltip || menu_changed {
            let _ = Item::new_tool_tip(i.signal_emitter()).await;
        }
    }
    menu_changed
}

// ---------- lifecycle ----------

pub struct Tray {
    st: Arc<State>,
    name: String,
    enabled: AtomicBool,
}

impl Tray {
    pub fn new(
        conn: Connection,
        shared: SharedRef,
        tx: UnboundedSender<Event>,
        web: Arc<Web>,
        db: PathBuf,
    ) -> Arc<Tray> {
        let st = Arc::new(State {
            conn,
            shared,
            tx,
            web,
            db,
            today: Mutex::new((0, None)),
            token: Mutex::new(None),
            revision: AtomicU32::new(1),
            shown: Mutex::new((false, "arch-tracker".into())),
        });
        let tray = Arc::new(Tray {
            st,
            name: format!("org.kde.StatusNotifierItem-{}-1", std::process::id()),
            enabled: AtomicBool::new(false),
        });
        tokio::spawn(watch(tray.clone()));
        tray
    }

    /// Shows or hides the icon (the [dashboard] tray setting).
    pub async fn set_enabled(&self, on: bool) {
        if self.enabled.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if on {
            if let Err(e) = self.show().await {
                tracing::warn!("tray icon unavailable: {e:#}");
            }
        } else {
            let server = self.st.conn.object_server();
            let _ = self.st.conn.release_name(self.name.as_str()).await;
            let _ = server.remove::<Item, _>(ITEM_PATH).await;
            let _ = server.remove::<Menu, _>(MENU_PATH).await;
        }
    }

    async fn show(&self) -> Result<()> {
        let server = self.st.conn.object_server();
        server
            .at(
                ITEM_PATH,
                Item {
                    st: self.st.clone(),
                },
            )
            .await?;
        server
            .at(
                MENU_PATH,
                Menu {
                    st: self.st.clone(),
                },
            )
            .await?;
        self.st.conn.request_name(self.name.as_str()).await?;
        refresh(&self.st, false).await;
        self.register().await
    }

    async fn register(&self) -> Result<()> {
        self.st
            .conn
            .call_method(
                Some(WATCHER),
                "/StatusNotifierWatcher",
                Some(WATCHER),
                "RegisterStatusNotifierItem",
                &(self.name.as_str(),),
            )
            .await?;
        tracing::info!("tray icon registered");
        Ok(())
    }
}

/// Re-registers when the tray host (plasmashell) restarts, and keeps the tooltip current.
async fn watch(tray: Arc<Tray>) {
    let owners = async {
        let proxy = zbus::fdo::DBusProxy::new(&tray.st.conn).await.ok()?;
        proxy
            .receive_name_owner_changed_with_args(&[(0, WATCHER)])
            .await
            .ok()
    }
    .await;
    let mut owners = match owners {
        Some(s) => s.boxed(),
        None => futures_util::stream::pending().boxed(),
    };
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n: u64 = 0;
    loop {
        tokio::select! {
            Some(sig) = owners.next() => {
                let back = sig.args().map(|a| a.new_owner().is_some()).unwrap_or(false);
                if back
                        && tray.enabled.load(Ordering::SeqCst)
                        && let Err(e) = tray.register().await
                {
                    tracing::warn!("tray: re-registering failed: {e:#}");
                }
            }
            _ = tick.tick() => {
                n += 1;
                if tray.enabled.load(Ordering::SeqCst) {
                    // Pause changes show within 30 s; the tooltip refreshes every minute.
                    refresh(&tray.st, n.is_multiple_of(2)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_dur(40), "40s");
        assert_eq!(fmt_dur(12 * 60 + 5), "12m");
        assert_eq!(fmt_dur(2 * 3600 + 5 * 60), "2h 05m");
    }

    #[test]
    fn icon_sizes_and_corners() {
        let (w, h, px) = render(22, false);
        assert_eq!((w, h, px.len()), (22, 22, 22 * 22 * 4));
        // The rounded corner is transparent, the middle is opaque.
        assert_eq!(px[0], 0);
        let mid = ((11 * 22 + 11) * 4) as usize;
        assert_eq!(px[mid], 255);
    }

    #[test]
    fn menu_shows_pause_or_resume() {
        let vis = |paused, id| {
            items(paused, "x")
                .into_iter()
                .find(|(i, _)| *i == id)
                .map(|(_, p)| bool::try_from(&*p["visible"]).unwrap())
                .unwrap()
        };
        assert!(vis(false, M_PAUSE));
        assert!(!vis(false, M_RESUME));
        assert!(vis(true, M_RESUME));
        assert!(!vis(true, M_PAUSE30));
    }
}
