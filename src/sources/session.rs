//! Lock screen (org.freedesktop.ScreenSaver + logind LockedHint) and suspend (logind
//! PrepareForSleep, with a delay inhibitor so data is flushed before the machine sleeps).

use crate::daemon::Event;
use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use zbus::{Connection, MatchRule, MessageStream, OwnedFd, OwnedObjectPath, OwnedVariant};

pub async fn spawn(session: Connection, tx: UnboundedSender<Event>) {
    // Screen saver / lock (session bus).
    {
        let conn = session.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = watch_screensaver(conn, tx).await {
                tracing::warn!("lock-screen signal unavailable: {e:#}");
            }
        });
    }
    // Suspend (system bus).
    tokio::spawn(async move {
        if let Err(e) = watch_sleep(tx).await {
            tracing::warn!("suspend detection unavailable: {e:#}");
        }
    });
}

async fn watch_screensaver(conn: Connection, tx: UnboundedSender<Event>) -> Result<()> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.ScreenSaver")
        .member("ActiveChanged")
        .build()?;
    let mut stream = MessageStream::for_match_rule(rule, &conn, None).await?;
    if let Ok(reply) = conn
        .call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "GetActive",
            &(),
        )
        .await
        && let Ok(active) = reply.body().deserialize::<bool>()
    {
        let _ = tx.send(Event::Locked(active));
    }
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        if let Ok(active) = msg.body().deserialize::<bool>() {
            let _ = tx.send(Event::Locked(active));
        }
    }
    Ok(())
}

async fn inhibit(sys: &Connection) -> Option<OwnedFd> {
    let reply = sys
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &("sleep", "arch-tracker", "Saving screen-time data", "delay"),
        )
        .await
        .ok()?;
    reply.body().deserialize::<OwnedFd>().ok()
}

async fn watch_sleep(tx: UnboundedSender<Event>) -> Result<()> {
    let sys = crate::util::system_bus().await?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.login1.Manager")
        .member("PrepareForSleep")
        .build()?;
    let mut stream = MessageStream::for_match_rule(rule, &sys, None).await?;
    let mut lock = inhibit(&sys).await;
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        let Ok(going) = msg.body().deserialize::<bool>() else {
            continue;
        };
        if going {
            let (done_tx, done_rx) = oneshot::channel();
            let _ = tx.send(Event::Sleep {
                going: true,
                done: Some(done_tx),
            });
            // Give the tracker up to 2 s to flush, then let the system sleep.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), done_rx).await;
            lock = None;
        } else {
            let _ = tx.send(Event::Sleep {
                going: false,
                done: None,
            });
            if lock.is_none() {
                lock = inhibit(&sys).await;
            }
        }
    }
    drop(lock);
    Ok(())
}

/// logind's LockedHint for this user's graphical session (backup for the ScreenSaver signal).
pub async fn locked_hint(sys: &Connection) -> Option<bool> {
    let uid = unsafe { libc::getuid() };
    let user = sys
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "GetUser",
            &(uid,),
        )
        .await
        .ok()?;
    let user_path: OwnedObjectPath = user.body().deserialize().ok()?;
    let display = sys
        .call_method(
            Some("org.freedesktop.login1"),
            user_path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.login1.User", "Display"),
        )
        .await
        .ok()?;
    let v: OwnedVariant = display.body().deserialize().ok()?;
    let (_, session_path): (String, OwnedObjectPath) = v.try_into().ok()?;
    let hint = sys
        .call_method(
            Some("org.freedesktop.login1"),
            session_path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.login1.Session", "LockedHint"),
        )
        .await
        .ok()?;
    let v: OwnedVariant = hint.body().deserialize().ok()?;
    bool::try_from(v).ok()
}
