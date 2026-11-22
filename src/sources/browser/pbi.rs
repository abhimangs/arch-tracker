//! Plasma Browser Integration fallback: its KRunner "TabsRunner" lists open tabs (title, URL,
//! audible). Results are cached per runner session, so Teardown() is called before each query.

use crate::sources::media::url_host;
use std::collections::HashMap;
use zbus::{Connection, OwnedVariant};

const NAME: &str = "org.kde.plasma.browser_integration";

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PbiTab {
    pub id: i64,
    pub title: String,
    pub host: String,
    pub audible: bool,
}

type Row = (
    String,
    String,
    String,
    i32,
    f64,
    HashMap<String, OwnedVariant>,
);

async fn call_match(conn: &Connection, q: &str) -> Vec<Row> {
    let Ok(reply) = conn
        .call_method(
            Some(NAME),
            "/TabsRunner",
            Some("org.kde.krunner1"),
            "Match",
            &(q,),
        )
        .await
    else {
        return vec![];
    };
    reply.body().deserialize::<Vec<Row>>().unwrap_or_default()
}

/// Returns (browser pid, tabs) if the integration host is running.
pub async fn query(conn: &Connection) -> Option<(u32, Vec<PbiTab>)> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
    let name = zbus::names::BusName::try_from(NAME).ok()?;
    let host_pid = dbus.get_connection_unix_process_id(name).await.ok()?;
    let browser_pid = crate::sources::terminal::procfs::stat(host_pid)
        .map(|s| s.ppid)
        .unwrap_or(0);
    let _ = conn
        .call_method(
            Some(NAME),
            "/TabsRunner",
            Some("org.kde.krunner1"),
            "Teardown",
            &(),
        )
        .await;
    let mut tabs: Vec<PbiTab> = vec![];
    for q in [".", "e", "a"] {
        for (id, title, icon, _, _, props) in call_match(conn, q).await {
            let Ok(id) = id.parse::<i64>() else { continue };
            if tabs.iter().any(|t| t.id == id) {
                continue;
            }
            let url = props
                .get("urls")
                .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
                .and_then(|u| u.into_iter().next())
                .or_else(|| {
                    props
                        .get("subtext")
                        .and_then(|v| String::try_from(v.clone()).ok())
                })
                .unwrap_or_default();
            let muteable = props
                .get("actions")
                .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
                .map(|a| a.iter().any(|x| x == "MUTE"))
                .unwrap_or(false);
            tabs.push(PbiTab {
                id,
                title,
                host: url_host(&url),
                audible: icon == "audio-volume-high" || muteable,
            });
        }
    }
    Some((browser_pid, tabs))
}
