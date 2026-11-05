//! `NetworkManager`'s `Metered` property over the system D-Bus.
//!
//! Where `NetworkManager` is absent, or the system bus is, metering is
//! `Unknown` (scope, Principle 4).

use std::future::poll_fn;
use std::pin::pin;

use futures_core::Stream;
use tokio::sync::watch;

use crate::Metering;

const DESTINATION: &str = "org.freedesktop.NetworkManager";
const PATH: &str = "/org/freedesktop/NetworkManager";
const INTERFACE: &str = "org.freedesktop.NetworkManager";

/// Follow `NetworkManager`'s `Metered` on the current runtime.
///
/// The receiver holds `Unknown` until `NetworkManager` answers, and again if
/// it goes away. Its first change is the first answer: `NetworkManager`'s
/// value, or `Unknown` where it, or the system bus, is absent.
pub(super) fn watch_metering() -> watch::Receiver<Metering> {
    let (tx, rx) = watch::channel(Metering::Unknown);
    tokio::spawn(async move {
        let _ = async {
            let connection = zbus::Connection::system().await?;
            follow(&connection, DESTINATION, &tx).await
        }
        .await;
        tx.send_replace(Metering::Unknown);
        // Keep the sender, so the receiver reads `Unknown` rather than closed.
        std::future::pending::<()>().await;
    });
    rx
}

/// Publish to `tx` the `Metered` of whichever process owns `destination`
/// on `connection`, and `Unknown` while none does. Returns only on a bus
/// error.
async fn follow(
    connection: &zbus::Connection,
    destination: &str,
    tx: &watch::Sender<Metering>,
) -> zbus::Result<()> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    // Subscribed before the owner is asked for, so no change falls between.
    let mut owners = pin!(
        bus.receive_name_owner_changed_with_args(&[(0, destination)])
            .await?
    );
    if !bus.name_has_owner(destination.try_into()?).await? {
        tx.send_replace(Metering::Unknown);
    }
    loop {
        let change = tokio::select! {
            result = follow_owner(connection, destination, tx) => return result,
            change = poll_fn(|cx| owners.as_mut().poll_next(cx)) => change,
        };
        let Some(change) = change else {
            return Ok(());
        };
        // The owner left: its last value no longer holds.
        if change.args()?.new_owner().is_none() {
            tx.send_replace(Metering::Unknown);
        }
        // Follow the next owner, if any, through a proxy of its own.
    }
}

/// Publish to `tx` the `Metered` the current owner of `destination` reports.
///
/// A proxy caches properties and keeps that cache when the owner leaves, so
/// each owner gets a fresh proxy, whose first value is that owner's.
async fn follow_owner(
    connection: &zbus::Connection,
    destination: &str,
    tx: &watch::Sender<Metering>,
) -> zbus::Result<()> {
    let proxy = zbus::Proxy::new(connection, destination, PATH, INTERFACE).await?;
    // Yields the current value first, then each change.
    let mut changes = pin!(proxy.receive_property_changed::<u32>("Metered").await);
    while let Some(change) = poll_fn(|cx| changes.as_mut().poll_next(cx)).await {
        tx.send_replace(change.get().await.map_or(Metering::Unknown, metering));
    }
    Ok(())
}

/// The metering `NetworkManager`'s `NMMetered` value reports.
///
/// `NM_METERED_YES` (1) and `NM_METERED_GUESS_YES` (3) are metered;
/// `NM_METERED_NO` (2) and `NM_METERED_GUESS_NO` (4) are not;
/// `NM_METERED_UNKNOWN` (0) and anything newer are `Unknown`. A guess counts
/// as the value it guesses: `NetworkManager` guesses yes from evidence on the
/// network itself (an Android hotspot's DHCP option, a cellular or Bluetooth
/// device), and treating a likely-metered link as unmetered costs the user
/// money, while the reverse costs only a deferred background transfer.
pub(super) fn metering(value: u32) -> Metering {
    match value {
        1 | 3 => Metering::Metered,
        2 | 4 => Metering::Unmetered,
        _ => Metering::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::watch;
    use tokio::time::timeout;

    use super::{follow, metering};
    use crate::Metering;

    // The owner-tracking path, against a name this test owns on the session
    // bus: the system bus does not let an unprivileged process own a name.
    #[tokio::test]
    async fn metering_is_answered_with_no_owner_and_unknown_once_the_owner_leaves() {
        let (Ok(watcher), Ok(owner)) = (
            zbus::Connection::session().await,
            zbus::Connection::session().await,
        ) else {
            eprintln!("skipped: no session bus");
            return;
        };
        let name = format!("org.euripos.Test.P{}", std::process::id());
        let tx = Arc::new(watch::channel(Metering::Metered).0);
        let mut rx = tx.subscribe();
        let follower = {
            let (tx, name) = (Arc::clone(&tx), name.clone());
            tokio::spawn(async move { follow(&watcher, &name, &tx).await })
        };
        let within = Duration::from_secs(5);

        // Nobody owns the name: the answer is `Unknown`, at once.
        timeout(within, rx.changed()).await.unwrap().unwrap();
        assert_eq!(*rx.borrow_and_update(), Metering::Unknown);

        // An owner that answered metered, then left.
        owner.request_name(name.as_str()).await.unwrap();
        tx.send_replace(Metering::Metered);
        rx.borrow_and_update();
        owner.release_name(name.as_str()).await.unwrap();
        timeout(within, rx.changed()).await.unwrap().unwrap();
        assert_eq!(*rx.borrow_and_update(), Metering::Unknown);

        follower.abort();
    }

    #[test]
    fn networkmanager_metered_values_map_as_documented() {
        assert_eq!(metering(0), Metering::Unknown);
        assert_eq!(metering(1), Metering::Metered);
        assert_eq!(metering(2), Metering::Unmetered);
        assert_eq!(metering(3), Metering::Metered);
        assert_eq!(metering(4), Metering::Unmetered);
        assert_eq!(metering(5), Metering::Unknown);
    }
}
