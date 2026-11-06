//! Which proxy a request goes through, and the credentials it presents
//! there: the caller's [`Proxy`], or the platform's configuration, which on
//! Linux is the environment.
//!
//! See docs/design/proxy.md.

use std::ffi::OsString;
use std::fmt;
use std::net::IpAddr;

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
fn invalid(reason: &'static str) -> Error {
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
    /// The proxy a request to `host`, over TLS when `tls`, goes through, or
    /// `None` when it goes to the origin directly.
    pub(crate) fn hop(&self, tls: bool, host: &str) -> Result<Option<Hop>, Error> {
        match self {
            Self::Fixed(proxy) => Ok(Some(proxy.hop.clone())),
            Self::System if platform::PROXY_FROM_ENVIRONMENT => {
                from_environment(tls, host, |name| std::env::var_os(name))
            }
            Self::System | Self::Direct => Ok(None),
        }
    }
}

/// The proxy the environment names for a request to `host`, over TLS when
/// `tls`, as curl reads it: `https_proxy` or `http_proxy` by the request's
/// scheme, then `all_proxy`, each lowercase before uppercase, unless
/// `no_proxy` names the host. `HTTP_PROXY` is ignored when `REQUEST_METHOD`
/// is set, where a CGI request's `Proxy` header could have set it
/// (CVE-2016-5385). A value without a scheme is an `http` proxy.
fn from_environment(
    tls: bool,
    host: &str,
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<Option<Hop>, Error> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    if let Some(no_proxy) = set("no_proxy").or_else(|| set("NO_PROXY"))
        && excluded(&no_proxy.to_string_lossy(), host)
    {
        return Ok(None);
    }
    let value = if tls {
        set("https_proxy").or_else(|| set("HTTPS_PROXY"))
    } else {
        set("http_proxy").or_else(|| {
            if var("REQUEST_METHOD").is_some() {
                None
            } else {
                set("HTTP_PROXY")
            }
        })
    }
    .or_else(|| set("all_proxy"))
    .or_else(|| set("ALL_PROXY"));
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| invalid("the proxy in the environment is not UTF-8"))?;
    let value = value.trim();
    if value.contains("://") {
        Hop::parse(value).map(Some)
    } else {
        Hop::parse(&format!("http://{value}")).map(Some)
    }
}

/// Whether `no_proxy`, a comma-separated list, names `host`: `*` names
/// every host; a name names itself and every host under it, with or without
/// a leading dot; an IP address names itself, and `ADDRESS/BITS` the
/// addresses in that range.
fn excluded(no_proxy: &str, host: &str) -> bool {
    let address = host.parse::<IpAddr>().ok();
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            let entry = entry
                .strip_prefix('[')
                .and_then(|entry| entry.strip_suffix(']'))
                .unwrap_or(entry);
            if let Some(address) = address {
                return in_range(entry, address);
            }
            let entry = entry.strip_prefix('.').unwrap_or(entry);
            host.eq_ignore_ascii_case(entry)
                || (host.len() > entry.len()
                    && host.as_bytes()[host.len() - entry.len() - 1] == b'.'
                    && host[host.len() - entry.len()..].eq_ignore_ascii_case(entry))
        })
}

/// Whether `address` is the address `entry` names, or in the range
/// `ADDRESS/BITS` names.
fn in_range(entry: &str, address: IpAddr) -> bool {
    let (network, bits) = match entry.split_once('/') {
        Some((network, bits)) => match bits.parse::<u32>() {
            Ok(bits) => (network, bits),
            Err(_) => return false,
        },
        None => (entry, u32::MAX),
    };
    match (network.parse::<IpAddr>(), address) {
        (Ok(IpAddr::V4(network)), IpAddr::V4(address)) => {
            prefix_matches(network.to_bits().into(), address.to_bits().into(), bits, 32)
        }
        (Ok(IpAddr::V6(network)), IpAddr::V6(address)) => {
            prefix_matches(network.to_bits(), address.to_bits(), bits, 128)
        }
        _ => false,
    }
}

/// Whether the first `bits` of `network` and `address`, of `width` bits
/// each, are equal; all of them when `bits` exceeds `width`.
fn prefix_matches(network: u128, address: u128, bits: u32, width: u32) -> bool {
    let bits = bits.min(width);
    let shift = width - bits;
    if shift >= 128 {
        return true;
    }
    network >> shift == address >> shift
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
    use super::{Hop, Proxy, Setting, base64, excluded, from_environment, percent_decode};
    use crate::ErrorKind;
    use std::ffi::OsString;

    /// An environment holding only `vars`.
    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| {
            vars.iter()
                .find(|(set, _)| set == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// The route through the proxy at `host:port` with no credentials, as
    /// `Setting::hop` returns it.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "compared with `Setting::hop`'s result"
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
    fn the_environment_picks_the_variable_for_the_scheme_then_all_proxy() {
        let vars = env(&[
            ("http_proxy", "http://plain:1"),
            ("HTTPS_PROXY", "secure:2"),
            ("all_proxy", "http://every:3"),
        ]);
        assert_eq!(
            from_environment(false, "example.com", &vars).expect("ok"),
            hop("plain", 1)
        );
        assert_eq!(
            from_environment(true, "example.com", &vars).expect("ok"),
            hop("secure", 2)
        );
        let vars = env(&[("ALL_PROXY", "every:3"), ("https_proxy", "")]);
        assert_eq!(
            from_environment(true, "example.com", &vars).expect("ok"),
            hop("every", 3)
        );
        assert_eq!(
            from_environment(true, "example.com", env(&[])).expect("ok"),
            None
        );
    }

    #[test]
    fn lowercase_comes_before_uppercase() {
        let vars = env(&[("https_proxy", "lower:1"), ("HTTPS_PROXY", "upper:2")]);
        assert_eq!(
            from_environment(true, "example.com", vars).expect("ok"),
            hop("lower", 1)
        );
    }

    #[test]
    fn uppercase_http_proxy_is_ignored_in_a_cgi_request() {
        let vars = env(&[("HTTP_PROXY", "injected:1"), ("REQUEST_METHOD", "GET")]);
        assert_eq!(
            from_environment(false, "example.com", vars).expect("ok"),
            None
        );
        let vars = env(&[("http_proxy", "set:1"), ("REQUEST_METHOD", "GET")]);
        assert_eq!(
            from_environment(false, "example.com", vars).expect("ok"),
            hop("set", 1)
        );
    }

    #[test]
    fn no_proxy_sends_the_hosts_it_names_directly() {
        let vars = env(&[
            ("http_proxy", "proxy:1"),
            ("NO_PROXY", "internal.example, 10.0.0.0/8"),
        ]);
        assert_eq!(
            from_environment(false, "api.internal.example", &vars).expect("ok"),
            None
        );
        assert_eq!(
            from_environment(false, "10.1.2.3", &vars).expect("ok"),
            None
        );
        assert_eq!(
            from_environment(false, "example.com", &vars).expect("ok"),
            hop("proxy", 1)
        );
    }

    #[test]
    fn an_unusable_environment_proxy_fails_rather_than_going_direct() {
        let vars = env(&[("https_proxy", "socks5://proxy:1080")]);
        let error = from_environment(true, "example.com", vars).expect_err("socks");
        assert_eq!(error.kind(), ErrorKind::Connect);
    }

    #[test]
    fn no_proxy_entries_match_names_suffixes_addresses_and_ranges() {
        assert!(excluded("*", "example.com"));
        assert!(excluded("example.com", "example.com"));
        assert!(excluded("example.com", "www.Example.COM"));
        assert!(excluded(".example.com", "example.com"));
        assert!(excluded(".example.com", "a.example.com"));
        assert!(!excluded("example.com", "badexample.com"));
        assert!(!excluded("www.example.com", "example.com"));
        assert!(excluded("localhost,127.0.0.1", "127.0.0.1"));
        assert!(!excluded("127.0.0.1", "127.0.0.2"));
        assert!(excluded("192.168.0.0/16", "192.168.4.5"));
        assert!(!excluded("192.168.0.0/16", "192.169.0.1"));
        assert!(excluded("0.0.0.0/0", "8.8.8.8"));
        assert!(excluded("[::1]", "::1"));
        assert!(excluded("2001:db8::/32", "2001:db8:1::1"));
        assert!(!excluded("2001:db8::/32", "2001:db9::1"));
        assert!(!excluded("10.0.0.0/8", "::1"));
        assert!(!excluded("10.0.0.0/x", "10.0.0.1"));
        assert!(!excluded(" , ", "example.com"));
    }

    #[test]
    fn direct_and_fixed_settings_ignore_the_platform() {
        assert_eq!(Setting::Direct.hop(true, "example.com").expect("ok"), None);
        let proxy = Proxy::new("http://proxy.example:3128").expect("proxy");
        assert_eq!(
            Setting::Fixed(proxy).hop(false, "example.com").expect("ok"),
            hop("proxy.example", 3128)
        );
    }
}
