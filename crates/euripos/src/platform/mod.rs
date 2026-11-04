//! What the platform this build targets can do.
//!
//! Platform code lives in one module per platform behind `cfg(target_os)`;
//! the rest of the crate branches on the capabilities reported here.

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub(crate) use android::platform_verifier;

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
