//! MPRIS media players (what is playing, from which process) and microphone capture state.

use crate::daemon::Event;
use futures_util::{FutureExt, StreamExt};
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use zbus::{Connection, MatchRule, MessageStream, OwnedVariant};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Player {
    pub bus: String,
    pub identity: String,
    pub pid: u32,
    pub playing: bool,
    pub url: String,
    pub title: String,
}

impl Player {
    pub fn is_browser_bridge(&self) -> bool {
        self.bus.contains("plasma-browser-integration")
    }
}

/// Watches MPRIS players: refreshes on any player property change, on players appearing or
/// disappearing, and every 15 s as a safety net.
pub async fn spawn(conn: Connection, tx: UnboundedSender<Event>) {
    tokio::spawn(async move {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")
            .member("PropertiesChanged")
            .path("/org/mpris/MediaPlayer2")
            .build();
        let owner_rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")
            .member("NameOwnerChanged")
            .arg0ns("org.mpris.MediaPlayer2")
            .build();
        let mut props = match rule {
            Ok(r) => MessageStream::for_match_rule(r, &conn, None).await.ok(),
            Err(_) => None,
        };
        let mut owners = match owner_rule {
            Ok(r) => MessageStream::for_match_rule(r, &conn, None).await.ok(),
            Err(_) => None,
        };
        let mut last: Option<Vec<Player>> = None;
        let mut every = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = every.tick() => {}
                Some(_) = next(&mut props) => {}
                Some(_) = next(&mut owners) => {}
            }
            // Coalesce bursts of property changes: wait, then swallow what queued meanwhile, so a
            // burst costs one round of ListNames/GetAll.
            tokio::time::sleep(Duration::from_millis(250)).await;
            drain(&mut props);
            drain(&mut owners);
            let players = read_players(&conn).await;
            if last.as_ref() != Some(&players) {
                last = Some(players.clone());
                let _ = tx.send(Event::Media(players));
            }
        }
    });
}

/// Discards messages already queued on the stream without waiting.
fn drain(s: &mut Option<MessageStream>) {
    let Some(stream) = s else { return };
    loop {
        match stream.next().now_or_never() {
            Some(Some(_)) => continue,
            Some(None) => {
                *s = None; // ended
                return;
            }
            None => return,
        }
    }
}

async fn next(s: &mut Option<MessageStream>) -> Option<()> {
    match s {
        Some(s) => s.next().await.map(|_| ()),
        None => std::future::pending().await,
    }
}

async fn get_all(conn: &Connection, bus: &str, iface: &str) -> HashMap<String, OwnedVariant> {
    let Ok(reply) = conn
        .call_method(
            Some(bus),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &(iface,),
        )
        .await
    else {
        return HashMap::new();
    };
    reply.body().deserialize().unwrap_or_default()
}

fn as_string(v: Option<&OwnedVariant>) -> String {
    v.and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default()
}

async fn read_players(conn: &Connection) -> Vec<Player> {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return vec![];
    };
    let Ok(names) = dbus.list_names().await else {
        return vec![];
    };
    let mut out = vec![];
    for name in names {
        let bus = name.to_string();
        if !bus.starts_with("org.mpris.MediaPlayer2.") {
            continue;
        }
        let player = get_all(conn, &bus, "org.mpris.MediaPlayer2.Player").await;
        let root = get_all(conn, &bus, "org.mpris.MediaPlayer2").await;
        let status = as_string(player.get("PlaybackStatus"));
        let meta: HashMap<String, OwnedVariant> = player
            .get("Metadata")
            .and_then(|v| HashMap::<String, OwnedVariant>::try_from(v.clone()).ok())
            .unwrap_or_default();
        let kde_pid = meta.get("kde:pid").and_then(|v| {
            i64::try_from(v.clone())
                .ok()
                .or_else(|| u32::try_from(v.clone()).ok().map(i64::from))
                .or_else(|| i32::try_from(v.clone()).ok().map(i64::from))
        });
        let pid = match kde_pid {
            Some(p) => p as u32,
            None => dbus
                .get_connection_unix_process_id(name.as_ref())
                .await
                .unwrap_or(0),
        };
        out.push(Player {
            identity: as_string(root.get("Identity")),
            pid,
            playing: status == "Playing",
            url: as_string(meta.get("xesam:url")),
            title: as_string(meta.get("xesam:title")),
            bus,
        });
    }
    dedupe(out)
}

/// Browsers expose the same media twice (their own MPRIS + Plasma Browser Integration);
/// keep the bridge, which knows the tab URL.
fn dedupe(players: Vec<Player>) -> Vec<Player> {
    let bridged: Vec<u32> = players
        .iter()
        .filter(|p| p.is_browser_bridge())
        .map(|p| p.pid)
        .collect();
    players
        .into_iter()
        .filter(|p| p.is_browser_bridge() || !bridged.contains(&p.pid))
        .collect()
}

/// True if any ALSA capture device is actively capturing (PipeWire closes idle sources).
pub fn mic_active() -> bool {
    let Ok(cards) = std::fs::read_dir("/proc/asound") else {
        return false;
    };
    for card in cards.flatten() {
        let name = card.file_name();
        if !name.to_string_lossy().starts_with("card") {
            continue;
        }
        let Ok(pcms) = std::fs::read_dir(card.path()) else {
            continue;
        };
        for pcm in pcms.flatten() {
            let pn = pcm.file_name();
            let pn = pn.to_string_lossy();
            if !(pn.starts_with("pcm") && pn.ends_with('c')) {
                continue;
            }
            let Ok(subs) = std::fs::read_dir(pcm.path()) else {
                continue;
            };
            for sub in subs.flatten() {
                if let Ok(s) = std::fs::read_to_string(sub.path().join("status"))
                    && s.contains("RUNNING")
                {
                    return true;
                }
            }
        }
    }
    false
}

/// "https://www.youtube.com/watch?v=x" -> "www.youtube.com"
pub fn url_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_lowercase();
            Some(match u.port() {
                Some(p) => format!("{host}:{p}"),
                None => host,
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(bus: &str, pid: u32) -> Player {
        Player {
            bus: bus.into(),
            identity: String::new(),
            pid,
            playing: true,
            url: String::new(),
            title: String::new(),
        }
    }

    #[test]
    fn dedupes_browser_players() {
        let out = dedupe(vec![
            p("org.mpris.MediaPlayer2.brave.instance70428", 70428),
            p("org.mpris.MediaPlayer2.plasma-browser-integration", 70428),
            p("org.mpris.MediaPlayer2.strawberry", 999),
        ]);
        let buses: Vec<&str> = out.iter().map(|p| p.bus.as_str()).collect();
        assert_eq!(
            buses,
            vec![
                "org.mpris.MediaPlayer2.plasma-browser-integration",
                "org.mpris.MediaPlayer2.strawberry"
            ]
        );
    }

    #[test]
    fn hosts() {
        assert_eq!(
            url_host("https://www.YouTube.com/watch?v=x"),
            "www.youtube.com"
        );
        assert_eq!(url_host("http://localhost:3000/a"), "localhost:3000");
        assert_eq!(url_host("not a url"), "");
    }
}
