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
/// it goes away.
pub(super) fn watch_metering() -> watch::Receiver<Metering> {
    let (tx, rx) = watch::channel(Metering::Unknown);
    tokio::spawn(async move {
        let _ = follow(&tx).await;
        tx.send_replace(Metering::Unknown);
        // Keep the sender, so the receiver reads `Unknown` rather than closed.
        std::future::pending::<()>().await;
    });
    rx
}

async fn follow(tx: &watch::Sender<Metering>) -> zbus::Result<()> {
    let connection = zbus::Connection::system().await?;
    let proxy = zbus::Proxy::new(&connection, DESTINATION, PATH, INTERFACE).await?;
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
    use super::metering;
    use crate::Metering;

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
