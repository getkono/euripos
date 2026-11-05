//! Windows: the network from IP Helper, its cost from the Network List
//! Manager, the proxy from `WinHTTP`.

mod change;
mod cost;
mod proxy;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use tokio::sync::Notify;
use windows::Win32::Foundation::{NO_ERROR, WIN32_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetBestRoute2, GetIfEntry2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IF_TYPE_WWANPP,
    IF_TYPE_WWANPP2, MIB_IF_ROW2, MIB_IPFORWARD_ROW2,
};
use windows::Win32::NetworkManagement::Ndis::{
    NDIS_PHYSICAL_MEDIUM, NdisPhysicalMediumNative802_11, NdisPhysicalMediumWirelessWan,
};
use windows::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0, SOCKADDR_IN, SOCKADDR_IN6,
    SOCKADDR_INET,
};

use crate::monitor::Source;
use crate::{Constraint, InterfaceKind, IpAvailability, Metering, NetworkFacts, NetworkId};
use change::Changes;
use cost::Cost;

/// The destinations whose routes stand for "the internet", as on Linux:
/// public resolver addresses, asked for with `GetBestRoute2`, which applies
/// the route table the way a connection does. No packet is sent to them.
const PROBE_V4: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
const PROBE_V6: Ipv6Addr = Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888);

/// Open the Windows network source. Must be called on the monitor's thread:
/// it joins the multithreaded COM apartment there.
#[allow(
    clippy::unused_async,
    reason = "the signature is every platform's; Linux opens its source asynchronously"
)]
pub(crate) async fn network_source() -> Option<WindowsSource> {
    let notify = Arc::new(Notify::new());
    let changes = Changes::open(&notify).ok()?;
    // Without the cost manager or the settings key the facts are still
    // read; metering is `Unknown`, and a proxy change waits for the next
    // network signal.
    let cost = Cost::open(&notify).ok();
    let proxy = proxy::Watch::open(&notify).ok();
    Some(WindowsSource {
        notify,
        _changes: changes,
        _proxy: proxy,
        cost,
    })
}

/// The Windows network source.
pub(crate) struct WindowsSource {
    notify: Arc<Notify>,
    _changes: Changes,
    _proxy: Option<proxy::Watch>,
    cost: Option<Cost>,
}

impl Source for WindowsSource {
    fn observe(&mut self) -> NetworkFacts {
        let (metering, constraint) = self
            .cost
            .as_ref()
            .map_or((Metering::Unknown, Constraint::Unknown), Cost::read);
        let v4 = best_route(IpAddr::V4(PROBE_V4));
        let v6 = best_route(IpAddr::V6(PROBE_V6));
        let interface = v4
            .as_ref()
            .or(v6.as_ref())
            .map_or(InterfaceKind::Unknown, |route| {
                interface_kind(route.interface)
            });
        let network = (v4.is_some() || v6.is_some())
            .then(|| NetworkId::from_key(&(v4.as_ref().map(key), v6.as_ref().map(key))));
        NetworkFacts {
            network,
            interface,
            metering,
            constraint,
            ipv4: availability(v4.as_ref()),
            ipv6: availability(v6.as_ref()),
            proxy: proxy::config(),
            ..NetworkFacts::UNKNOWN
        }
    }

    async fn changed(&mut self) {
        self.notify.notified().await;
    }
}

/// The route the system would send traffic to one destination through.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Route {
    interface: u32,
    gateway: Option<IpAddr>,
    source: Option<IpAddr>,
}

/// The best route to `destination`, or `None` where there is none.
///
/// Any failure reads as no route: `GetBestRoute2` fails with
/// `ERROR_NETWORK_UNREACHABLE` where nothing routes there, and with other
/// codes where the IP version is not installed, which means the same.
fn best_route(destination: IpAddr) -> Option<Route> {
    let destination = to_sockaddr(destination);
    let mut route = MIB_IPFORWARD_ROW2::default();
    let mut source = SOCKADDR_INET::default();
    // SAFETY: `destination` outlives the call; `route` and `source` are
    // written by it.
    let status = unsafe {
        GetBestRoute2(
            None,
            0,
            None,
            &raw const destination,
            0,
            &raw mut route,
            &raw mut source,
        )
    };
    if status != NO_ERROR {
        return None;
    }
    Some(Route {
        interface: route.InterfaceIndex,
        gateway: from_sockaddr(&route.NextHop).filter(|gateway| !gateway.is_unspecified()),
        source: from_sockaddr(&source),
    })
}

fn to_sockaddr(address: IpAddr) -> SOCKADDR_INET {
    match address {
        IpAddr::V4(address) => SOCKADDR_INET {
            Ipv4: SOCKADDR_IN {
                sin_family: AF_INET,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(address.octets()),
                    },
                },
                ..Default::default()
            },
        },
        IpAddr::V6(address) => SOCKADDR_INET {
            Ipv6: SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: address.octets(),
                    },
                },
                ..Default::default()
            },
        },
    }
}

fn from_sockaddr(address: &SOCKADDR_INET) -> Option<IpAddr> {
    // SAFETY: every variant starts with the family, which says which one
    // the rest of the union holds; each address field is plain bytes.
    unsafe {
        match address.si_family {
            AF_INET => Some(IpAddr::V4(Ipv4Addr::from(
                address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
            ))),
            AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(address.Ipv6.sin6_addr.u.Byte))),
            _ => None,
        }
    }
}

/// What identifies the network a route leaves through, as on Linux: the
/// interface, the next hop and the source address, an IPv6 source by its
/// /64 prefix only, so a rotated temporary address keeps the network.
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

/// The kind of the interface with index `index`, from its `MIB_IF_ROW2`.
fn interface_kind(index: u32) -> InterfaceKind {
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    // SAFETY: `row` names the interface by index and is written by the call.
    if unsafe { GetIfEntry2(&raw mut row) } != NO_ERROR {
        return InterfaceKind::Unknown;
    }
    // `HardwareInterface` is the first bit of the flags.
    let hardware = row.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
    classify(row.Type, row.PhysicalMediumType, hardware)
}

/// The kind of an interface of `IF_TYPE_*` type `kind` over physical medium
/// `medium`, backed by hardware or not.
fn classify(kind: u32, medium: NDIS_PHYSICAL_MEDIUM, hardware: bool) -> InterfaceKind {
    if kind == IF_TYPE_IEEE80211 || medium == NdisPhysicalMediumNative802_11 {
        InterfaceKind::Wifi
    } else if kind == IF_TYPE_WWANPP
        || kind == IF_TYPE_WWANPP2
        || medium == NdisPhysicalMediumWirelessWan
    {
        InterfaceKind::Cellular
    } else if kind == IF_TYPE_ETHERNET_CSMACD && hardware {
        InterfaceKind::Ethernet
    } else {
        // Loopback, tunnels, VPN adapters, Hyper-V's virtual Ethernet.
        InterfaceKind::Other
    }
}

/// A Win32 status as an `io::Result`.
fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == NO_ERROR {
        Ok(())
    } else {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "a Win32 error code is the OS error's bit pattern"
        )]
        Err(io::Error::from_raw_os_error(status.0 as i32))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::sync::Arc;
    use std::time::Duration;

    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;
    use tokio_rustls::TlsAcceptor;
    use windows::Win32::NetworkManagement::IpHelper::{
        IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL,
        IF_TYPE_WWANPP2,
    };
    use windows::Win32::NetworkManagement::Ndis::{
        NDIS_PHYSICAL_MEDIUM, NdisPhysicalMedium802_3, NdisPhysicalMediumNative802_11,
        NdisPhysicalMediumWirelessWan,
    };

    use super::{
        Cost, PROBE_V4, PROBE_V6, Route, best_route, classify, cost, from_sockaddr, key,
        network_source, to_sockaddr,
    };
    use crate::monitor::Source;
    use crate::tls::Trust;
    use crate::{Constraint, ErrorKind, InterfaceKind, IpAvailability, Metering};

    const UNSPECIFIED: NDIS_PHYSICAL_MEDIUM = NDIS_PHYSICAL_MEDIUM(0);

    #[test]
    fn interfaces_are_classified_by_type_medium_and_hardware() {
        assert_eq!(
            classify(IF_TYPE_IEEE80211, NdisPhysicalMediumNative802_11, true),
            InterfaceKind::Wifi
        );
        // A Wi-Fi adapter can report Ethernet framing; the medium says.
        assert_eq!(
            classify(
                IF_TYPE_ETHERNET_CSMACD,
                NdisPhysicalMediumNative802_11,
                true
            ),
            InterfaceKind::Wifi
        );
        assert_eq!(
            classify(IF_TYPE_WWANPP2, NdisPhysicalMediumWirelessWan, true),
            InterfaceKind::Cellular
        );
        assert_eq!(
            classify(IF_TYPE_ETHERNET_CSMACD, NdisPhysicalMedium802_3, true),
            InterfaceKind::Ethernet
        );
        // Hyper-V's virtual switch and VPN adapters: Ethernet with no
        // hardware behind it.
        assert_eq!(
            classify(IF_TYPE_ETHERNET_CSMACD, UNSPECIFIED, false),
            InterfaceKind::Other
        );
        assert_eq!(
            classify(IF_TYPE_SOFTWARE_LOOPBACK, UNSPECIFIED, false),
            InterfaceKind::Other
        );
        assert_eq!(
            classify(IF_TYPE_TUNNEL, UNSPECIFIED, false),
            InterfaceKind::Other
        );
    }

    #[test]
    fn socket_addresses_round_trip() {
        for address in [IpAddr::V4(PROBE_V4), IpAddr::V6(PROBE_V6)] {
            assert_eq!(from_sockaddr(&to_sockaddr(address)), Some(address));
        }
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
    fn loopback_routes_through_loopback() {
        let route = best_route(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert_eq!(route.source, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(route.gateway, None);
        assert!(best_route(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_some());
    }

    // The host's own network: whatever it is, the facts agree with
    // themselves, and the hosted runners route IPv4 to the internet.
    #[test]
    fn the_host_network_is_named_exactly_when_a_route_exists() {
        std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut source = network_source().await.unwrap();
                let facts = source.observe();
                // The cost manager is open, or this edition provides none
                // and the cost is `Unknown`.
                if source.cost.is_none() {
                    let error = Cost::open(&Arc::new(Notify::new())).err().unwrap();
                    assert!(cost::unregistered(&error), "{error}");
                    assert_eq!(facts.metering, Metering::Unknown);
                    assert_eq!(facts.constraint, Constraint::Unknown);
                }
                assert_eq!(facts.ipv4, IpAvailability::Available, "{facts:?}");
                assert!(facts.network.is_some());
                assert_ne!(facts.interface, InterfaceKind::Unknown);
                assert_ne!(facts.ipv6, IpAvailability::Unknown);
                // Two reads of an unchanged network name it the same.
                assert_eq!(source.observe().network, facts.network);
            });
        })
        .join()
        .unwrap();
    }

    // rustls-platform-verifier on Windows verifies through CryptoAPI and
    // needs no initialisation: a self-signed certificate no root vouches for
    // fails the handshake as a TLS error.
    #[tokio::test]
    async fn the_platform_verifier_rejects_an_untrusted_certificate() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![key.cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.signing_key.serialize_der())),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ = acceptor.accept(tcp).await;
        });
        let tcp = TcpStream::connect(address).await.unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            Trust::platform().handshake("localhost", tcp),
        )
        .await
        .unwrap()
        .expect_err("no root vouches for a self-signed certificate");
        assert_eq!(error.kind(), ErrorKind::Tls);
    }
}
