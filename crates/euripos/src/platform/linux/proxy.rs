//! The proxy from the environment: `http_proxy`, `https_proxy`,
//! `all_proxy` and `no_proxy`, read for each request (docs/design/proxy.md).

use std::ffi::OsString;
use std::net::IpAddr;

use crate::Error;
use crate::proxy::{Hop, invalid, proxy_value};

/// The proxy the environment names for a request to `host`, over TLS when
/// `tls`, as curl reads it: `https_proxy` or `http_proxy` by the request's
/// scheme, then `all_proxy`, each lowercase before uppercase, unless
/// `no_proxy` names the host. `HTTP_PROXY` is ignored when `REQUEST_METHOD`
/// is set, where a CGI request's `Proxy` header could have set it
/// (CVE-2016-5385). A value without a scheme is an `http` proxy.
pub(super) fn from_environment(
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
    proxy_value(&value).map(Some)
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

#[cfg(test)]
mod tests {
    use super::{excluded, from_environment};
    use crate::ErrorKind;
    use crate::proxy::Hop;
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
    /// `from_environment` returns it.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "compared with `from_environment`'s result"
    )]
    fn hop(host: &str, port: u16) -> Option<Hop> {
        Some(Hop {
            host: host.into(),
            port,
            auth: None,
        })
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
}
