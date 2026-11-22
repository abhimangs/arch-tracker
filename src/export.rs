//! Raw span export (CSV or JSON) for a local date range. Used by the CLI and the dashboard.

use crate::model::Kind;
use crate::store;
use crate::util;
use anyhow::{Result, bail};
use rusqlite::Connection;

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

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Returns (content type, body).
pub fn export(
    conn: &Connection,
    from: &str,
    to: &str,
    format: &str,
) -> Result<(&'static str, String)> {
    let (Some((a, _)), Some((_, b))) = (util::date_bounds(from), util::date_bounds(to)) else {
        bail!("dates must look like 2026-11-22");
    };
    if b <= a {
        bail!("'to' must not be before 'from'");
    }
    let spans = store::spans_in(conn, a, b)?;
    let ids: Vec<i64> = spans.iter().map(|s| s.subject).collect();
    let subjects = store::subjects_by_id(conn, &ids)?;
    let empty = crate::model::Subject::default();
    match format {
        "json" => {
            let rows: Vec<serde_json::Value> = spans
                .iter()
                .map(|s| {
                    let subj = subjects.get(&s.subject).unwrap_or(&empty);
                    serde_json::json!({"kind": kind_name(s.kind), "start_ms": s.start, "end_ms": s.end, "subject": subj})
                })
                .collect();
            Ok(("application/json", serde_json::to_string_pretty(&rows)?))
        }
        "csv" => {
            let mut out = String::from(
                "kind,start_ms,end_ms,seconds,app,app_name,title,host,private,program,project\n",
            );
            for s in &spans {
                let subj = subjects.get(&s.subject).unwrap_or(&empty);
                let row = [
                    kind_name(s.kind).to_string(),
                    s.start.to_string(),
                    s.end.to_string(),
                    ((s.end - s.start) / 1000).to_string(),
                    csv_field(&subj.app),
                    csv_field(&subj.app_name),
                    csv_field(&subj.title),
                    csv_field(&subj.host),
                    subj.private.to_string(),
                    csv_field(&subj.program),
                    csv_field(&subj.project),
                ];
                out.push_str(&row.join(","));
                out.push('\n');
            }
            Ok(("text/csv; charset=utf-8", out))
        }
        other => bail!("unknown format {other:?} (use csv or json)"),
    }
}
