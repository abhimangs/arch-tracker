//! Wayland ext-idle-notify-v1. Version 2's input-idle notification ignores inhibitors, so we get
//! raw "no input" gaps; the classic notification respects inhibitors (video, calls), so the
//! difference tells us when something is keeping the session awake.

use crate::daemon::Event;
use crate::util::now_ms;
use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

/// Raw input gaps shorter than this are never recorded; any idle threshold >= this works.
pub const GAP_MS: u32 = 30_000;

#[derive(Clone, Copy, Debug)]
enum Which {
    /// Ignores inhibitors (v2): pure keyboard/mouse inactivity.
    Input,
    /// Respects inhibitors.
    Inhibitable,
}

struct State {
    tx: UnboundedSender<Event>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, Which> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        which: &Which,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idle = match event {
            ext_idle_notification_v1::Event::Idled => true,
            ext_idle_notification_v1::Event::Resumed => false,
            _ => return,
        };
        let input = matches!(which, Which::Input);
        let _ = state.tx.send(Event::Idle {
            input,
            idle,
            at: now_ms(),
        });
    }
}

/// Runs the Wayland connection on its own thread (blocking dispatch, no polling).
pub fn spawn(tx: UnboundedSender<Event>) {
    let _ = std::thread::Builder::new()
        .name("wayland-idle".into())
        .spawn(move || {
            if let Err(e) = run(tx.clone()) {
                tracing::warn!("idle detection unavailable: {e:#}");
                let _ = tx.send(Event::IdleUnavailable(format!("{e:#}")));
            }
        });
}

fn run(tx: UnboundedSender<Event>) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland (WAYLAND_DISPLAY)")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).context("no wl_seat")?;
    let notifier: ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=2, ())
        .context("compositor lacks ext_idle_notifier_v1")?;
    let v = notifier.version();
    if v >= 2 {
        notifier.get_input_idle_notification(GAP_MS, &seat, &qh, Which::Input);
        notifier.get_idle_notification(GAP_MS, &seat, &qh, Which::Inhibitable);
    } else {
        // v1 only: the single notification respects inhibitors; treat it as input idle.
        notifier.get_idle_notification(GAP_MS, &seat, &qh, Which::Input);
    }
    let _ = tx.send(Event::IdleReady { version: v });
    tracing::info!("idle notifier bound (v{v})");
    let mut state = State { tx };
    loop {
        queue.blocking_dispatch(&mut state)?;
    }
}
