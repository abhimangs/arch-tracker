//! The tracker actor: owns the live picture of the desktop and turns it into spans.

use super::{Event, SharedRef};
use crate::config::Config;
use crate::ipc::Request;
use crate::model::Kind;
use crate::store::Store;
use crate::store::ledger::Ledger;
use crate::util::now_ms;
use serde_json::json;
use std::time::Instant;

pub struct Tracker {
    store: Store,
    ledger: Ledger,
    cfg: Config,
    shared: SharedRef,
    started_ms: i64,
    /// Some(None) = paused until resumed; Some(Some(t)) = paused until t.
    paused: Option<Option<i64>>,
    last_flush: Instant,
    last_wall: i64,
    last_mono: Instant,
    next_midnight: i64,
}

impl Tracker {
    pub fn new(store: Store, cfg: Config, shared: SharedRef) -> Self {
        let now = now_ms();
        Tracker {
            store,
            ledger: Ledger::default(),
            cfg,
            shared,
            started_ms: now,
            paused: None,
            last_flush: Instant::now(),
            last_wall: now,
            last_mono: Instant::now(),
            next_midnight: next_local_midnight(now),
        }
    }

    pub fn start(&mut self) {
        self.reconcile(now_ms());
    }

    pub async fn handle(&mut self, ev: Event) {
        let now = now_ms();
        match ev {
            Event::Control(req, reply) => {
                let resp = self.control(req, now);
                let _ = reply.send(resp);
            }
            Event::ConfigChanged(cfg) => {
                tracing::info!("config reloaded");
                self.cfg = *cfg;
                if let Ok(mut s) = self.shared.lock() {
                    s.config = self.cfg.clone();
                }
            }
        }
        self.reconcile(now);
    }

    /// Periodic work: pause expiry, clock jumps, midnight split, sampling, flushing.
    pub async fn tick(&mut self) {
        let now = now_ms();
        let mono_elapsed = self.last_mono.elapsed().as_millis() as i64;
        let expected = self.last_wall + mono_elapsed;
        if (now - expected).abs() > 5_000 {
            // Wall clock jumped (suspend we missed, NTP, manual change): don't let open spans
            // stretch across the jump.
            tracing::info!("clock jump of {} s detected", (now - expected) / 1000);
            self.ledger.close_all(expected.min(now));
            self.next_midnight = next_local_midnight(now);
        }
        self.last_wall = now;
        self.last_mono = Instant::now();

        if let Some(Some(until)) = self.paused
            && now >= until
        {
            tracing::info!("pause expired");
            self.paused = None;
        }
        if now >= self.next_midnight {
            self.ledger.split_all(self.next_midnight);
            self.next_midnight = next_local_midnight(now);
        }
        self.reconcile(now);
        if self.last_flush.elapsed().as_secs() >= self.cfg.general.flush_interval_secs {
            self.flush(now);
        }
    }

    pub fn shutdown(&mut self) {
        let now = now_ms();
        self.ledger.close_all(now);
        self.flush(now);
    }

    fn flush(&mut self, now: i64) {
        if let Err(e) = self.ledger.flush(&mut self.store.conn, now) {
            tracing::error!("database write failed: {e:#}");
        }
        self.last_flush = Instant::now();
    }

    fn control(&mut self, req: Request, now: i64) -> serde_json::Value {
        match req {
            Request::Status => self.status_json(now),
            Request::Pause { until_ms } => {
                self.paused = Some(until_ms);
                tracing::info!("paused until {:?}", until_ms);
                json!({"ok": true, "paused_until": until_ms})
            }
            Request::Resume => {
                self.paused = None;
                tracing::info!("resumed");
                json!({"ok": true})
            }
            Request::Reload => match Config::load() {
                Ok(c) => {
                    self.cfg = c;
                    json!({"ok": true})
                }
                Err(e) => json!({"error": format!("{e:#}")}),
            },
        }
    }

    fn paused_now(&self) -> bool {
        self.paused.is_some()
    }

    /// Recomputes which spans should be open right now.
    fn reconcile(&mut self, now: i64) {
        self.ledger.set_flag(Kind::Paused, self.paused_now(), now);
        let status = self.status_json(now);
        if let Ok(mut s) = self.shared.lock() {
            s.status = status;
        }
    }

    fn status_json(&self, now: i64) -> serde_json::Value {
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "now_ms": now,
            "started_ms": self.started_ms,
            "paused": self.paused.is_some(),
            "paused_until_ms": self.paused.flatten(),
            "open_spans": self.ledger.open_count(),
        })
    }
}

pub use crate::util::next_local_midnight;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midnight_is_in_the_future_and_within_a_day() {
        let now = now_ms();
        let m = next_local_midnight(now);
        assert!(m > now);
        assert!(m - now <= 25 * 3_600_000);
    }
}
