//! What the platform this build targets can do.
//!
//! Platform code lives in one module per platform behind `cfg(target_os)`;
//! the rest of the crate branches on the capabilities reported here.

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "android")]
pub(crate) use android::platform_verifier;
#[cfg(target_os = "linux")]
pub(crate) use linux::network_source;

/// Whether this platform reports the network it is on through
/// [`network_source`].
pub(crate) const OBSERVES_NETWORK: bool = cfg!(target_os = "linux");

/// The platform's network source, opened on the monitor's runtime. `None`
/// where the platform reports nothing about the network, or the source could
/// not be opened.
#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::unused_async,
    reason = "the signature is every platform's; Linux opens its source asynchronously"
)]
pub(crate) async fn network_source() -> Option<Unobservable> {
    // TODO(#11): Apple; TODO(#12): Android; TODO(#13): Windows.
    None
}

/// The source of a platform that reports nothing about the network.
#[cfg(not(target_os = "linux"))]
pub(crate) enum Unobservable {}

#[cfg(not(target_os = "linux"))]
impl crate::monitor::Source for Unobservable {
    fn observe(&mut self) -> crate::NetworkFacts {
        match *self {}
    }

    fn changed(&mut self) -> impl Future<Output = ()> {
        std::future::pending()
    }
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
