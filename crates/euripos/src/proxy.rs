//! Which proxy a request goes through, and the credentials it presents
//! there: the caller's [`Proxy`], or the routes the platform's configuration
//! gives it ([`platform::proxy_routes`]), tried in order.
//!
//! See docs/design/proxy.md.

use std::fmt;

use http::uri::Scheme;
use http::{HeaderValue, Uri};

use crate::{Error, ErrorKind, platform};

/// An HTTP proxy that requests go through, and the credentials the client
/// presents to it.
///
/// Set one with [`ClientBuilder::proxy`](crate::ClientBuilder::proxy). An
/// `http` request is sent to the proxy whole, its target the absolute URI
/// (RFC 9112 §3.2.2); an `https` request goes through a tunnel the proxy
/// opens with `CONNECT` (RFC 9110 §9.3.6), so the proxy sees the origin's
/// host and port but not the request.
///
/// Credentials are sent as `Proxy-Authorization: Basic` (RFC 7617) on every
/// proxied request and on every `CONNECT`. A proxy that refuses them, or
/// asks for credentials when none were given, fails the request as
/// [`ErrorKind::ProxyAuthRequired`](crate::ErrorKind::ProxyAuthRequired).
#[derive(Clone)]
pub struct Proxy {
    hop: Hop,
}

impl Proxy {
    /// The proxy `uri` names: `http://[user:password@]host[:port]`.
    ///
    /// The port defaults to 80. User information in the URI, percent-decoded,
    /// becomes the proxy's Basic credentials; [`basic_auth`](Self::basic_auth)
    /// replaces them.
    ///
    /// # Errors
    ///
    /// Fails as [`ErrorKind::Connect`](crate::ErrorKind::Connect), as a
    /// request URI with no origin does, when `uri` is not an `http` URI with a
    /// host. A proxy reached over TLS (`https`) or SOCKS is not supported.
    pub fn new(uri: &str) -> Result<Self, Error> {
        Ok(Self {
            hop: Hop::parse(uri)?,
        })
    }

    /// Present `username` and `password` to the proxy as Basic credentials,
    /// in place of any the URI held.
    ///
    /// They are encoded as UTF-8 (RFC 7617 §2.1). A `username` containing
    /// `:` cannot be told apart from the password by the proxy.
    #[must_use]
    pub fn basic_auth(mut self, username: &str, password: &str) -> Self {
        self.hop.auth = Some(basic(username.as_bytes(), password.as_bytes()));
        self
    }
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.hop, f)
    }
}

/// The proxy a connection goes through, and the `Proxy-Authorization` it
/// presents there.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Hop {
    /// A name or an IP address, without IPv6 brackets.
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) auth: Option<HeaderValue>,
}

impl Hop {
    /// The proxy an `http` URI names, with the credentials its user
    /// information holds.
    fn parse(uri: &str) -> Result<Self, Error> {
        let uri: Uri = uri
            .parse()
            .map_err(|error| Error::with_source(ErrorKind::Connect, error))?;
        if uri.scheme() != Some(&Scheme::HTTP) {
            return Err(invalid("the proxy URI is not an http URI"));
        }
        let authority = uri
            .authority()
            .ok_or_else(|| invalid("the proxy URI has no host"))?;
        let host = authority.host();
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        if host.is_empty() {
            return Err(invalid("the proxy URI has no host"));
        }
        let auth = authority.as_str().rsplit_once('@').map(|(userinfo, _)| {
            let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            basic(&percent_decode(user), &percent_decode(password))
        });
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port: authority.port_u16().unwrap_or(80),
            auth,
        })
    }

    /// `host:port` as a URI authority: an IPv6 address in brackets.
    pub(crate) fn authority(host: &str, port: u16) -> String {
        if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        }
    }
}

impl fmt::Debug for Hop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The credentials are never printed.
        f.debug_struct("Proxy")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("basic_auth", &self.auth.is_some())
            .finish()
    }
}

/// A proxy URI that names no `http` proxy.
pub(crate) fn invalid(reason: &'static str) -> Error {
    Error::with_source(ErrorKind::Connect, reason)
}

/// How a client picks the proxy for each request.
#[derive(Debug, Clone, Default)]
pub(crate) enum Setting {
    /// The platform's configuration, read for each request.
    #[default]
    System,
    /// No proxy: every request goes to its origin.
    Direct,
    /// Every request through this proxy.
    Fixed(Proxy),
}

impl Setting {
    /// The routes a request to `host` on `port`, over TLS when `tls`, may
    /// take, in the order to try them: through a proxy, or `None` for the
    /// origin directly. Never empty.
    ///
    /// The platform's configuration is asked when the request is sent, so a
    /// change to it applies from the next request.
    pub(crate) async fn routes(
        &self,
        tls: bool,
        host: &str,
        port: u16,
    ) -> Result<Vec<Option<Hop>>, Error> {
        match self {
            Self::Fixed(proxy) => Ok(vec![Some(proxy.hop.clone())]),
            Self::Direct => Ok(vec![None]),
            Self::System => platform::proxy_routes(tls, host, port).await,
        }
    }
}

/// The routes a platform's proxy list names, in its order: each entry a
/// proxy's `host[:port]` (or an `http` URI), or `None` for the origin
/// directly.
///
/// An entry that names no `http` proxy (a SOCKS proxy, or one reached over
/// TLS) is skipped. A list of nothing but such entries fails as
/// [`ErrorKind::Connect`], as an unusable proxy in the environment does,
/// rather than sending the request around the proxy configured; an empty
/// list is the origin directly.
#[cfg_attr(
    not(any(test, target_os = "android", target_os = "windows")),
    expect(dead_code, reason = "the other platforms' sources are wired next")
)]
pub(crate) fn platform_routes<'a>(
    entries: impl IntoIterator<Item = Option<&'a str>>,
) -> Result<Vec<Option<Hop>>, Error> {
    let mut routes = Vec::new();
    let mut skipped = None;
    for entry in entries {
        match entry.map(proxy_value).transpose() {
            Ok(route) => routes.push(route),
            Err(error) => skipped = Some(error),
        }
    }
    match skipped {
        Some(error) if routes.is_empty() => Err(error),
        _ if routes.is_empty() => Ok(vec![None]),
        _ => Ok(routes),
    }
}

/// Whether `host` matches `pattern` whole, where `*` matches any run of
/// characters, ignoring ASCII case as host names do: the form of the
/// bypass lists Android and Windows keep.
#[cfg_attr(
    not(any(test, target_os = "android", target_os = "windows")),
    expect(dead_code, reason = "only the Android and Windows bypass lists use it")
)]
pub(crate) fn wildcard(pattern: &str, host: &str) -> bool {
    let pattern = pattern.as_bytes();
    let host = host.as_bytes();
    let (mut p, mut h) = (0, 0);
    // The last `*` seen, and the host position it is tried from next.
    let mut star = None;
    while h < host.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, h));
            p += 1;
        } else if p < pattern.len() && pattern[p].eq_ignore_ascii_case(&host[h]) {
            p += 1;
            h += 1;
        } else if let Some((star_p, star_h)) = star {
            p = star_p + 1;
            h = star_h + 1;
            star = Some((star_p, h));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

/// The proxy a configured value names: an `http` URI, or `host[:port]`,
/// which is an `http` proxy.
#[cfg_attr(
    not(any(
        test,
        target_os = "linux",
        target_os = "android",
        target_os = "windows"
    )),
    expect(dead_code, reason = "the other platforms' sources are wired next")
)]
pub(crate) fn proxy_value(value: &str) -> Result<Hop, Error> {
    let value = value.trim();
    if value.contains("://") {
        Hop::parse(value)
    } else {
        Hop::parse(&format!("http://{value}"))
    }
}

/// `Basic` credentials for `user` and `password` (RFC 7617 §2), marked
/// sensitive so they are kept out of HPACK tables and debug output.
fn basic(user: &[u8], password: &[u8]) -> HeaderValue {
    let mut credentials = Vec::with_capacity(user.len() + 1 + password.len());
    credentials.extend_from_slice(user);
    credentials.push(b':');
    credentials.extend_from_slice(password);
    let mut value = HeaderValue::from_str(&format!("Basic {}", base64(&credentials)))
        // Invariant: base64 is ASCII, which a header value may hold.
        .expect("base64 is a valid header value");
    value.set_sensitive(true);
    value
}

/// `bytes` in base64 with padding (RFC 4648 §4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0_u32, |group, (i, &byte)| {
            group | u32::from(byte) << (16 - 8 * i)
        });
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(
                    ALPHABET[(group >> (18 - 6 * i) & 0x3f) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `text` with each `%XX` escape replaced by the byte it stands for; an
/// escape that is not two hex digits is kept as it is.
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            // `from_str_radix` alone would accept a leading `+`.
            && hex.iter().all(u8::is_ascii_hexdigit)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Hop, Proxy, Setting, base64, percent_decode, platform_routes, proxy_value, wildcard,
    };
    use crate::ErrorKind;

    /// The route through the proxy at `host:port` with no credentials, as
    /// `Setting::routes` gives it.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "compared with `Setting::routes`'s result"
    )]
    fn hop(host: &str, port: u16) -> Option<Hop> {
        Some(Hop {
            host: host.into(),
            port,
            auth: None,
        })
    }

    #[test]
    fn base64_pads_to_whole_groups() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        // RFC 7617 §2's example.
        assert_eq!(
            base64(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn a_proxy_uri_names_host_port_and_credentials() {
        let proxy = Proxy::new("http://Aladdin:open%20sesame@Proxy.example:3128").expect("proxy");
        assert_eq!(proxy.hop.host, "proxy.example");
        assert_eq!(proxy.hop.port, 3128);
        assert_eq!(
            proxy
                .hop
                .auth
                .as_ref()
                .map(|auth| auth.to_str().expect("ascii")),
            Some("Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==")
        );
        assert!(
            proxy
                .hop
                .auth
                .as_ref()
                .is_some_and(http::HeaderValue::is_sensitive)
        );

        let proxy = Proxy::new("http://[::1]/").expect("proxy");
        assert_eq!(
            (proxy.hop.host.as_str(), proxy.hop.port, proxy.hop.auth),
            ("::1", 80, None)
        );
    }

    #[test]
    fn basic_auth_replaces_the_uri_credentials() {
        let proxy = Proxy::new("http://a:b@proxy.example")
            .expect("proxy")
            .basic_auth("Aladdin", "open sesame");
        assert_eq!(
            proxy
                .hop
                .auth
                .as_ref()
                .map(|auth| auth.to_str().expect("ascii")),
            Some("Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==")
        );
    }

    #[test]
    fn debug_never_prints_the_credentials() {
        let proxy = Proxy::new("http://user:secret@proxy.example:8080").expect("proxy");
        let debug = format!("{proxy:?}");
        assert!(!debug.contains("secret"), "{debug}");
        assert!(!debug.contains("dXNlcjpzZWNyZXQ"), "{debug}");
        assert!(debug.contains("basic_auth: true"), "{debug}");
    }

    #[test]
    fn a_uri_that_names_no_http_proxy_fails_as_connect() {
        for uri in [
            "https://proxy.example",
            "socks5://proxy.example",
            "proxy.example:8080",
            "/",
            "http://",
        ] {
            let error = Proxy::new(uri).expect_err(uri);
            assert_eq!(error.kind(), ErrorKind::Connect, "{uri}");
        }
    }

    #[test]
    fn percent_escapes_decode_and_malformed_ones_stay() {
        assert_eq!(percent_decode("a%40b%3Ac"), b"a@b:c");
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("%zz%4"), b"%zz%4");
    }

    #[test]
    fn a_sign_is_not_a_hex_digit_in_an_escape() {
        assert_eq!(percent_decode("%+f"), b"%+f");
        assert_eq!(percent_decode("%-f"), b"%-f");
    }

    #[test]
    fn a_configured_value_without_a_scheme_is_an_http_proxy() {
        assert_eq!(
            proxy_value(" proxy.example:3128 ").ok(),
            hop("proxy.example", 3128)
        );
        assert_eq!(proxy_value("[::1]:8080").ok(), hop("::1", 8080));
        assert_eq!(
            proxy_value("http://proxy.example").ok(),
            hop("proxy.example", 80)
        );
        let error = proxy_value("socks5://proxy.example:1080").expect_err("socks");
        assert_eq!(error.kind(), ErrorKind::Connect);
    }

    #[test]
    fn patterns_match_whole_hosts_with_stars_anywhere() {
        assert!(wildcard("example.com", "example.com"));
        assert!(!wildcard("example.com", "www.example.com"));
        assert!(!wildcard("example.com", "example.co"));
        assert!(wildcard("*.example.com", "a.b.example.com"));
        assert!(!wildcard("*.example.com", "example.com"));
        assert!(wildcard("*", "anything"));
        assert!(wildcard("192.168.*.1", "192.168.10.1"));
        assert!(!wildcard("192.168.*.1", "192.168.10.2"));
        assert!(wildcard("*a*b*", "xxaxxbxx"));
        assert!(!wildcard("*a*b*", "xxbxxaxx"));
    }

    #[test]
    fn a_platform_list_keeps_its_order_and_skips_what_is_no_http_proxy() {
        assert_eq!(
            platform_routes([Some("a:1"), None, Some("socks5://b:2"), Some("c:3")]).expect("ok"),
            [hop("a", 1), None, hop("c", 3)]
        );
        assert_eq!(platform_routes([]).expect("ok"), [None]);
        assert_eq!(platform_routes([None]).expect("ok"), [None]);
        let error = platform_routes([Some("https://secure:443")]).expect_err("no http proxy");
        assert_eq!(error.kind(), ErrorKind::Connect);
    }

    #[tokio::test]
    async fn direct_and_fixed_settings_ignore_the_platform() {
        assert_eq!(
            Setting::Direct
                .routes(true, "example.com", 443)
                .await
                .expect("ok"),
            [None]
        );
        let proxy = Proxy::new("http://proxy.example:3128").expect("proxy");
        assert_eq!(
            Setting::Fixed(proxy)
                .routes(false, "example.com", 80)
                .await
                .expect("ok"),
            [hop("proxy.example", 3128)]
        );
    }

    // Whatever the host is configured with, the platform gives at least one
    // route, the origin directly where it has nothing to say, or an error.
    #[tokio::test]
    async fn the_platform_never_gives_no_route() {
        match Setting::System.routes(true, "example.com", 443).await {
            Ok(routes) => assert_ne!(routes, []),
            Err(error) => assert_eq!(error.kind(), ErrorKind::Connect),
        }
    }
}
