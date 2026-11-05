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
pub(crate) use android::platform_verifier;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) use apple::{Stream, connect, network_source};
#[cfg(target_os = "linux")]
pub(crate) use linux::network_source;
#[cfg(target_os = "windows")]
pub(crate) use windows::network_source;

/// Whether this platform reports the network it is on through
/// [`network_source`].
pub(crate) const OBSERVES_NETWORK: bool = cfg!(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
));

/// The platform's network source, opened on the monitor's runtime. `None`
/// where the platform reports nothing about the network, or the source could
/// not be opened.
#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
)))]
#[allow(
    clippy::unused_async,
    reason = "the signature is every platform's; Linux opens its source asynchronously"
)]
pub(crate) async fn network_source() -> Option<Unobservable> {
    // TODO(#12): Android.
    None
}

/// The source of a platform that reports nothing about the network.
#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
)))]
pub(crate) enum Unobservable {}

#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
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
/// address it returns is tried in order until one accepts. A failure is
/// [`ErrorKind::Connect`](crate::ErrorKind::Connect).
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub(crate) async fn connect(host: &str, port: u16) -> Result<Stream, crate::Error> {
    use crate::{Error, ErrorKind};

    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| Error::with_source(ErrorKind::Connect, error))?;
    let mut last = None;
    for address in addresses {
        match Stream::connect(address).await {
            Ok(tcp) => return Ok(tcp),
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(error) => Error::with_source(ErrorKind::Connect, error),
        None => Error::with_source(ErrorKind::Connect, "the host resolved to no address"),
    })
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
