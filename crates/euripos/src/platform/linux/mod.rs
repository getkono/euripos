//! Linux: the network from rtnetlink, metering from `NetworkManager`, the
//! proxy from the environment.

mod netlink;
mod networkmanager;

use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::Duration;
use std::{env, fs, io};

use tokio::sync::watch;

use crate::monitor::Source;
use crate::{
    Constraint, InterfaceKind, IpAvailability, Metering, NetworkFacts, NetworkId, ProxyConfig,
};
use netlink::{Kernel, Link, Route, Signals};

/// The destinations whose routes stand for "the internet": public resolver
/// addresses, so a default route, a split default (`0.0.0.0/1` and
/// `128.0.0.0/1`) and a policy-routed VPN all resolve as they would for a
/// real connection. No packet is sent to them.
const PROBE_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
const PROBE_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888));

/// `ARPHRD_ETHER`.
const ARPHRD_ETHER: u16 = 1;

/// How long opening the source waits for `NetworkManager`'s first answer,
/// so the first reading carries metering. Within the monitor's 500 ms
/// startup wait, leaving room for the reading itself.
const METERING_STARTUP: Duration = Duration::from_millis(250);

/// Open the Linux network source. Must be called on the monitor's runtime.
///
/// Waits up to [`METERING_STARTUP`] for the first metering answer; a slower
/// one arrives as a change.
pub(crate) async fn network_source() -> Option<LinuxSource> {
    let signals = Signals::open().ok()?;
    let kernel = Kernel::open().ok()?;
    let mut metering = networkmanager::watch_metering();
    let _ = tokio::time::timeout(METERING_STARTUP, metering.changed()).await;
    Some(LinuxSource {
        signals,
        kernel,
        metering,
        last: NetworkFacts::UNKNOWN,
    })
}

/// The Linux network source.
pub(crate) struct LinuxSource {
    signals: Signals,
    kernel: Kernel,
    metering: watch::Receiver<Metering>,
    last: NetworkFacts,
}

impl Source for LinuxSource {
    fn observe(&mut self) -> NetworkFacts {
        let metering = *self.metering.borrow_and_update();
        // A failed read says nothing about the network: keep the last facts.
        if let Ok(facts) = read(&mut self.kernel, metering) {
            self.last = facts;
        }
        self.last.clone()
    }

    async fn changed(&mut self) {
        tokio::select! {
            () = self.signals.next() => {}
            Ok(()) = self.metering.changed() => {}
        }
    }
}

/// The facts as the kernel and the environment report them now.
fn read(kernel: &mut Kernel, metering: Metering) -> io::Result<NetworkFacts> {
    let v4 = kernel.route_to(PROBE_V4)?;
    let v6 = kernel.route_to(PROBE_V6)?;
    let interface = match v4.as_ref().or(v6.as_ref()) {
        Some(route) => kernel
            .link(route.interface)?
            .map_or(InterfaceKind::Unknown, |link| interface_kind(&link)),
        None => InterfaceKind::Unknown,
    };
    let network = (v4.is_some() || v6.is_some())
        .then(|| NetworkId::from_key(&(v4.as_ref().map(key), v6.as_ref().map(key))));
    Ok(NetworkFacts {
        network,
        interface,
        metering,
        // Linux has no low-data or constrained mode to read.
        constraint: Constraint::Unknown,
        ipv4: availability(v4.as_ref()),
        ipv6: availability(v6.as_ref()),
        proxy: proxy_config(|name| env::var_os(name)),
    })
}

/// What identifies the network a route leaves through: the interface, the
/// next hop and the source address. An IPv6 source counts by its /64 prefix
/// only, so temporary addresses, which rotate within a prefix, do not read
/// as a new network.
fn key(route: &Route) -> (u32, Option<IpAddr>, Option<IpAddr>) {
    let source = route.source.map(|source| match source {
        IpAddr::V4(_) => source,
        IpAddr::V6(address) => {
            let prefix = u128::from(address) & !u128::from(u64::MAX);
            IpAddr::V6(Ipv6Addr::from(prefix))
        }
    });
    (route.interface, route.gateway, source)
}

fn availability(route: Option<&Route>) -> IpAvailability {
    if route.is_some() {
        IpAvailability::Available
    } else {
        IpAvailability::Unavailable
    }
}

/// The kind of `link`, from its hardware type and what sysfs says of it.
fn interface_kind(link: &Link) -> InterfaceKind {
    // Interface names hold no `/`; refuse one that would leave the directory.
    if link.name.is_empty() || link.name.contains('/') || link.name.starts_with('.') {
        return InterfaceKind::Unknown;
    }
    let sysfs = Path::new("/sys/class/net").join(&link.name);
    let uevent = fs::read_to_string(sysfs.join("uevent")).unwrap_or_default();
    let devtype = uevent
        .lines()
        .find_map(|line| line.strip_prefix("DEVTYPE="));
    classify(
        link.hardware,
        devtype,
        sysfs.join("wireless").exists(),
        sysfs.join("device").exists(),
    )
}

/// The kind of an interface with `ARPHRD_*` type `hardware`, sysfs
/// `DEVTYPE` `devtype`, a `wireless` directory or not, and backed by a
/// `physical` device or not.
fn classify(hardware: u16, devtype: Option<&str>, wireless: bool, physical: bool) -> InterfaceKind {
    match devtype {
        Some("wlan") => InterfaceKind::Wifi,
        Some("wwan") => InterfaceKind::Cellular,
        _ if wireless => InterfaceKind::Wifi,
        None if hardware == ARPHRD_ETHER && physical => InterfaceKind::Ethernet,
        // Loopback, tunnels, bridges, veth pairs, PPP.
        _ => InterfaceKind::Other,
    }
}

/// The proxy configuration the environment holds. On Linux the environment
/// is where euripos reads the proxy from (docs/scope.md, "Platform
/// integration"), so no variable set reads as `Direct`, not `Unknown`.
fn proxy_config(var: impl Fn(&str) -> Option<OsString>) -> ProxyConfig {
    const VARIABLES: [&str; 6] = [
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ];
    if VARIABLES
        .iter()
        .any(|name| var(name).is_some_and(|value| !value.is_empty()))
    {
        ProxyConfig::Manual
    } else {
        ProxyConfig::Direct
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::Instant;

    use super::netlink::Route;
    use super::{
        ARPHRD_ETHER, Kernel, METERING_STARTUP, PROBE_V4, PROBE_V6, classify, key, network_source,
        proxy_config, read,
    };
    use crate::monitor::Source;
    use crate::{Constraint, InterfaceKind, IpAvailability, Metering, ProxyConfig};

    #[test]
    fn interfaces_are_classified_by_devtype_then_hardware() {
        assert_eq!(
            classify(ARPHRD_ETHER, Some("wlan"), true, true),
            InterfaceKind::Wifi
        );
        assert_eq!(
            classify(ARPHRD_ETHER, None, true, true),
            InterfaceKind::Wifi
        );
        assert_eq!(
            classify(519, Some("wwan"), false, true),
            InterfaceKind::Cellular
        );
        assert_eq!(
            classify(ARPHRD_ETHER, None, false, true),
            InterfaceKind::Ethernet
        );
        // veth, bridge: Ethernet framing with no device behind it.
        assert_eq!(
            classify(ARPHRD_ETHER, None, false, false),
            InterfaceKind::Other
        );
        assert_eq!(
            classify(ARPHRD_ETHER, Some("bridge"), false, false),
            InterfaceKind::Other
        );
        // loopback, tun.
        assert_eq!(classify(772, None, false, false), InterfaceKind::Other);
        assert_eq!(classify(65534, None, false, false), InterfaceKind::Other);
    }

    #[test]
    fn a_rotated_ipv6_temporary_address_keeps_the_network() {
        let route = |source: &str| Route {
            interface: 2,
            gateway: Some("fe80::1".parse().unwrap()),
            source: Some(source.parse().unwrap()),
        };
        assert_eq!(
            key(&route("2001:db8:1:2:aaaa::1")),
            key(&route("2001:db8:1:2:bbbb::2"))
        );
        assert_ne!(
            key(&route("2001:db8:1:2::1")),
            key(&route("2001:db8:1:3::1"))
        );
    }

    #[test]
    fn a_new_ipv4_address_or_gateway_is_a_new_network() {
        let route = |gateway: [u8; 4], source: [u8; 4]| Route {
            interface: 2,
            gateway: Some(IpAddr::V4(Ipv4Addr::from(gateway))),
            source: Some(IpAddr::V4(Ipv4Addr::from(source))),
        };
        let home = key(&route([192, 0, 2, 1], [192, 0, 2, 7]));
        assert_ne!(home, key(&route([192, 0, 2, 1], [192, 0, 2, 8])));
        assert_ne!(home, key(&route([192, 0, 2, 254], [192, 0, 2, 7])));
    }

    #[test]
    fn any_proxy_variable_is_a_manual_proxy_and_none_is_direct() {
        let only = |set: &'static str| {
            move |name: &str| (name == set).then(|| OsString::from("http://proxy:3128"))
        };
        assert_eq!(proxy_config(|_| None), ProxyConfig::Direct);
        assert_eq!(proxy_config(|_| Some(OsString::new())), ProxyConfig::Direct);
        assert_eq!(proxy_config(only("https_proxy")), ProxyConfig::Manual);
        assert_eq!(proxy_config(only("ALL_PROXY")), ProxyConfig::Manual);
        assert_eq!(proxy_config(only("no_proxy")), ProxyConfig::Direct);
    }

    #[test]
    fn the_probes_are_public_addresses() {
        assert!(matches!(PROBE_V4, IpAddr::V4(address) if !address.is_private()));
        assert!(matches!(PROBE_V6, IpAddr::V6(address) if address.segments()[0] == 0x2001));
        assert_ne!(PROBE_V6, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }

    // The host's own kernel: whatever the host's network, the facts agree
    // with themselves.
    #[test]
    fn the_host_network_is_named_exactly_when_a_route_exists() {
        let mut kernel = Kernel::open().unwrap();
        let facts = read(&mut kernel, Metering::Unknown).unwrap();
        let routed =
            facts.ipv4 == IpAvailability::Available || facts.ipv6 == IpAvailability::Available;
        assert_eq!(facts.network.is_some(), routed);
        assert_ne!(facts.ipv4, IpAvailability::Unknown);
        assert_ne!(facts.ipv6, IpAvailability::Unknown);
        assert_eq!(facts.constraint, Constraint::Unknown);
        assert_eq!(facts.interface == InterfaceKind::Unknown, !routed);
        // Two reads of an unchanged network name it the same.
        assert_eq!(
            read(&mut kernel, Metering::Unknown).unwrap().network,
            facts.network
        );
    }

    // Whether the host runs `NetworkManager`, has a system bus without it,
    // or has no system bus, the first metering answer comes before the
    // startup wait ends, so the first reading carries it.
    #[tokio::test]
    async fn the_source_opens_with_the_first_metering_answer_and_reads() {
        let opening = Instant::now();
        let mut source = network_source().await.unwrap();
        assert!(opening.elapsed() < METERING_STARTUP);
        let facts = source.observe();
        assert_ne!(facts.ipv4, IpAvailability::Unknown);
    }
}
