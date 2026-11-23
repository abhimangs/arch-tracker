//! Apps that are running, whether or not you use them: windows (KWin), apps KDE launched as
//! systemd "app-*" units (this includes tray/background apps such as a Notion closed to tray),
//! and StatusNotifierItem tray icons. Recorded as "running", never as screen time.

use crate::desktop::{DesktopIndex, pretty};
use crate::sources::kwin::Snapshot;
use crate::sources::terminal::procfs;
use std::collections::BTreeMap;
use zbus::{Connection, OwnedObjectPath, OwnedVariant};

type UnitRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

/// Parses systemd app unit names (XDG spec: app[-<launcher>]-<id>[-<random>].scope or
/// app[-<launcher>]-<id>[@<random>].service). Dashes inside the id are escaped as \x2d, so plain
/// dashes separate the fields.
/// "app-com.github.wwmm.easyeffects@autostart.service" -> "com.github.wwmm.easyeffects"
/// "app-KDE-brave\x2dorigin\x2dbeta-a81332d8.scope" -> "brave-origin-beta"
/// "app-flatpak-com.notion.app.desktop.notion-279328114.scope" -> "com.notion.app.desktop.notion"
pub fn app_id_from_unit(unit: &str) -> Option<String> {
    let rest = unit.strip_prefix("app-")?;
    let (base, scope) = if let Some(b) = rest.strip_suffix(".service") {
        (b.split('@').next().unwrap_or(b), false)
    } else {
        let b = rest.strip_suffix(".scope")?;
        (b, true)
    };
    let parts: Vec<&str> = base.split('-').collect();
    let id = match (parts.len(), scope) {
        (1, _) => parts[0],
        (2, true) => parts[0],  // <id>-<random>
        (2, false) => parts[1], // <launcher>-<id>
        (_, true) => parts[parts.len() - 2],
        (_, false) => parts[parts.len() - 1],
    };
    let id = id.replace("\\x2d", "-");
    (!id.is_empty()).then_some(id)
}

fn unit_of_pid(pid: u32) -> Option<String> {
    let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cg.split('/')
        .find(|s| s.starts_with("app-"))
        .map(|s| s.trim().to_string())
}

/// (app key, app name) of everything running.
pub async fn scan(
    conn: &Connection,
    desktop: &DesktopIndex,
    kwin: Option<&Snapshot>,
) -> Vec<(String, String)> {
    let mut apps: BTreeMap<String, String> = BTreeMap::new();
    let name_for = |id: &str| {
        desktop
            .by_id(id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| pretty(id))
    };
    // Units that hold a windowed process belong to that window's app (e.g. Chromium's own
    // "org.chromium.Chromium.beta-<pid>" scope is Brave).
    let mut covered = std::collections::HashSet::new();
    if let Some(k) = kwin {
        for w in k.windows.iter().filter(|w| w.normal || w.dialog) {
            let (key, name) = desktop.identify(&w.df, &w.cls, w.pid);
            apps.insert(key, name);
            if w.pid > 0
                && let Some(u) = unit_of_pid(w.pid as u32)
            {
                covered.insert(u);
            }
        }
    }
    // Only active app units: a few rows instead of every unit of the session.
    if let Ok(reply) = conn
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "ListUnitsByPatterns",
            &(vec!["active"], vec!["app-*"]),
        )
        .await
        && let Ok(units) = reply.body().deserialize::<Vec<UnitRow>>()
    {
        for u in units
            .iter()
            .filter(|u| u.3 == "active" && !covered.contains(&u.0))
        {
            if let Some(id) = app_id_from_unit(&u.0) {
                let key = desktop
                    .by_id(&id)
                    .map(|e| e.id.clone())
                    .unwrap_or(id.clone());
                apps.entry(key).or_insert_with(|| name_for(&id));
            }
        }
    }
    // Tray icons whose process is not inside an app unit.
    if let Ok(reply) = conn
        .call_method(
            Some("org.kde.StatusNotifierWatcher"),
            "/StatusNotifierWatcher",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(
                "org.kde.StatusNotifierWatcher",
                "RegisteredStatusNotifierItems",
            ),
        )
        .await
        && let Ok(v) = reply.body().deserialize::<OwnedVariant>()
        && let Ok(items) = Vec::<String>::try_from(v)
        && let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await
    {
        for item in items {
            let bus = item.split('/').next().unwrap_or(&item).to_string();
            let Ok(name) = zbus::names::BusName::try_from(bus.as_str()) else {
                continue;
            };
            let Ok(pid) = dbus.get_connection_unix_process_id(name).await else {
                continue;
            };
            // Our own tray icon is not an app you have running.
            if pid == std::process::id() {
                continue;
            }
            if let Some(id) = unit_of_pid(pid).and_then(|u| app_id_from_unit(&u)) {
                let key = desktop
                    .by_id(&id)
                    .map(|e| e.id.clone())
                    .unwrap_or(id.clone());
                apps.entry(key).or_insert_with(|| name_for(&id));
            } else if let Some(st) = procfs::stat(pid) {
                // comm can hold a rewritten process title: keep its first word only.
                let comm = st.comm.split_whitespace().next().unwrap_or_default();
                let (key, name) = desktop.identify("", comm, pid as i64);
                apps.entry(key).or_insert(name);
            }
        }
    }
    apps.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names() {
        assert_eq!(
            app_id_from_unit("app-com.github.wwmm.easyeffects@autostart.service").as_deref(),
            Some("com.github.wwmm.easyeffects")
        );
        assert_eq!(
            app_id_from_unit("app-kitty@14ca0707089d4a44872c26fa8431a2bf.service").as_deref(),
            Some("kitty")
        );
        assert_eq!(
            app_id_from_unit("app-brave\\x2dorigin\\x2dbeta@ab12.service").as_deref(),
            Some("brave-origin-beta")
        );
        assert_eq!(
            app_id_from_unit("app-org.kde.dolphin-12345.scope").as_deref(),
            Some("org.kde.dolphin")
        );
        assert_eq!(
            app_id_from_unit("app-flatpak-com.spotify.Client-9981.scope").as_deref(),
            Some("com.spotify.Client")
        );
        assert_eq!(
            app_id_from_unit("app-KDE-brave\\x2dorigin\\x2dbeta-a81332d8.scope").as_deref(),
            Some("brave-origin-beta")
        );
        assert_eq!(
            app_id_from_unit("app-copyq\\x2dstart.sh@autostart.service").as_deref(),
            Some("copyq-start.sh")
        );
        assert_eq!(app_id_from_unit("dbus-broker.service"), None);
    }
}
