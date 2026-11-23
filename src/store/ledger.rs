//! Open/closed bookkeeping for spans. The tracker says "these subjects are now active for this
//! kind"; the ledger opens and closes rows, and writes them in batches. Open rows get their end
//! time refreshed on every flush, so a crash loses at most one flush interval. Spans never cross
//! local midnight: the ledger cuts them there when they close and when it flushes, whatever the
//! caller does (back-dated starts, a suspend that ends the next day, a missed tick).

use crate::model::Kind;
use anyhow::Result;
use rusqlite::{Connection, Transaction, params};
use std::collections::{HashMap, HashSet};

/// Spans shorter than this that were never written are dropped (alt-tab flicker).
const MIN_SPAN_MS: i64 = 500;

/// Upper bound on the days one span is cut into (guards against a wildly wrong clock).
const MAX_DAYS: usize = 400;

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

pub struct Ledger {
    open: HashMap<(Kind, i64), Open>,
    pending: Vec<Pending>,
    /// [start, end) of the local day containing an instant (swappable in tests).
    day_bounds: fn(i64) -> (i64, i64),
    /// Last answer of day_bounds, reused while instants fall inside it.
    day: (i64, i64),
}

impl Default for Ledger {
    fn default() -> Self {
        Ledger::with_day_bounds(crate::util::local_day_bounds)
    }
}

impl Ledger {
    pub fn with_day_bounds(day_bounds: fn(i64) -> (i64, i64)) -> Self {
        Ledger {
            open: HashMap::new(),
            pending: vec![],
            day_bounds,
            day: (0, 0),
        }
    }

    /// Start of the day after the one containing t.
    fn next_day(&mut self, t: i64) -> i64 {
        if !(self.day.0 <= t && t < self.day.1) {
            self.day = (self.day_bounds)(t);
        }
        self.day.1
    }

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

    /// Closes an open span at end, cut into one piece per local day.
    fn close(&mut self, key: (Kind, i64), end: i64) {
        let Some(o) = self.open.remove(&key) else {
            return;
        };
        let end = end.max(o.start);
        let (mut start, mut row) = (o.start, o.row);
        for _ in 0..MAX_DAYS {
            let m = self.next_day(start);
            let piece_end = if m > start && m < end { m } else { end };
            self.piece(key, row.take(), start, piece_end);
            if piece_end >= end {
                return;
            }
            start = piece_end;
        }
        self.piece(key, None, start, end);
    }

    fn piece(&mut self, key: (Kind, i64), row: Option<i64>, start: i64, end: i64) {
        match row {
            Some(row) => self.pending.push(Pending::Close { row, end }),
            None if end - start >= MIN_SPAN_MS => self.pending.push(Pending::Insert {
                kind: key.0,
                subject: key.1,
                start,
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

    /// Open spans that started before the local day containing now are closed at the last
    /// midnight (one piece per day) and reopened there.
    pub fn split_due(&mut self, now: i64) {
        let starts: Vec<((Kind, i64), i64)> =
            self.open.iter().map(|(k, o)| (*k, o.start)).collect();
        for (k, start) in starts {
            let mut m = self.next_day(start);
            if m > now || m <= start {
                continue;
            }
            for _ in 0..MAX_DAYS {
                let n = self.next_day(m);
                if n > now || n <= m {
                    break;
                }
                m = n;
            }
            self.close(k, m);
            self.open.insert(
                k,
                Open {
                    row: None,
                    start: m,
                },
            );
        }
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// Writes closed spans and refreshes open ones in one transaction.
    pub fn flush(&mut self, conn: &mut Connection, now: i64) -> Result<()> {
        self.split_due(now);
        let tx = conn.transaction()?;
        let rows = self.write(&tx, now)?;
        tx.commit()?;
        self.committed(rows);
        Ok(())
    }

    /// Writes pending spans and open rows inside tx. Nothing in the ledger changes here: the
    /// row ids of newly inserted open spans are returned and only applied (with the pending list
    /// cleared) by committed(), so a failed commit loses nothing and leaves no stale row ids.
    pub fn write(&self, tx: &Transaction, now: i64) -> Result<Vec<((Kind, i64), i64)>> {
        let mut ins = tx.prepare_cached(
            "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut upd = tx.prepare_cached("UPDATE spans SET end_ms=?2 WHERE id=?1")?;
        for p in &self.pending {
            match *p {
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
        let mut rows = vec![];
        for (&(kind, subject), o) in &self.open {
            match o.row {
                Some(row) => {
                    upd.execute(params![row, now.max(o.start)])?;
                }
                None if now - o.start >= MIN_SPAN_MS => {
                    ins.execute(params![kind as i64, subject, o.start, now])?;
                    rows.push(((kind, subject), tx.last_insert_rowid()));
                }
                None => {}
            }
        }
        Ok(rows)
    }

    /// Call after the transaction given to write() committed.
    pub fn committed(&mut self, rows: Vec<((Kind, i64), i64)>) {
        self.pending.clear();
        for (key, row) in rows {
            if let Some(o) = self.open.get_mut(&key) {
                o.row = Some(row);
            }
        }
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
    fn failed_commit_keeps_pending_and_row_ids() {
        let mut st = mem();
        let set = |v: &[i64]| v.iter().copied().collect::<HashSet<_>>();
        let mut l = Ledger::default();
        l.reconcile(Kind::Focus, &set(&[1]), 1_000);
        l.flush(&mut st.conn, 5_000).unwrap(); // row for 1 written
        l.reconcile(Kind::Focus, &set(&[7]), 6_000); // close 1 (pending), open 7
        l.set_flag(Kind::Mic, true, 6_000);
        // Make the next COMMIT fail after every statement ran: a deferred foreign key that a
        // trigger violates when subject 7 is inserted.
        st.conn
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE p(id INTEGER PRIMARY KEY);
                 CREATE TABLE c(pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE TRIGGER boom AFTER INSERT ON spans WHEN NEW.subject_id = 7
                 BEGIN INSERT INTO c VALUES (42); END;",
            )
            .unwrap();
        assert!(l.flush(&mut st.conn, 9_000).is_err());
        st.conn.execute_batch("DROP TRIGGER boom;").unwrap();
        // An unrelated row takes the id the failed transaction handed out.
        st.conn
            .execute(
                "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (2, 99, 100, 200)",
                [],
            )
            .unwrap();
        l.flush(&mut st.conn, 12_000).unwrap();
        let spans = spans_in(&st.conn, 0, 100_000).unwrap();
        let mut got: Vec<(i64, i64, i64)> =
            spans.iter().map(|s| (s.subject, s.start, s.end)).collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                (0, 6_000, 12_000),
                (1, 1_000, 6_000),
                (7, 6_000, 12_000),
                (99, 100, 200)
            ]
        );
    }

    /// Test days are 100 s long and start at multiples of 100_000 ms (no time zone involved).
    const D: i64 = 100_000;

    fn test_days(t: i64) -> (i64, i64) {
        let s = t.div_euclid(D) * D;
        (s, s + D)
    }

    fn all(st: &Store) -> Vec<(i64, i64, i64)> {
        let spans = spans_in(&st.conn, i64::MIN, i64::MAX).unwrap();
        spans.iter().map(|s| (s.subject, s.start, s.end)).collect()
    }

    #[test]
    fn close_splits_at_midnight() {
        let mut st = mem();
        let mut l = Ledger::with_day_bounds(test_days);
        l.set_flag(Kind::Suspended, true, 90_000);
        l.set_flag(Kind::Suspended, false, 130_000);
        // A piece shorter than the flicker limit after midnight is dropped.
        l.set_flag(Kind::Paused, true, 195_000);
        l.set_flag(Kind::Paused, false, 200_200);
        l.flush(&mut st.conn, 210_000).unwrap();
        assert_eq!(
            all(&st),
            vec![
                (0, 90_000, 100_000),
                (0, 100_000, 130_000),
                (0, 195_000, 200_000)
            ]
        );
    }

    #[test]
    fn flush_splits_open_rows_at_midnight() {
        let mut st = mem();
        let mut l = Ledger::with_day_bounds(test_days);
        l.set_flag(Kind::Paused, true, 1_000);
        l.flush(&mut st.conn, 95_000).unwrap(); // row written, end 95_000
        l.flush(&mut st.conn, 105_000).unwrap(); // cut at 100_000, reopened there
        l.set_flag(Kind::Paused, false, 120_000);
        l.flush(&mut st.conn, 130_000).unwrap();
        assert_eq!(all(&st), vec![(0, 1_000, 100_000), (0, 100_000, 120_000)]);
    }

    #[test]
    fn back_dated_start_is_split_per_day() {
        let mut st = mem();
        let mut l = Ledger::with_day_bounds(test_days);
        // An idle gap that began two "days" before it was reported.
        l.set_flag(Kind::InputIdle, true, 50_000);
        l.flush(&mut st.conn, 250_000).unwrap();
        assert_eq!(
            all(&st),
            vec![
                (0, 50_000, 100_000),
                (0, 100_000, 200_000),
                (0, 200_000, 250_000)
            ]
        );
        l.set_flag(Kind::InputIdle, false, 260_000);
        l.flush(&mut st.conn, 270_000).unwrap();
        assert_eq!(all(&st)[2], (0, 200_000, 260_000));
    }

    #[test]
    fn real_local_days_are_never_crossed() {
        let mut st = mem();
        let mut l = Ledger::default();
        // 2026-03-07 12:00 UTC, closed three and a half days later: whatever the time zone,
        // every stored span must end by the local midnight after its start.
        let start = 1_772_884_800_000;
        l.set_flag(Kind::Suspended, true, start);
        l.flush(&mut st.conn, start + 30 * 3_600_000).unwrap();
        l.set_flag(Kind::Suspended, false, start + 84 * 3_600_000);
        l.flush(&mut st.conn, start + 85 * 3_600_000).unwrap();
        let spans = all(&st);
        assert!(spans.len() >= 4);
        let mut covered = 0;
        for (_, s, e) in &spans {
            assert!(*e <= crate::util::next_local_midnight(*s), "{s}..{e}");
            covered += e - s;
        }
        assert_eq!(covered, 84 * 3_600_000);
    }
}
