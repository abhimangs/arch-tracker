//! Raw span export (CSV or JSON) for a local date range. Used by the CLI and the dashboard.
//! Rows are read and written one at a time, so memory stays flat however long the range is.

use crate::model::{Kind, Subject};
use crate::report::{BadRequest, parse_date};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::io::Write;

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Focus => "focus",
        Kind::Visible => "visible",
        Kind::BgAudio => "background_audio",
        Kind::AgentBusy => "agent_busy",
        Kind::Running => "running",
        Kind::InputIdle => "input_idle",
        Kind::Inhibited => "inhibited",
        Kind::MediaVisible => "media_visible",
        Kind::Mic => "mic",
        Kind::Locked => "locked",
        Kind::Suspended => "suspended",
        Kind::Paused => "paused",
    }
}

/// A CSV cell. Text that a spreadsheet would run as a formula (starting with = + - @, a tab or a
/// carriage return) gets a leading apostrophe: page titles are untrusted.
fn csv_field(s: &str) -> String {
    let guarded;
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        guarded = format!("'{s}");
        guarded.as_str()
    } else {
        s
    };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Result<Format> {
        match s {
            "csv" => Ok(Format::Csv),
            "json" => Ok(Format::Json),
            other => Err(BadRequest(format!("unknown format {other:?} (use csv or json)")).into()),
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::Csv => "text/csv; charset=utf-8",
            Format::Json => "application/json",
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Format::Csv => "arch-tracker.csv",
            Format::Json => "arch-tracker.json",
        }
    }
}

/// [start, end) in ms of the local dates from..=to.
pub fn bounds(from: &str, to: &str) -> Result<(i64, i64)> {
    let (_, a, _) = parse_date(from)?;
    let (_, _, b) = parse_date(to)?;
    if b <= a {
        return Err(BadRequest("'to' must not be before 'from'".into()).into());
    }
    Ok((a, b))
}

const CSV_HEADER: &str =
    "kind,start_ms,end_ms,seconds,app,app_name,title,host,private,program,project\n";

#[derive(Serialize)]
struct JsonRow<'a> {
    kind: &'static str,
    start_ms: i64,
    end_ms: i64,
    subject: &'a Subject,
}

/// Writes every span that starts in [a, b) to out, oldest first.
pub fn write(
    conn: &Connection,
    (a, b): (i64, i64),
    format: Format,
    out: &mut dyn Write,
) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT s.kind, s.start_ms, s.end_ms,
                u.app, u.app_name, u.title, u.host, u.private, u.program, u.project
         FROM spans s LEFT JOIN subjects u ON u.id = s.subject_id
         WHERE s.start_ms >= ?1 AND s.start_ms < ?2 ORDER BY s.start_ms",
    )?;
    let mut rows = stmt.query(params![a, b])?;
    match format {
        Format::Csv => out.write_all(CSV_HEADER.as_bytes())?,
        Format::Json => out.write_all(b"[")?,
    }
    let mut n = 0u64;
    while let Some(r) = rows.next()? {
        let Some(kind) = Kind::from_i64(r.get(0)?) else {
            continue;
        };
        let start: i64 = r.get(1)?;
        let end = r.get::<_, i64>(2)?.min(b);
        let text = |i: usize| -> rusqlite::Result<String> {
            Ok(r.get::<_, Option<String>>(i)?.unwrap_or_default())
        };
        let subj = Subject {
            app: text(3)?,
            app_name: text(4)?,
            title: text(5)?,
            host: text(6)?,
            private: r.get::<_, Option<bool>>(7)?.unwrap_or(false),
            program: text(8)?,
            project: text(9)?,
        };
        match format {
            Format::Csv => writeln!(
                out,
                "{},{},{},{},{},{},{},{},{},{},{}",
                kind_name(kind),
                start,
                end,
                (end - start) / 1000,
                csv_field(&subj.app),
                csv_field(&subj.app_name),
                csv_field(&subj.title),
                csv_field(&subj.host),
                subj.private,
                csv_field(&subj.program),
                csv_field(&subj.project),
            )?,
            Format::Json => {
                out.write_all(if n == 0 { b"\n  " } else { b",\n  " })?;
                let row = JsonRow {
                    kind: kind_name(kind),
                    start_ms: start,
                    end_ms: end,
                    subject: &subj,
                };
                serde_json::to_writer(&mut *out, &row).map_err(std::io::Error::from)?;
            }
        }
        n += 1;
    }
    if format == Format::Json {
        out.write_all(if n == 0 { b"]\n" } else { b"\n]\n" })?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_cells_never_start_a_formula() {
        assert_eq!(csv_field("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
        assert_eq!(csv_field("+1 new message"), "'+1 new message");
        assert_eq!(csv_field("-2"), "'-2");
        assert_eq!(csv_field("@everyone"), "'@everyone");
        assert_eq!(csv_field("\tcmd"), "'\tcmd");
        assert_eq!(csv_field("\rcmd"), "\"'\rcmd\"");
        assert_eq!(csv_field("a, b"), "\"a, b\"");
        assert_eq!(csv_field("plain title"), "plain title");
        assert_eq!(csv_field(""), "");
    }

    fn sample() -> crate::store::Store {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("at-export-{}-{nanos}", std::process::id()));
        let st = crate::store::Store::open(&dir.join("t.db")).unwrap();
        st.conn
            .execute(
                "INSERT INTO subjects(app, app_name, title, host, private, program, project)
                 VALUES ('browser', 'Browser', '=1+1', 'example.org', 0, '', '')",
                [],
            )
            .unwrap();
        for (kind, subject, a, b) in [
            (1, 1, 1_000, 61_000),
            (10, 0, 5_000, 9_000),
            (1, 1, 90_000, 200_000),
        ] {
            st.conn
                .execute(
                    "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (?1, ?2, ?3, ?4)",
                    params![kind, subject, a, b],
                )
                .unwrap();
        }
        st
    }

    #[test]
    fn csv_and_json_stream_every_row() {
        let st = sample();
        let mut csv = Vec::new();
        write(&st.conn, (0, 100_000), Format::Csv, &mut csv).unwrap();
        let csv = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], CSV_HEADER.trim_end());
        assert_eq!(
            lines[1],
            "focus,1000,61000,60,browser,Browser,'=1+1,example.org,false,,"
        );
        assert_eq!(lines[2], "input_idle,5000,9000,4,,,,,false,,");
        // Ends are clipped to the range.
        assert!(lines[3].starts_with("focus,90000,100000,10,"));
        assert_eq!(lines.len(), 4);

        let mut json = Vec::new();
        write(&st.conn, (0, 100_000), Format::Json, &mut json).unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_slice(&json).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0]["kind"], "focus");
        assert_eq!(v[0]["subject"]["title"], "=1+1");
        assert_eq!(v[1]["subject"]["app"], "");

        let mut empty = Vec::new();
        write(&st.conn, (500_000, 600_000), Format::Json, &mut empty).unwrap();
        assert_eq!(
            serde_json::from_slice::<Vec<serde_json::Value>>(&empty)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn arguments_are_checked_first() {
        assert!(Format::parse("xml").is_err());
        assert!(bounds("2026-11-02", "2026-11-01").is_err());
        assert!(bounds("2026-11-01", "nope").is_err());
        let (a, b) = bounds("2026-11-01", "2026-11-01").unwrap();
        assert!(b > a);
    }
}
