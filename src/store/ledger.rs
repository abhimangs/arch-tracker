//! Open/closed bookkeeping for spans. The tracker says "these subjects are now active for this
//! kind"; the ledger opens and closes rows, and writes them in batches. Open rows get their end
//! time refreshed on every flush, so a crash loses at most one flush interval.

use crate::model::Kind;
use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};

/// Spans shorter than this that were never written are dropped (alt-tab flicker).
const MIN_SPAN_MS: i64 = 500;

#[derive(Debug)]
struct Open {
    row: Option<i64>,
    start: i64,
}

#[derive(Debug)]
enum Pending {
    Insert {
        kind: Kind,
        subject: i64,
        start: i64,
        end: i64,
    },
    Close {
        row: i64,
        end: i64,
    },
}

#[derive(Default)]
pub struct Ledger {
    open: HashMap<(Kind, i64), Open>,
    pending: Vec<Pending>,
}

impl Ledger {
    /// Makes exactly the subjects in desired active for kind.
    pub fn reconcile(&mut self, kind: Kind, desired: &HashSet<i64>, now: i64) {
        let stale: Vec<(Kind, i64)> = self
            .open
            .keys()
            .filter(|(k, s)| *k == kind && !desired.contains(s))
            .copied()
            .collect();
        for key in stale {
            self.close(key, now);
        }
        for s in desired {
            self.open.entry((kind, *s)).or_insert(Open {
                row: None,
                start: now,
            });
        }
    }

    /// Convenience for kinds that are on/off with no subject (signals).
    pub fn set_flag(&mut self, kind: Kind, on: bool, now: i64) {
        let mut d = HashSet::new();
        if on {
            d.insert(0);
        }
        self.reconcile(kind, &d, now);
    }

    /// Like set_flag but with an explicit start time (e.g. idle starts at the last input).
    pub fn set_flag_at(&mut self, kind: Kind, on: bool, at: i64) {
        self.set_flag(kind, on, at);
    }

    pub fn is_open(&self, kind: Kind) -> bool {
        self.open.keys().any(|(k, _)| *k == kind)
    }

    fn close(&mut self, key: (Kind, i64), end: i64) {
        let Some(o) = self.open.remove(&key) else {
            return;
        };
        let end = end.max(o.start);
        match o.row {
            Some(row) => self.pending.push(Pending::Close { row, end }),
            None if end - o.start >= MIN_SPAN_MS => self.pending.push(Pending::Insert {
                kind: key.0,
                subject: key.1,
                start: o.start,
                end,
            }),
            None => {}
        }
    }

    pub fn close_all(&mut self, now: i64) {
        let keys: Vec<_> = self.open.keys().copied().collect();
        for k in keys {
            self.close(k, now);
        }
    }

    /// Closes every open span at at and reopens it at at (local-midnight split).
    pub fn split_all(&mut self, at: i64) {
        let keys: Vec<_> = self.open.keys().copied().collect();
        for k in keys {
            let start_after = self.open.get(&k).map(|o| o.start >= at).unwrap_or(true);
            if start_after {
                continue;
            }
            self.close(k, at);
            self.open.insert(
                k,
                Open {
                    row: None,
                    start: at,
                },
            );
        }
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// Writes closed spans and refreshes open ones in one transaction.
    pub fn flush(&mut self, conn: &mut Connection, now: i64) -> Result<()> {
        let tx = conn.transaction()?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut upd = tx.prepare_cached("UPDATE spans SET end_ms=?2 WHERE id=?1")?;
            for p in self.pending.drain(..) {
                match p {
                    Pending::Insert {
                        kind,
                        subject,
                        start,
                        end,
                    } => {
                        ins.execute(params![kind as i64, subject, start, end])?;
                    }
                    Pending::Close { row, end } => {
                        upd.execute(params![row, end])?;
                    }
                }
            }
            for ((kind, subject), o) in self.open.iter_mut() {
                match o.row {
                    Some(row) => {
                        upd.execute(params![row, now.max(o.start)])?;
                    }
                    None if now - o.start >= MIN_SPAN_MS => {
                        ins.execute(params![*kind as i64, *subject, o.start, now])?;
                        o.row = Some(tx.last_insert_rowid());
                    }
                    None => {}
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, spans_in};

    fn mem() -> Store {
        let dir =
            std::env::temp_dir().join(format!("at-test-{}-{}", std::process::id(), rand_suffix()));
        Store::open(&dir.join("t.db")).unwrap()
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn open_close_flush() {
        let mut st = mem();
        let mut l = Ledger::default();
        let set = |v: &[i64]| v.iter().copied().collect::<HashSet<_>>();
        l.reconcile(Kind::Focus, &set(&[1]), 1_000);
        l.reconcile(Kind::Focus, &set(&[2]), 5_000);
        l.reconcile(Kind::Focus, &set(&[3]), 5_100); // 100 ms flicker: dropped
        l.flush(&mut st.conn, 10_000).unwrap(); // 3 is open: written with end=10_000
        l.flush(&mut st.conn, 12_000).unwrap(); // refreshed to 12_000
        l.reconcile(Kind::Focus, &set(&[]), 13_000);
        l.flush(&mut st.conn, 14_000).unwrap();
        let spans = spans_in(&st.conn, 0, 100_000).unwrap();
        let got: Vec<(i64, i64, i64)> = spans.iter().map(|s| (s.subject, s.start, s.end)).collect();
        assert_eq!(got, vec![(1, 1_000, 5_000), (3, 5_100, 13_000)]);
    }

    #[test]
    fn split_at_midnight() {
        let mut st = mem();
        let mut l = Ledger::default();
        l.set_flag(Kind::Paused, true, 1_000);
        l.split_all(50_000);
        l.set_flag(Kind::Paused, false, 60_000);
        l.flush(&mut st.conn, 70_000).unwrap();
        let spans = spans_in(&st.conn, 0, 100_000).unwrap();
        let got: Vec<(i64, i64)> = spans.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(got, vec![(1_000, 50_000), (50_000, 60_000)]);
    }
}
