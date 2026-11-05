//! What the platform this build targets can do.
//!
//! Platform code lives in one module per platform behind `cfg(target_os)`;
//! the rest of the crate branches on the capabilities reported here.

#[cfg(target_os = "android")]
mod android;
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "android")]
pub(crate) use android::{bind_to_network, network_source, platform_verifier, query_https};
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) use apple::{Stream, connect, network_source, query_https};
#[cfg(target_os = "linux")]
pub(crate) use linux::{network_source, query_https};
#[cfg(target_os = "windows")]
pub(crate) use windows::network_source;

/// Query `name`'s HTTPS records through the platform's resolver
/// ([`crate::dns`]) on `network`, where the platform can put a query to a
/// given network, and on the network it is on otherwise. Where the platform
/// has no way to query them, there is no answer, and the origin's records are
/// `Unknown`.
// TODO(#57): `DnsQueryEx` on Windows (docs/adr/0002-windows-dns.md).
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
#[allow(
    clippy::unused_async,
    reason = "the signature is every platform's; the others query asynchronously"
)]
pub(crate) async fn query_https(
    _network: Option<crate::NetworkId>,
    _name: &str,
) -> Option<crate::dns::Answer> {
    None
}

/// Whether this platform reports the network it is on through
/// [`network_source`].
pub(crate) const OBSERVES_NETWORK: bool = cfg!(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios",
    target_os = "android"
));

/// The platform's network source, opened on the monitor's runtime. `None`
/// where the platform reports nothing about the network, or the source could
/// not be opened.
#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios",
    target_os = "android"
)))]
#[allow(
    clippy::unused_async,
    reason = "the signature is every platform's; Linux opens its source asynchronously"
)]
pub(crate) async fn network_source() -> Option<Unobservable> {
    None
}

/// The source of a platform that reports nothing about the network.
#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios",
    target_os = "android"
)))]
pub(crate) enum Unobservable {}

#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios",
    target_os = "android"
)))]
impl crate::monitor::Source for Unobservable {
    fn observe(&mut self) -> crate::NetworkFacts {
        match *self {}
    }

    fn changed(&mut self) -> impl Future<Output = ()> {
        std::future::pending()
    }
}

/// The byte stream of a connection the platform opened: a socket, or on
/// macOS and iOS a Network.framework connection.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub(crate) type Stream = tokio::net::TcpStream;

/// Open a TCP connection to `host` (a name or an IP address) on `port`.
///
/// With sockets, the host is resolved by the system resolver and each
/// address it returns is tried in order until one accepts, each socket bound
/// first to the network the platform names ([`bind_to_network`]). A failure
/// is [`ErrorKind::Connect`](crate::ErrorKind::Connect).
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub(crate) async fn connect(host: &str, port: u16) -> Result<Stream, crate::Error> {
    use crate::{Error, ErrorKind};

    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| Error::with_source(ErrorKind::Connect, error))?;
    let mut last = None;
    for address in addresses {
        match connect_to(address).await {
            Ok(tcp) => return Ok(tcp),
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(error) => Error::with_source(ErrorKind::Connect, error),
        None => Error::with_source(ErrorKind::Connect, "the host resolved to no address"),
    })
}

/// Connect a socket to `address`, bound to the platform's network first.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
async fn connect_to(address: std::net::SocketAddr) -> std::io::Result<Stream> {
    let socket = if address.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    bind_to_network(&socket)?;
    socket.connect(address).await
}

/// Bind `socket`, before it connects, to the network the platform reports
/// traffic leaves through. Only Android binds: elsewhere a socket takes the
/// route the kernel picks when it connects.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature is every platform's; Android's binding can fail"
)]
fn bind_to_network(_socket: &tokio::net::TcpSocket) -> std::io::Result<()> {
    Ok(())
}

/// Whether rustls-platform-verifier can verify certificates in this process.
///
/// `Err` holds why it cannot, as an error message. A caller treats it as a
/// TLS failure rather than calling into the verifier.
#[cfg(not(target_os = "android"))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature is every platform's; Android reports an error"
)]
pub(crate) fn platform_verifier() -> Result<(), &'static str> {
    Ok(())
}
