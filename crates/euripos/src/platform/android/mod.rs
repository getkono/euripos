//! Android: the default network from `ConnectivityManager`'s callback over
//! JNI, sockets bound to it with `android_setsocknetwork`, its proxy, and the
//! platform verifier initialised from the app's `Context`. See
//! docs/design/android.md.

mod jvm;
mod proxy;

use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use tokio::net::TcpSocket;
use tokio::sync::Notify;

use crate::monitor::Source;
use crate::{Constraint, InterfaceKind, IpAvailability, Metering, NetworkFacts, NetworkId};

/// How long opening the source waits for the callback's first report, which
/// `ConnectivityManager` sends from its own thread once the callback is
/// registered. Within the monitor's 500 ms startup wait, as on Linux.
const FIRST_REPORT: Duration = Duration::from_millis(250);

/// The bits of a report's `flags`, as `NetworkMonitor.java` sets them.
mod flag {
    /// The network's `NetworkCapabilities` have been reported.
    pub(super) const CAPABILITIES: u32 = 1;
    /// `NET_CAPABILITY_NOT_METERED`.
    pub(super) const NOT_METERED: u32 = 1 << 1;
    /// `NET_CAPABILITY_TEMPORARILY_NOT_METERED`.
    pub(super) const TEMPORARILY_NOT_METERED: u32 = 1 << 2;
    /// The network's `LinkProperties` have been reported.
    pub(super) const LINK: u32 = 1 << 3;
    /// A default IPv4 route.
    pub(super) const IPV4: u32 = 1 << 4;
    /// A default IPv6 route.
    pub(super) const IPV6: u32 = 1 << 5;
    /// The platform blocks this app's traffic on the network.
    pub(super) const BLOCKED: u32 = 1 << 6;
    /// `getRestrictBackgroundStatus` gave a known status.
    pub(super) const DATA_SAVER_KNOWN: u32 = 1 << 7;
    /// Data Saver is on, and this app is not exempt from it.
    pub(super) const DATA_SAVER: u32 = 1 << 8;
}

/// `NetworkCapabilities.TRANSPORT_*`, as bit positions in a report's
/// `transports`.
mod transport {
    pub(super) const CELLULAR: u32 = 0;
    pub(super) const WIFI: u32 = 1;
    pub(super) const ETHERNET: u32 = 3;
    pub(super) const VPN: u32 = 4;
}

/// What the monitor's Java last reported about the default network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Report {
    /// `Network.getNetworkHandle()`; 0 where there is no default network.
    handle: u64,
    /// One bit per transport the network has, at its `TRANSPORT_*` value.
    transports: u32,
    /// [`flag`] bits.
    flags: u32,
    /// The proxy, `getDefaultProxy()`.
    proxy: proxy::Settings,
}

/// The last report, and a wake for the monitor when it changes. One per
/// process, as the callback is.
struct Shared {
    report: Mutex<Report>,
    notify: Notify,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        report: Mutex::new(Report::default()),
        notify: Notify::new(),
    })
}

/// Called from the callback's thread with every report.
fn report(report: Report) {
    let shared = shared();
    *shared.report.lock().unwrap_or_else(PoisonError::into_inner) = report;
    shared.notify.notify_one();
}

/// The last report.
fn last() -> Report {
    shared()
        .report
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Open the Android network source: register the default-network callback,
/// and wait up to [`FIRST_REPORT`] for its first report. `None` where the
/// app gave `ndk-context` no JVM, or the callback cannot be registered (the
/// app lacks `ACCESS_NETWORK_STATE`).
pub(crate) async fn network_source() -> Option<AndroidSource> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    let started =
        *STARTED.get_or_init(|| jvm::jvm().is_some_and(|jvm| jvm.start_monitor().is_ok()));
    if !started {
        return None;
    }
    let _ = tokio::time::timeout(FIRST_REPORT, shared().notify.notified()).await;
    Some(AndroidSource)
}

/// The Android network source: what the callback last reported.
pub(crate) struct AndroidSource;

impl Source for AndroidSource {
    fn observe(&mut self) -> NetworkFacts {
        facts(&last())
    }

    async fn changed(&mut self) {
        shared().notify.notified().await;
    }
}

/// The facts a report describes.
fn facts(report: &Report) -> NetworkFacts {
    let has = |bit| report.flags & bit != 0;
    if report.handle == 0 {
        // No default network: nothing routes, and nothing else is known.
        return NetworkFacts {
            ipv4: IpAvailability::Unavailable,
            ipv6: IpAvailability::Unavailable,
            ..NetworkFacts::UNKNOWN
        };
    }
    let (interface, metering) = if has(flag::CAPABILITIES) {
        (interface(report.transports), metering(report.flags))
    } else {
        (InterfaceKind::Unknown, Metering::Unknown)
    };
    // Data Saver limits background traffic on metered networks only.
    let constraint = match (metering, has(flag::DATA_SAVER_KNOWN)) {
        (Metering::Unknown, _) | (_, false) => Constraint::Unknown,
        (Metering::Metered, true) if has(flag::DATA_SAVER) => Constraint::Constrained,
        _ => Constraint::Unconstrained,
    };
    let route = |bit| {
        if !has(flag::LINK) {
            IpAvailability::Unknown
        } else if has(bit) {
            IpAvailability::Available
        } else {
            IpAvailability::Unavailable
        }
    };
    NetworkFacts {
        network: Some(NetworkId::from_key(&report.handle)),
        interface,
        metering,
        constraint,
        ipv4: route(flag::IPV4),
        ipv6: route(flag::IPV6),
        proxy: report.proxy.config(),
        blocked: has(flag::BLOCKED),
    }
}

/// The kind of a network with these transports. A VPN is other, as a tunnel
/// is on Linux and Windows, whatever it runs over.
fn interface(transports: u32) -> InterfaceKind {
    let has = |transport: u32| transports & (1 << transport) != 0;
    if has(transport::VPN) {
        InterfaceKind::Other
    } else if has(transport::WIFI) {
        InterfaceKind::Wifi
    } else if has(transport::CELLULAR) {
        InterfaceKind::Cellular
    } else if has(transport::ETHERNET) {
        InterfaceKind::Ethernet
    } else if transports != 0 {
        // Bluetooth, USB, Thread, satellite and the rest.
        InterfaceKind::Other
    } else {
        InterfaceKind::Unknown
    }
}

/// Metering from the capabilities: a network without `NOT_METERED` is
/// metered, as `ConnectivityManager.isActiveNetworkMetered` reads it.
fn metering(flags: u32) -> Metering {
    if flags & flag::NOT_METERED != 0 {
        Metering::Unmetered
    } else if flags & flag::TEMPORARILY_NOT_METERED != 0 {
        Metering::TemporarilyUnmetered
    } else {
        Metering::Metered
    }
}

/// Where a connection to `host` should go under the default network's proxy
/// settings.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the proxy path, #49")
)]
pub(crate) fn proxy_for(host: &str) -> proxy::Hop {
    last().proxy.hop(host)
}

/// Bind `socket`, before it connects, to the default network the callback
/// last reported, so the connection stays on the network the client
/// believes it is on. Unbound where no callback runs or no network is
/// reported: the socket then takes the process's default network.
pub(crate) fn bind_to_network(socket: &TcpSocket) -> io::Result<()> {
    match last().handle {
        0 => Ok(()),
        handle => bind(socket.as_raw_fd(), handle),
    }
}

/// `android_setsocknetwork`: bind the socket `fd` to the network `handle`
/// names; 0 (`NETWORK_UNSPECIFIED`) unbinds it.
fn bind(fd: RawFd, handle: u64) -> io::Result<()> {
    // SAFETY: `fd` is an open socket the caller holds for the call.
    if unsafe { android_setsocknetwork(handle, fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// API 23; `net_handle_t` is a `uint64_t`.
#[link(name = "android")]
unsafe extern "C" {
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
}

/// Whether rustls-platform-verifier can verify certificates in this process.
///
/// On Android it calls into the JVM, and panics while verifying when it was
/// never given one, so it is initialised here, once, from the `Context` the
/// app gave `ndk-context`. Without one it is reported unavailable and an
/// `https` request fails with `ErrorKind::Tls`.
pub(crate) fn platform_verifier() -> Result<(), &'static str> {
    static VERIFIER: OnceLock<Result<(), &'static str>> = OnceLock::new();
    *VERIFIER.get_or_init(|| {
        let jvm = jvm::jvm().ok_or("the android context is not initialised (ndk-context)")?;
        jvm.init_verifier()
            .map_err(|_| "the android platform verifier failed to initialise")
    })
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::os::fd::AsRawFd;

    use tokio::net::{TcpListener, TcpSocket};

    use super::{Report, bind, facts, flag, interface, network_source, proxy, transport};
    use crate::{Constraint, InterfaceKind, IpAvailability, Metering, ProxyConfig};

    fn on(handle: u64, transports: u32, flags: u32) -> Report {
        Report {
            handle,
            transports,
            flags,
            proxy: proxy::Settings::default(),
        }
    }

    const KNOWN: u32 = flag::CAPABILITIES | flag::LINK | flag::DATA_SAVER_KNOWN;

    #[test]
    fn no_default_network_routes_nothing_and_names_nothing() {
        let facts = facts(&Report::default());
        assert_eq!(facts.network, None);
        assert_eq!(facts.ipv4, IpAvailability::Unavailable);
        assert_eq!(facts.ipv6, IpAvailability::Unavailable);
        assert_eq!(facts.interface, InterfaceKind::Unknown);
        assert_eq!(facts.metering, Metering::Unknown);
        assert_eq!(facts.proxy, ProxyConfig::Unknown);
    }

    #[test]
    fn a_network_is_named_by_its_handle() {
        let wifi = facts(&on(0x64_cafe_d00d, 1 << transport::WIFI, KNOWN));
        assert!(wifi.network.is_some());
        assert_eq!(
            wifi.network,
            facts(&on(0x64_cafe_d00d, 0, 0)).network,
            "the identity is the handle's alone"
        );
        assert_ne!(
            wifi.network,
            facts(&on(0x65_cafe_d00d, 1 << transport::WIFI, KNOWN)).network
        );
    }

    #[test]
    fn facts_not_yet_reported_are_unknown() {
        let facts = facts(&on(7, 0, 0));
        assert!(facts.network.is_some());
        assert_eq!(facts.interface, InterfaceKind::Unknown);
        assert_eq!(facts.metering, Metering::Unknown);
        assert_eq!(facts.constraint, Constraint::Unknown);
        assert_eq!(facts.ipv4, IpAvailability::Unknown);
        assert_eq!(facts.ipv6, IpAvailability::Unknown);
        assert_eq!(facts.proxy, ProxyConfig::Direct);
        assert!(!facts.blocked);
    }

    #[test]
    fn transports_map_to_interface_kinds() {
        assert_eq!(interface(1 << transport::WIFI), InterfaceKind::Wifi);
        assert_eq!(interface(1 << transport::CELLULAR), InterfaceKind::Cellular);
        assert_eq!(interface(1 << transport::ETHERNET), InterfaceKind::Ethernet);
        // A VPN reports the transports it runs over too.
        assert_eq!(
            interface(1 << transport::VPN | 1 << transport::WIFI),
            InterfaceKind::Other
        );
        // Bluetooth.
        assert_eq!(interface(1 << 2), InterfaceKind::Other);
        assert_eq!(interface(0), InterfaceKind::Unknown);
    }

    #[test]
    fn capabilities_map_to_metering() {
        let metering = |flags| facts(&on(7, 1, KNOWN | flags)).metering;
        assert_eq!(metering(flag::NOT_METERED), Metering::Unmetered);
        assert_eq!(
            metering(flag::TEMPORARILY_NOT_METERED),
            Metering::TemporarilyUnmetered
        );
        assert_eq!(metering(0), Metering::Metered);
    }

    #[test]
    fn data_saver_constrains_metered_networks_only() {
        let constraint = |flags| facts(&on(7, 1, flag::CAPABILITIES | flags)).constraint;
        let saver = flag::DATA_SAVER_KNOWN | flag::DATA_SAVER;
        assert_eq!(constraint(saver), Constraint::Constrained);
        assert_eq!(
            constraint(saver | flag::NOT_METERED),
            Constraint::Unconstrained
        );
        assert_eq!(
            constraint(saver | flag::TEMPORARILY_NOT_METERED),
            Constraint::Unconstrained
        );
        assert_eq!(
            constraint(flag::DATA_SAVER_KNOWN),
            Constraint::Unconstrained
        );
        assert_eq!(constraint(0), Constraint::Unknown);
    }

    #[test]
    fn default_routes_map_to_ip_availability() {
        let facts = facts(&on(7, 1, KNOWN | flag::IPV6));
        assert_eq!(facts.ipv4, IpAvailability::Unavailable);
        assert_eq!(facts.ipv6, IpAvailability::Available);
    }

    #[test]
    fn a_blocked_network_is_reported_blocked() {
        assert!(facts(&on(7, 1, KNOWN | flag::BLOCKED)).blocked);
        assert!(!facts(&on(7, 1, KNOWN)).blocked);
    }

    #[test]
    fn the_proxy_fact_follows_the_reported_settings() {
        let mut report = on(7, 1, KNOWN);
        report.proxy = proxy::Settings::new(Some("proxy.example".into()), 3128, None, None);
        assert_eq!(facts(&report).proxy, ProxyConfig::Manual);
    }

    // The emulator's test binaries run outside any app: no JVM, no Context.
    #[tokio::test]
    async fn without_an_android_context_nothing_is_observed_or_verified() {
        assert!(network_source().await.is_none());
        assert!(super::platform_verifier().is_err());
        assert_eq!(super::proxy_for("example.com"), proxy::Hop::Direct);
    }

    // The binding is the platform's: unspecified unbinds, a value that is
    // not a handle is refused, and so is a handle naming no network. A
    // handle is the network's id over `Network`'s magic, 0xcafed00d.
    #[tokio::test]
    async fn sockets_bind_to_a_network_by_handle() {
        let socket = TcpSocket::new_v4().unwrap();
        bind(socket.as_raw_fd(), 0).unwrap();
        assert!(bind(socket.as_raw_fd(), 1).is_err());
        assert!(bind(socket.as_raw_fd(), 0x7fff_0000_dead_beef).is_err());
        assert!(bind(socket.as_raw_fd(), 0x7fff_cafe_d00d).is_err());
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        socket.connect(address).await.unwrap();
    }

    // With no callback running, connections go out unbound.
    #[tokio::test]
    async fn an_unreported_network_leaves_sockets_unbound() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let socket = TcpSocket::new_v4().unwrap();
        super::bind_to_network(&socket).unwrap();
        socket.connect(address).await.unwrap();
    }
}
