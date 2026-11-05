//! Android.

/// rustls-platform-verifier on Android needs the application's JVM context,
/// and panics while verifying a certificate when it has none. Nothing
/// initialises it until the Android integration does, so it is reported
/// unavailable and an `https` request fails with `ErrorKind::Tls` instead.
// TODO(#12): report it available once the Android integration initialises it.
pub(crate) fn platform_verifier() -> Result<(), &'static str> {
    Err("the android platform verifier is not initialised")
}

#[cfg(test)]
mod tests {
    #[test]
    fn platform_verifier_is_unavailable_until_initialised() {
        assert!(super::platform_verifier().is_err());
    }
}
