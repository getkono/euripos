//! The network from Network.framework's path monitor: the path the system
//! would send a new connection over, with its interface, gateways and the
//! expensive and constrained flags.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use block2::{RcBlock, StackBlock};
use tokio::sync::Notify;

use super::{Owned, ffi, queue};
use crate::monitor::Source;
use crate::{
    Constraint, InterfaceKind, IpAvailability, Metering, NetworkFacts, NetworkId, ProxyConfig,
};

/// How long opening the source waits for the monitor's first path, so the
/// first reading names the network. Within the monitor's 500 ms startup
/// wait, as on Linux.
const PATH_STARTUP: Duration = Duration::from_millis(250);

/// Open the Apple network source: start a path monitor, and wait up to
/// [`PATH_STARTUP`] for its first path. A slower one arrives as a change.
pub(crate) async fn network_source() -> Option<AppleSource> {
    let shared = Arc::new(Shared::default());
    // SAFETY: returns a new monitor, owned from here on.
    let monitor = Owned::new(unsafe { ffi::nw_path_monitor_create() })?;
    let handler = {
        let shared = shared.clone();
        RcBlock::new(move |path: ffi::nw_path_t| {
            // SAFETY: the monitor passes a live path for the call.
            let reading = unsafe { Reading::of(path) };
            *shared
                .reading
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(reading);
            shared.notify.notify_one();
        })
    };
    // SAFETY: `monitor` is live; the handler is copied by the call, and the
    // queue lives as long as the process.
    unsafe {
        ffi::nw_path_monitor_set_queue(monitor.0, queue());
        ffi::nw_path_monitor_set_update_handler(monitor.0, &handler);
        ffi::nw_path_monitor_start(monitor.0);
    }
    let _ = tokio::time::timeout(PATH_STARTUP, shared.notify.notified()).await;
    Some(AppleSource { monitor, shared })
}

/// The Apple network source.
pub(crate) struct AppleSource {
    monitor: Owned,
    shared: Arc<Shared>,
}

/// What the monitor's handler hands to the source.
#[derive(Default)]
struct Shared {
    /// The last path the monitor reported; `None` before the first.
    reading: Mutex<Option<Reading>>,
    notify: Notify,
}

impl Source for AppleSource {
    fn observe(&mut self) -> NetworkFacts {
        self.shared
            .reading
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or(NetworkFacts::UNKNOWN, Reading::facts)
    }

    async fn changed(&mut self) {
        self.shared.notify.notified().await;
    }
}

impl Drop for AppleSource {
    fn drop(&mut self) {
        // SAFETY: the monitor is live until `self.monitor` drops after this.
        unsafe { ffi::nw_path_monitor_cancel(self.monitor.0) }
    }
}

/// A path, copied out of its `nw_path_t` in the monitor's handler.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is one of the path's own flags, read by a call of its own"
)]
struct Reading {
    /// `nw_path_status_t`.
    status: u32,
    /// The path's first interface, the one traffic leaves through.
    interface: Option<Interface>,
    /// The gateways of the path's interfaces.
    gateways: Vec<IpAddr>,
    expensive: bool,
    constrained: bool,
    /// `None` where the system has no ultra-constrained mode.
    ultra_constrained: Option<bool>,
    ipv4: bool,
    ipv6: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Interface {
    index: u32,
    /// `nw_interface_type_t`.
    kind: u32,
}

impl Reading {
    /// The reading `path` holds.
    ///
    /// # Safety
    ///
    /// `path` is a live `nw_path_t`.
    unsafe fn of(path: ffi::nw_path_t) -> Self {
        let first = Cell::new(None);
        let interfaces = StackBlock::new(|interface: ffi::nw_interface_t| {
            // SAFETY: the interface is live for the call.
            first.set(Some(unsafe {
                Interface {
                    index: ffi::nw_interface_get_index(interface),
                    kind: ffi::nw_interface_get_type(interface),
                }
            }));
            // The first is the one traffic leaves through; stop there.
            0
        });
        let gateways = RefCell::new(Vec::new());
        let enumerate_gateways = StackBlock::new(|gateway: ffi::nw_endpoint_t| {
            // SAFETY: the endpoint is live for the call; an address
            // endpoint's address is valid while the endpoint is.
            if let Some(address) = unsafe { gateway_address(gateway) } {
                gateways.borrow_mut().push(address);
            }
            1
        });
        // SAFETY: `path` is live, per the contract; each block is called
        // only during the call it is passed to.
        unsafe {
            ffi::nw_path_enumerate_interfaces(path, &interfaces);
            ffi::nw_path_enumerate_gateways(path, &enumerate_gateways);
            Self {
                status: ffi::nw_path_get_status(path),
                interface: first.get(),
                gateways: gateways.into_inner(),
                expensive: ffi::nw_path_is_expensive(path),
                constrained: ffi::nw_path_is_constrained(path),
                ultra_constrained: is_ultra_constrained().map(|is| is(path)),
                ipv4: ffi::nw_path_has_ipv4(path),
                ipv6: ffi::nw_path_has_ipv6(path),
            }
        }
    }

    /// The facts this reading reports.
    fn facts(&self) -> NetworkFacts {
        let availability = |has| {
            if has {
                IpAvailability::Available
            } else {
                IpAvailability::Unavailable
            }
        };
        let mut facts = NetworkFacts {
            ipv4: availability(self.ipv4),
            ipv6: availability(self.ipv6),
            // The path carries no proxy configuration.
            proxy: ProxyConfig::Unknown,
            ..NetworkFacts::UNKNOWN
        };
        // Unsatisfied, or satisfiable only once something (VPN On Demand)
        // brings an interface up: no network, and its flags mean nothing.
        if self.status != ffi::NW_PATH_STATUS_SATISFIED {
            return facts;
        }
        facts.network = Some(NetworkId::from_key(&self.key()));
        facts.interface = self
            .interface
            .map_or(InterfaceKind::Unknown, |interface| kind(interface.kind));
        facts.metering = if self.expensive {
            Metering::Metered
        } else {
            Metering::Unmetered
        };
        facts.constraint = if self.ultra_constrained == Some(true) {
            Constraint::UltraConstrained
        } else if self.constrained {
            Constraint::Constrained
        } else {
            Constraint::Unconstrained
        };
        facts
    }

    /// What identifies the network: the interface traffic leaves through
    /// and its gateways, in an order of their own.
    fn key(&self) -> (Option<u32>, Vec<IpAddr>) {
        let mut gateways = self.gateways.clone();
        gateways.sort_unstable();
        gateways.dedup();
        (self.interface.map(|interface| interface.index), gateways)
    }
}

/// The kind of interface `nw_interface_type_t` `kind` names.
fn kind(kind: u32) -> InterfaceKind {
    match kind {
        ffi::NW_INTERFACE_TYPE_WIFI => InterfaceKind::Wifi,
        ffi::NW_INTERFACE_TYPE_CELLULAR => InterfaceKind::Cellular,
        ffi::NW_INTERFACE_TYPE_WIRED => InterfaceKind::Ethernet,
        // Loopback, and other: tunnels (utun), bridges.
        _ => InterfaceKind::Other,
    }
}

/// The IP address of `endpoint`, where it is an address endpoint.
///
/// # Safety
///
/// `endpoint` is a live `nw_endpoint_t`.
unsafe fn gateway_address(endpoint: ffi::nw_endpoint_t) -> Option<IpAddr> {
    // SAFETY: `endpoint` is live, per the contract.
    unsafe {
        if ffi::nw_endpoint_get_type(endpoint) != ffi::NW_ENDPOINT_TYPE_ADDRESS {
            return None;
        }
        ip_address(ffi::nw_endpoint_get_address(endpoint))
    }
}

/// The IP address in `address`, where it is an IPv4 or IPv6 one.
///
/// # Safety
///
/// `address` is null, or points to a socket address as long as its family
/// says.
unsafe fn ip_address(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    // SAFETY: a non-null `address` holds at least its family, and as many
    // bytes as the family's structure, per the contract; each is read
    // unaligned.
    unsafe {
        match i32::from(address.read_unaligned().sa_family) {
            libc::AF_INET => {
                let address = address.cast::<libc::sockaddr_in>().read_unaligned();
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    address.sin_addr.s_addr,
                ))))
            }
            libc::AF_INET6 => {
                let address = address.cast::<libc::sockaddr_in6>().read_unaligned();
                Some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

/// `nw_path_is_ultra_constrained`, which exists from macOS and iOS 26, looked
/// up at run time so the library still loads on earlier releases. `None`
/// where the system predates it, and so has no ultra-constrained mode.
fn is_ultra_constrained() -> Option<unsafe extern "C" fn(ffi::nw_path_t) -> bool> {
    static FUNCTION: OnceLock<Option<unsafe extern "C" fn(ffi::nw_path_t) -> bool>> =
        OnceLock::new();
    *FUNCTION.get_or_init(|| {
        // SAFETY: looks a symbol up by a NUL-terminated name.
        let symbol: *mut c_void =
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"nw_path_is_ultra_constrained".as_ptr()) };
        // SAFETY: the symbol, where it exists, is the function declared in
        // Network.framework's `path.h` as `bool (nw_path_t)`.
        (!symbol.is_null()).then(|| unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(ffi::nw_path_t) -> bool>(symbol)
        })
    })
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::{Interface, Reading, ffi, ip_address, network_source};
    use crate::monitor::Source;
    use crate::{Constraint, InterfaceKind, IpAvailability, Metering, ProxyConfig};

    /// A satisfied path over Wi-Fi interface 4 with one IPv4 gateway.
    fn wifi() -> Reading {
        Reading {
            status: ffi::NW_PATH_STATUS_SATISFIED,
            interface: Some(Interface {
                index: 4,
                kind: ffi::NW_INTERFACE_TYPE_WIFI,
            }),
            gateways: vec!["192.0.2.1".parse().unwrap()],
            expensive: false,
            constrained: false,
            ultra_constrained: None,
            ipv4: true,
            ipv6: false,
        }
    }

    #[test]
    fn a_satisfied_path_reports_its_interface_flags_and_ip_versions() {
        let facts = wifi().facts();
        assert!(facts.network.is_some());
        assert_eq!(facts.interface, InterfaceKind::Wifi);
        assert_eq!(facts.metering, Metering::Unmetered);
        assert_eq!(facts.constraint, Constraint::Unconstrained);
        assert_eq!(facts.ipv4, IpAvailability::Available);
        assert_eq!(facts.ipv6, IpAvailability::Unavailable);
        assert_eq!(facts.proxy, ProxyConfig::Unknown);

        let cellular = Reading {
            interface: Some(Interface {
                index: 9,
                kind: ffi::NW_INTERFACE_TYPE_CELLULAR,
            }),
            expensive: true,
            constrained: true,
            ..wifi()
        }
        .facts();
        assert_eq!(cellular.interface, InterfaceKind::Cellular);
        assert_eq!(cellular.metering, Metering::Metered);
        assert_eq!(cellular.constraint, Constraint::Constrained);

        let wired = Reading {
            interface: Some(Interface {
                index: 5,
                kind: ffi::NW_INTERFACE_TYPE_WIRED,
            }),
            ..wifi()
        };
        assert_eq!(wired.facts().interface, InterfaceKind::Ethernet);
        let tunnel = Reading {
            interface: Some(Interface { index: 12, kind: 0 }),
            ..wifi()
        };
        assert_eq!(tunnel.facts().interface, InterfaceKind::Other);
    }

    #[test]
    fn ultra_constrained_is_reported_where_the_system_has_it() {
        let ultra = Reading {
            constrained: true,
            ultra_constrained: Some(true),
            ..wifi()
        };
        assert_eq!(ultra.facts().constraint, Constraint::UltraConstrained);
        let constrained = Reading {
            constrained: true,
            ultra_constrained: Some(false),
            ..wifi()
        };
        assert_eq!(constrained.facts().constraint, Constraint::Constrained);
    }

    // The iOS simulator's path is satisfied and reports neither version:
    // the system has not said which one routes, so neither is unavailable.
    #[test]
    fn a_satisfied_path_with_no_ip_version_leaves_both_unknown() {
        let facts = Reading {
            ipv4: false,
            ipv6: false,
            ..wifi()
        }
        .facts();
        assert!(facts.network.is_some());
        assert_eq!(facts.ipv4, IpAvailability::Unknown);
        assert_eq!(facts.ipv6, IpAvailability::Unknown);
        // One version is enough for the other's absence to be known.
        let v6_only = Reading {
            ipv4: false,
            ipv6: true,
            ..wifi()
        }
        .facts();
        assert_eq!(v6_only.ipv4, IpAvailability::Unavailable);
        assert_eq!(v6_only.ipv6, IpAvailability::Available);
    }

    #[test]
    fn an_unsatisfied_path_names_no_network_and_no_flags() {
        for status in [0, 2, 3] {
            let facts = Reading {
                status,
                interface: None,
                gateways: Vec::new(),
                expensive: true,
                ipv4: false,
                ..wifi()
            }
            .facts();
            assert_eq!(facts.network, None);
            assert_eq!(facts.interface, InterfaceKind::Unknown);
            assert_eq!(facts.metering, Metering::Unknown);
            assert_eq!(facts.constraint, Constraint::Unknown);
            assert_eq!(facts.ipv4, IpAvailability::Unavailable);
        }
    }

    #[test]
    fn the_network_is_the_interface_and_its_gateways() {
        let home = wifi().facts().network;
        // The same gateways, reported in another order or twice.
        let reordered = Reading {
            gateways: vec![
                "fe80::1".parse().unwrap(),
                "192.0.2.1".parse().unwrap(),
                "fe80::1".parse().unwrap(),
            ],
            ..wifi()
        };
        let with_v6 = Reading {
            gateways: vec!["192.0.2.1".parse().unwrap(), "fe80::1".parse().unwrap()],
            ..wifi()
        };
        assert_eq!(reordered.facts().network, with_v6.facts().network);
        // Another Wi-Fi network on the same interface, or another interface.
        let other_wifi = Reading {
            gateways: vec!["198.51.100.1".parse().unwrap()],
            ..wifi()
        };
        assert_ne!(other_wifi.facts().network, home);
        let other_interface = Reading {
            interface: Some(Interface {
                index: 5,
                kind: ffi::NW_INTERFACE_TYPE_WIFI,
            }),
            ..wifi()
        };
        assert_ne!(other_interface.facts().network, home);
        // A flag changing is not a new network.
        let expensive = Reading {
            expensive: true,
            ..wifi()
        };
        assert_eq!(expensive.facts().network, home);
    }

    #[test]
    fn socket_addresses_parse_by_family() {
        let v4: IpAddr = "192.0.2.7".parse().unwrap();
        let v6: IpAddr = "2001:db8::7".parse().unwrap();
        for address in [v4, v6] {
            let storage = socket2::SockAddr::from(std::net::SocketAddr::new(address, 443));
            // SAFETY: `storage` holds a socket address of its family.
            assert_eq!(
                unsafe { ip_address(storage.as_ptr().cast()) },
                Some(address)
            );
        }
        // SAFETY: null is accepted.
        assert_eq!(unsafe { ip_address(std::ptr::null()) }, None);
    }

    // The host's own path: the hosted macOS runners and the iOS simulator
    // are on a network with IPv4, which the monitor names.
    #[test]
    fn the_host_network_is_named_and_its_flags_known() {
        std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut source = network_source().await.unwrap();
                let facts = source.observe();
                assert!(facts.network.is_some(), "{facts:?}");
                assert_eq!(facts.ipv4, IpAvailability::Available, "{facts:?}");
                assert_ne!(facts.interface, InterfaceKind::Unknown);
                assert_ne!(facts.metering, Metering::Unknown);
                assert_ne!(facts.constraint, Constraint::Unknown);
                // Two reads of an unchanged network name it the same.
                assert_eq!(source.observe().network, facts.network);
            });
        })
        .join()
        .unwrap();
    }
}
