//! rtnetlink: the route the kernel picks to the internet, the link it leaves
//! through, and the signals that either may have changed.
//!
//! The messages are encoded and parsed here rather than through a netlink
//! message crate: the monitor sends two kinds of request and reads four
//! fields, and the layouts are the kernel's stable ABI (`linux/netlink.h`,
//! `linux/rtnetlink.h`, `linux/if_link.h`).

use std::io;
use std::net::IpAddr;

use netlink_sys::protocols::NETLINK_ROUTE;
use netlink_sys::{AsyncSocket, AsyncSocketExt, Socket, SocketAddr, TokioSocket};

const NLMSG_HDRLEN: usize = 16;
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 1;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;

/// `struct rtmsg`, `struct ifinfomsg` and `struct ifaddrmsg` lengths.
const RTMSG_LEN: usize = 12;
const IFINFOMSG_LEN: usize = 16;
const IFADDRMSG_LEN: usize = 8;

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PREFSRC: u16 = 7;
const RTA_TABLE: u16 = 15;
const IFLA_IFNAME: u16 = 3;
const IFLA_WIRELESS: u16 = 11;

const RTN_UNICAST: u8 = 1;
const RTN_LOCAL: u8 = 2;
const RTN_BROADCAST: u8 = 3;
const RTN_ANYCAST: u8 = 4;
const RTN_MULTICAST: u8 = 5;
const RTM_F_CLONED: u32 = 0x200;
const RT_TABLE_LOCAL: u32 = 255;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

/// `RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV4_ROUTE |
/// RTMGRP_IPV6_IFADDR | RTMGRP_IPV6_ROUTE`.
const GROUPS: u32 = 0x1 | 0x10 | 0x40 | 0x100 | 0x400;

/// The route the kernel picks to a destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Route {
    /// The index of the interface traffic leaves through.
    pub(super) interface: u32,
    /// The next hop, where the destination is not on the link.
    pub(super) gateway: Option<IpAddr>,
    /// The source address the kernel would use.
    pub(super) source: Option<IpAddr>,
}

/// A network interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Link {
    /// Its name, as in `/sys/class/net`.
    pub(super) name: String,
    /// Its `ARPHRD_*` hardware type.
    pub(super) hardware: u16,
}

/// A netlink socket for requests to the kernel.
pub(super) struct Kernel {
    socket: Socket,
    sequence: u32,
}

impl Kernel {
    pub(super) fn open() -> io::Result<Self> {
        let mut socket = Socket::new(NETLINK_ROUTE)?;
        socket.bind_auto()?;
        Ok(Self {
            socket,
            sequence: 0,
        })
    }

    /// The route the kernel picks to `destination` now, as `ip route get`
    /// reports it, after policy rules. No packet is sent. `None` when the
    /// destination is unreachable.
    pub(super) fn route_to(&mut self, destination: IpAddr) -> io::Result<Option<Route>> {
        let reply = self.request(RTM_GETROUTE, &route_request(destination))?;
        Ok(match reply {
            Reply::Message(RTM_NEWROUTE, payload) => parse_route(&payload),
            Reply::Message(..) | Reply::Error => None,
        })
    }

    /// The interface with index `index`, if it exists.
    pub(super) fn link(&mut self, index: u32) -> io::Result<Option<Link>> {
        let reply = self.request(RTM_GETLINK, &link_request(index))?;
        Ok(match reply {
            Reply::Message(RTM_NEWLINK, payload) => parse_link(&payload),
            Reply::Message(..) | Reply::Error => None,
        })
    }

    /// Send one request and read the kernel's reply to it.
    fn request(&mut self, kind: u16, body: &[u8]) -> io::Result<Reply> {
        self.sequence = self.sequence.wrapping_add(1);
        let sequence = self.sequence;
        let message = encode(kind, NLM_F_REQUEST, sequence, body);
        self.socket.send_to(&message, &SocketAddr::new(0, 0), 0)?;
        loop {
            let (datagram, from) = self.socket.recv_from_full()?;
            if from.port_number() != 0 {
                continue;
            }
            for message in messages(&datagram) {
                if message.sequence != sequence {
                    continue;
                }
                if message.kind == NLMSG_ERROR {
                    return Ok(Reply::Error);
                }
                return Ok(Reply::Message(message.kind, message.payload.to_vec()));
            }
        }
    }
}

/// The kernel's reply to a request.
enum Reply {
    /// A message of this type, with its payload.
    Message(u16, Vec<u8>),
    /// An error: for a route, the destination is unreachable; for a link,
    /// there is no such link.
    Error,
}

/// A netlink socket subscribed to link, address and route changes.
pub(super) struct Signals {
    socket: TokioSocket,
}

impl Signals {
    /// Subscribe. Must be called on a tokio runtime with I/O enabled.
    pub(super) fn open() -> io::Result<Self> {
        let mut socket = TokioSocket::new(NETLINK_ROUTE)?;
        socket.socket_mut().bind(&SocketAddr::new(0, GROUPS))?;
        Ok(Self { socket })
    }

    /// Resolves at the next change that may move the route to the internet.
    ///
    /// A lost notification (`ENOBUFS`, when changes arrive faster than they
    /// are read) is a signal too: what was lost is unknown. Cancel-safe.
    pub(super) async fn next(&mut self) {
        loop {
            match self.socket.recv_from_full().await {
                Ok((datagram, from)) => {
                    if from.port_number() == 0 && is_signal(&datagram) {
                        return;
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => return,
                // The socket is broken; the facts stay as last read.
                Err(_) => std::future::pending::<()>().await,
            }
        }
    }
}

/// Whether a notification may change the route to the internet or the link
/// it leaves through.
///
/// Not: wireless events on a link (scan results, which arrive in floods),
/// host- and link-scoped addresses, cloned (cached) routes, and local,
/// broadcast, anycast and multicast routes or anything in the local table.
pub(super) fn is_signal(datagram: &[u8]) -> bool {
    messages(datagram).any(|message| match message.kind {
        RTM_NEWLINK | RTM_DELLINK => {
            message
                .payload
                .get(IFINFOMSG_LEN..)
                .is_some_and(|attributes| {
                    !attributes_of(attributes).any(|(kind, _)| kind == IFLA_WIRELESS)
                })
        }
        RTM_NEWADDR | RTM_DELADDR => message
            .payload
            .get(..IFADDRMSG_LEN)
            .is_some_and(|header| !matches!(header[3], RT_SCOPE_LINK | RT_SCOPE_HOST)),
        RTM_NEWROUTE | RTM_DELROUTE => route_is_signal(message.payload),
        _ => false,
    })
}

fn route_is_signal(payload: &[u8]) -> bool {
    let (Some(header), Some(attributes)) = (payload.get(..RTMSG_LEN), payload.get(RTMSG_LEN..))
    else {
        return false;
    };
    let flags = u32::from_ne_bytes([header[8], header[9], header[10], header[11]]);
    let table = attributes_of(attributes)
        .find(|&(kind, _)| kind == RTA_TABLE)
        .and_then(|(_, value)| value.first_chunk::<4>())
        .map_or(u32::from(header[4]), |bytes| u32::from_ne_bytes(*bytes));
    flags & RTM_F_CLONED == 0
        && table != RT_TABLE_LOCAL
        && !matches!(
            header[7],
            RTN_LOCAL | RTN_BROADCAST | RTN_ANYCAST | RTN_MULTICAST
        )
}

/// The body of an `RTM_GETROUTE` request for `destination`.
fn route_request(destination: IpAddr) -> Vec<u8> {
    let (family, address) = match destination {
        IpAddr::V4(address) => (AF_INET, address.octets().to_vec()),
        IpAddr::V6(address) => (AF_INET6, address.octets().to_vec()),
    };
    // Prefix length in bits: 32 or 128.
    let bits = u8::try_from(address.len() * 8).unwrap_or(u8::MAX);
    let mut body = vec![family, bits, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    push_attribute(&mut body, RTA_DST, &address);
    body
}

/// The body of an `RTM_GETLINK` request for the interface `index`.
fn link_request(index: u32) -> Vec<u8> {
    let mut body = vec![0; IFINFOMSG_LEN];
    body[4..8].copy_from_slice(&index.to_ne_bytes());
    body
}

/// The route in an `RTM_NEWROUTE` payload, if it is a unicast route out of
/// an interface.
fn parse_route(payload: &[u8]) -> Option<Route> {
    let header = payload.get(..RTMSG_LEN)?;
    if header[7] != RTN_UNICAST {
        return None;
    }
    let mut route = Route {
        interface: 0,
        gateway: None,
        source: None,
    };
    for (kind, value) in attributes_of(&payload[RTMSG_LEN..]) {
        match kind {
            RTA_OIF => route.interface = u32::from_ne_bytes(*value.first_chunk::<4>()?),
            RTA_GATEWAY => route.gateway = address(value),
            RTA_PREFSRC => route.source = address(value),
            _ => {}
        }
    }
    (route.interface != 0).then_some(route)
}

/// The link in an `RTM_NEWLINK` payload.
fn parse_link(payload: &[u8]) -> Option<Link> {
    let header = payload.get(..IFINFOMSG_LEN)?;
    let hardware = u16::from_ne_bytes([header[2], header[3]]);
    let name = attributes_of(&payload[IFINFOMSG_LEN..])
        .find(|&(kind, _)| kind == IFLA_IFNAME)
        .map(|(_, value)| value.split(|&byte| byte == 0).next().unwrap_or_default())?;
    Some(Link {
        name: String::from_utf8(name.to_vec()).ok()?,
        hardware,
    })
}

fn address(value: &[u8]) -> Option<IpAddr> {
    if let Ok(octets) = <[u8; 4]>::try_from(value) {
        Some(IpAddr::from(octets))
    } else {
        <[u8; 16]>::try_from(value).ok().map(IpAddr::from)
    }
}

/// One netlink message in a datagram.
struct Message<'a> {
    kind: u16,
    sequence: u32,
    payload: &'a [u8],
}

/// The messages in a datagram, stopping at the first malformed one.
fn messages(mut datagram: &[u8]) -> impl Iterator<Item = Message<'_>> {
    std::iter::from_fn(move || {
        let header = datagram.get(..NLMSG_HDRLEN)?;
        let length = usize::try_from(u32::from_ne_bytes([
            header[0], header[1], header[2], header[3],
        ]))
        .ok()?;
        if length < NLMSG_HDRLEN || length > datagram.len() {
            return None;
        }
        let message = Message {
            kind: u16::from_ne_bytes([header[4], header[5]]),
            sequence: u32::from_ne_bytes([header[8], header[9], header[10], header[11]]),
            payload: &datagram[NLMSG_HDRLEN..length],
        };
        datagram = datagram.get(align(length)..).unwrap_or_default();
        Some(message)
    })
}

/// The `(type, value)` attributes in `attributes`, stopping at the first
/// malformed one.
fn attributes_of(mut attributes: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        let header = attributes.get(..4)?;
        let length = usize::from(u16::from_ne_bytes([header[0], header[1]]));
        if length < 4 || length > attributes.len() {
            return None;
        }
        // The top two bits are the nested and byte-order flags.
        let kind = u16::from_ne_bytes([header[2], header[3]]) & 0x3fff;
        let value = &attributes[4..length];
        attributes = attributes.get(align(length)..).unwrap_or_default();
        Some((kind, value))
    })
}

/// A netlink message: header, then `body`.
fn encode(kind: u16, flags: u16, sequence: u32, body: &[u8]) -> Vec<u8> {
    let length = u32::try_from(NLMSG_HDRLEN + body.len()).unwrap_or(u32::MAX);
    let mut message = Vec::with_capacity(NLMSG_HDRLEN + body.len());
    message.extend_from_slice(&length.to_ne_bytes());
    message.extend_from_slice(&kind.to_ne_bytes());
    message.extend_from_slice(&flags.to_ne_bytes());
    message.extend_from_slice(&sequence.to_ne_bytes());
    message.extend_from_slice(&0_u32.to_ne_bytes());
    message.extend_from_slice(body);
    message
}

fn push_attribute(body: &mut Vec<u8>, kind: u16, value: &[u8]) {
    let length = u16::try_from(4 + value.len()).unwrap_or(u16::MAX);
    body.extend_from_slice(&length.to_ne_bytes());
    body.extend_from_slice(&kind.to_ne_bytes());
    body.extend_from_slice(value);
    body.resize(align(body.len()), 0);
}

/// Netlink aligns messages and attributes to 4 bytes.
const fn align(length: usize) -> usize {
    (length + 3) & !3
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::{
        AF_INET, IFINFOMSG_LEN, IFLA_IFNAME, IFLA_WIRELESS, Kernel, Link, RTA_GATEWAY, RTA_OIF,
        RTA_PREFSRC, RTA_TABLE, RTM_DELROUTE, RTM_F_CLONED, RTM_NEWADDR, RTM_NEWLINK, RTM_NEWROUTE,
        RTN_UNICAST, Route, encode, is_signal, messages, parse_link, parse_route, push_attribute,
        route_request,
    };

    fn rtmsg(family: u8, table: u8, kind: u8, flags: u32) -> Vec<u8> {
        let mut header = vec![family, 0, 0, 0, table, 0, 0, kind];
        header.extend_from_slice(&flags.to_ne_bytes());
        header
    }

    fn route_message(table: u8, kind: u8, flags: u32) -> Vec<u8> {
        encode(RTM_NEWROUTE, 0, 0, &rtmsg(AF_INET, table, kind, flags))
    }

    fn link_message(attributes: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = vec![0; IFINFOMSG_LEN];
        for &(kind, value) in attributes {
            push_attribute(&mut body, kind, value);
        }
        encode(RTM_NEWLINK, 0, 0, &body)
    }

    #[test]
    fn a_main_table_unicast_route_change_is_a_signal() {
        assert!(is_signal(&route_message(254, RTN_UNICAST, 0)));
        let mut body = rtmsg(AF_INET, 254, RTN_UNICAST, 0);
        body[4] = 252;
        push_attribute(&mut body, RTA_TABLE, &1000_u32.to_ne_bytes());
        assert!(is_signal(&encode(RTM_DELROUTE, 0, 0, &body)));
    }

    // The Linux shape of a route-miss flood: cached routes the kernel clones
    // per destination, and local, broadcast and multicast routes.
    #[test]
    fn cloned_local_and_multicast_routes_are_not_signals() {
        assert!(!is_signal(&route_message(254, RTN_UNICAST, RTM_F_CLONED)));
        assert!(!is_signal(&route_message(255, RTN_UNICAST, 0)));
        assert!(!is_signal(&route_message(254, 2, 0)));
        assert!(!is_signal(&route_message(254, 3, 0)));
        assert!(!is_signal(&route_message(254, 5, 0)));
        let mut body = rtmsg(AF_INET, 252, RTN_UNICAST, 0);
        push_attribute(&mut body, RTA_TABLE, &255_u32.to_ne_bytes());
        assert!(!is_signal(&encode(RTM_NEWROUTE, 0, 0, &body)));
    }

    #[test]
    fn a_flood_of_cloned_routes_in_one_datagram_is_not_a_signal() {
        let flood: Vec<u8> = (0..1_000)
            .flat_map(|_| route_message(254, RTN_UNICAST, RTM_F_CLONED))
            .collect();
        assert_eq!(messages(&flood).count(), 1_000);
        assert!(!is_signal(&flood));
    }

    #[test]
    fn wireless_link_events_are_not_signals_and_other_link_events_are() {
        assert!(!is_signal(&link_message(&[(IFLA_WIRELESS, &[0; 8])])));
        assert!(is_signal(&link_message(&[(IFLA_IFNAME, b"wlan0\0")])));
    }

    #[test]
    fn only_global_address_changes_are_signals() {
        let global = encode(RTM_NEWADDR, 0, 0, &[AF_INET, 24, 0, 0, 2, 0, 0, 0]);
        let link = encode(RTM_NEWADDR, 0, 0, &[AF_INET, 24, 0, 253, 2, 0, 0, 0]);
        let host = encode(RTM_NEWADDR, 0, 0, &[AF_INET, 8, 0, 254, 1, 0, 0, 0]);
        assert!(is_signal(&global));
        assert!(!is_signal(&link));
        assert!(!is_signal(&host));
    }

    #[test]
    fn malformed_datagrams_are_not_signals() {
        assert!(!is_signal(&[]));
        assert!(!is_signal(&[0xff; 7]));
        let mut truncated = route_message(254, RTN_UNICAST, 0);
        truncated[0] = 200;
        assert!(!is_signal(&truncated));
    }

    #[test]
    fn a_route_reply_is_parsed() {
        let mut body = rtmsg(AF_INET, 254, RTN_UNICAST, 0);
        push_attribute(&mut body, RTA_OIF, &3_u32.to_ne_bytes());
        push_attribute(&mut body, RTA_GATEWAY, &[192, 0, 2, 1]);
        push_attribute(&mut body, RTA_PREFSRC, &[192, 0, 2, 7]);
        assert_eq!(
            parse_route(&body),
            Some(Route {
                interface: 3,
                gateway: Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
                source: Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))),
            })
        );
    }

    #[test]
    fn an_unreachable_route_reply_is_no_route() {
        let mut body = rtmsg(AF_INET, 254, 7, 0);
        push_attribute(&mut body, RTA_OIF, &1_u32.to_ne_bytes());
        assert_eq!(parse_route(&body), None);
    }

    #[test]
    fn a_link_reply_is_parsed() {
        let mut body = vec![0; IFINFOMSG_LEN];
        body[2..4].copy_from_slice(&1_u16.to_ne_bytes());
        push_attribute(&mut body, IFLA_IFNAME, b"eth0\0");
        assert_eq!(
            parse_link(&body),
            Some(Link {
                name: "eth0".into(),
                hardware: 1,
            })
        );
    }

    #[test]
    fn a_route_request_carries_the_destination() {
        let request = route_request(IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(request[..2], [10, 128]);
        assert_eq!(request[12..14], 20_u16.to_ne_bytes());
        assert_eq!(request[16..32], Ipv6Addr::LOCALHOST.octets());
    }

    // The kernel itself: loopback is always routed, out of `lo`.
    #[test]
    fn the_kernel_routes_loopback_through_lo() {
        let mut kernel = Kernel::open().unwrap();
        let route = kernel.route_to(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        // 127.0.0.1 is a local route, which is not a route out of an
        // interface.
        assert_eq!(route, None);
        let lo = kernel.link(1).unwrap().unwrap();
        assert_eq!(lo.name, "lo");
        assert_eq!(kernel.link(u32::MAX).unwrap(), None);
    }
}
