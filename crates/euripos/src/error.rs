//! The one error type, classified by what went wrong rather than by the
//! crate underneath that noticed it.

use std::error::Error as StdError;
use std::fmt;

/// A boxed underlying cause, reachable through [`Error::source`].
type Source = Box<dyn StdError + Send + Sync + 'static>;

/// An error from sending a request or reading its response.
///
/// Branch on [`Error::kind`]; the underlying cause, when there is one, is
/// reachable through [`std::error::Error::source`] and is not part of the
/// stable API.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    source: Option<Source>,
}

impl Error {
    /// An error of `kind` with no underlying cause.
    pub(crate) fn new(kind: ErrorKind) -> Self {
        Self { kind, source: None }
    }

    /// An error of `kind` caused by `source`.
    pub(crate) fn with_source(kind: ErrorKind, source: impl Into<Source>) -> Self {
        Self {
            kind,
            source: Some(source.into()),
        }
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.kind, f)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

/// The class of an [`Error`].
///
/// New kinds may be added in a minor release, so a `match` needs a wildcard
/// arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// No connection to the server could be established: resolution failed,
    /// the server refused it, or no route reached it.
    Connect,
    /// The TLS handshake failed, or the server's certificate was not trusted.
    Tls,
    /// One of the client's timeouts elapsed; the value says which.
    Timeout(TimeoutKind),
    /// The server's response violated HTTP, or the connection broke in the
    /// middle of an exchange.
    Protocol,
    /// The network the request was using went away or was replaced while the
    /// request was in flight.
    NetworkChanged,
    /// The request was cancelled before it completed.
    Cancelled,
    /// The request body could not be produced: the source a
    /// [`Body::wrap`](crate::Body::wrap) body streams from failed, or a
    /// value could not be serialized into one.
    Body,
    /// The response body could not be decoded into what the caller asked
    /// for: it was not UTF-8 text or the expected JSON, or its content
    /// coding was corrupt.
    Decode,
    /// The server redirected the request more times than the client
    /// follows, set by
    /// [`ClientBuilder::max_redirects`](crate::ClientBuilder::max_redirects).
    Redirect,
    /// The proxy refused the request for want of valid credentials: it
    /// answered `407 Proxy Authentication Required` (RFC 9110 §15.5.8) to
    /// the request or to the `CONNECT` for its tunnel. The client does not
    /// retry with other credentials; set them with
    /// [`Proxy::basic_auth`](crate::Proxy::basic_auth) or in the proxy's URI.
    ProxyAuthRequired,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect => f.write_str("connection failed"),
            Self::Tls => f.write_str("tls handshake failed"),
            Self::Timeout(kind) => write!(f, "{kind} timed out"),
            Self::Protocol => f.write_str("protocol error"),
            Self::NetworkChanged => f.write_str("network changed during the request"),
            Self::Cancelled => f.write_str("request cancelled"),
            Self::Body => f.write_str("request body failed"),
            Self::Decode => f.write_str("response body could not be decoded"),
            Self::Redirect => f.write_str("too many redirects"),
            Self::ProxyAuthRequired => f.write_str("proxy authentication required"),
        }
    }
}

/// Which of the client's timeouts elapsed.
///
/// Each matches one setter on [`ClientBuilder`](crate::ClientBuilder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TimeoutKind {
    /// Establishing the transport connection, set by
    /// [`ClientBuilder::connect_timeout`](crate::ClientBuilder::connect_timeout).
    Connect,
    /// The TLS handshake, set by
    /// [`ClientBuilder::tls_timeout`](crate::ClientBuilder::tls_timeout).
    Tls,
    /// The whole request, from sending it to the end of the response body,
    /// set by [`ClientBuilder::total_timeout`](crate::ClientBuilder::total_timeout).
    Total,
    /// The wait for the next piece of the response while none arrives, set
    /// by [`ClientBuilder::read_stall_timeout`](crate::ClientBuilder::read_stall_timeout).
    ReadStall,
}

impl fmt::Display for TimeoutKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Connect => "connect",
            Self::Tls => "tls handshake",
            Self::Total => "request",
            Self::ReadStall => "read",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind, TimeoutKind};
    use std::error::Error as _;

    #[test]
    fn message_names_the_kind_and_the_timeout() {
        let error = Error::new(ErrorKind::Timeout(TimeoutKind::ReadStall));
        assert_eq!(error.to_string(), "read timed out");
        assert_eq!(error.kind(), ErrorKind::Timeout(TimeoutKind::ReadStall));
    }

    #[test]
    fn body_kinds_say_which_side_failed() {
        assert_eq!(
            Error::new(ErrorKind::Body).to_string(),
            "request body failed"
        );
        assert_eq!(
            Error::new(ErrorKind::Decode).to_string(),
            "response body could not be decoded"
        );
    }

    #[test]
    fn a_proxy_refusing_credentials_is_its_own_kind() {
        let error = Error::new(ErrorKind::ProxyAuthRequired);
        assert_eq!(error.to_string(), "proxy authentication required");
        assert_ne!(error.kind(), ErrorKind::Connect);
    }

    #[test]
    fn cause_is_reachable_through_source_and_not_repeated() {
        let cause = std::io::Error::other("connection reset");
        let error = Error::with_source(ErrorKind::Protocol, cause);
        assert_eq!(error.to_string(), "protocol error");
        let source = error.source().map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("connection reset"));
    }

    #[test]
    fn error_is_send_sync_and_static() {
        fn assert_bounds<T: std::error::Error + Send + Sync + 'static>() {}
        assert_bounds::<Error>();
    }
}
