//! TLS: rustls with the platform verifier, offering HTTP/2 and HTTP/1.1 by
//! ALPN.
//!
//! See docs/baseline.md, "TLS and protocol negotiation".

use std::sync::{Arc, Mutex, PoisonError};

use rustls::ClientConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::{Error, ErrorKind, platform};

/// The ALPN protocol IDs offered, most preferred first.
const ALPN: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// Where a client's TLS configuration comes from.
#[derive(Clone)]
pub(crate) enum Trust {
    /// The platform verifier, built on first use and kept once built.
    Platform(Arc<Mutex<Option<Arc<ClientConfig>>>>),
    /// A fixed configuration; tests use it to trust their own certificate.
    #[cfg(test)]
    Fixed(Arc<ClientConfig>),
}

impl Trust {
    pub(crate) fn platform() -> Self {
        Self::Platform(Arc::default())
    }

    /// The configuration to handshake with.
    fn config(&self) -> Result<Arc<ClientConfig>, Error> {
        match self {
            Self::Platform(cached) => {
                let mut cached = cached.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(config) = &*cached {
                    return Ok(config.clone());
                }
                // A failure is not kept: the application may install a
                // provider, or the platform's trust store may become readable,
                // before the next request.
                let config = Arc::new(platform_config()?);
                *cached = Some(config.clone());
                Ok(config)
            }
            #[cfg(test)]
            Self::Fixed(config) => Ok(config.clone()),
        }
    }

    /// Runs the TLS handshake for `host` over `tcp`.
    pub(crate) async fn handshake<IO>(&self, host: &str, tcp: IO) -> Result<TlsStream<IO>, Error>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let name = ServerName::try_from(host.to_owned())
            .map_err(|error| Error::with_source(ErrorKind::Tls, error))?;
        let connector = TlsConnector::from(self.config()?);
        connector
            .connect(name, tcp)
            .await
            .map_err(|error| Error::with_source(ErrorKind::Tls, error))
    }
}

/// The crypto provider the client uses: ring, which `crypto-ring` selects,
/// whatever the process installed as its default.
///
/// See docs/adr/0001-crypto-provider.md, "Choice".
#[cfg(feature = "crypto-ring")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "a build without a provider feature can fail here"
)]
fn provider() -> Result<Arc<CryptoProvider>, Error> {
    Ok(Arc::new(rustls::crypto::ring::default_provider()))
}

/// The crypto provider the client uses: with no `crypto-*` feature, the
/// process-wide default the application installed, as
/// docs/adr/0001-crypto-provider.md specifies for a build without one.
#[cfg(not(feature = "crypto-ring"))]
fn provider() -> Result<Arc<CryptoProvider>, Error> {
    CryptoProvider::get_default().cloned().ok_or_else(|| {
        Error::with_source(
            ErrorKind::Tls,
            "no rustls crypto provider is installed for this process",
        )
    })
}

/// A configuration that verifies servers with the platform verifier and offers
/// [`ALPN`].
fn platform_config() -> Result<ClientConfig, Error> {
    platform_config_where(platform::platform_verifier())
}

/// [`platform_config`], where `verifier` is what the platform reports about
/// its verifier. One reported unavailable fails as [`ErrorKind::Tls`] without
/// being called, because on Android it would panic.
fn platform_config_where(verifier: Result<(), &'static str>) -> Result<ClientConfig, Error> {
    verifier.map_err(|reason| Error::with_source(ErrorKind::Tls, reason))?;
    let provider = provider()?;
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
        .map_err(|error| Error::with_source(ErrorKind::Tls, error))?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::with_source(ErrorKind::Tls, error))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(with_alpn(config))
}

/// `config` offering [`ALPN`].
pub(crate) fn with_alpn(mut config: ClientConfig) -> ClientConfig {
    config.alpn_protocols = ALPN.iter().map(|id| id.to_vec()).collect();
    config
}

/// Whether the handshake on `stream` agreed on HTTP/2.
pub(crate) fn negotiated_h2<IO>(stream: &TlsStream<IO>) -> bool {
    stream.get_ref().1.alpn_protocol() == Some(b"h2")
}

#[cfg(test)]
mod tests {
    use super::{ALPN, platform_config_where, with_alpn};
    use crate::ErrorKind;
    use std::sync::Arc;

    #[test]
    fn an_unavailable_platform_verifier_fails_as_tls() {
        // With a provider installed, only the verifier can fail the build.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let error = platform_config_where(Err("not initialised"))
            .expect_err("an unavailable verifier builds no configuration");
        assert_eq!(error.kind(), ErrorKind::Tls);
        let source = std::error::Error::source(&error).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("not initialised"));
    }

    #[test]
    fn alpn_offers_h2_before_http1() {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default versions")
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let config = with_alpn(config);
        assert_eq!(config.alpn_protocols, ALPN.map(<[u8]>::to_vec));
        assert_eq!(config.alpn_protocols[0], b"h2");
    }
}
