//! The proxy, as `ConnectivityManager.getDefaultProxy()` reports it for the
//! default network: the configuration the facts report, and the proxy a
//! connection to one host goes through.

use crate::ProxyConfig;

/// A `ProxyInfo`, as the monitor's Java reports it. Empty where the network
/// has no proxy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Settings {
    /// The proxy host: the configured one, or `localhost` for a PAC script,
    /// which the platform runs behind a proxy of its own.
    pub(super) host: Option<String>,
    /// The proxy port; not a port (`-1`) where none is set, as for a PAC
    /// script whose local proxy has not started.
    pub(super) port: i32,
    /// Hosts reached directly despite a fixed proxy, each a pattern in which
    /// `*` matches any run of characters.
    pub(super) exclusions: Vec<String>,
    /// The PAC script's URL.
    pub(super) pac: Option<String>,
}

/// Where a connection to one host should go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hop {
    /// Straight to the origin.
    Direct,
    /// Through the proxy at this `host:port`.
    Proxy(String),
}

impl Settings {
    /// Settings from the strings the monitor's Java passes: the exclusions
    /// joined by commas, which no host name contains.
    pub(super) fn new(
        host: Option<String>,
        port: i32,
        exclusions: Option<&str>,
        pac: Option<String>,
    ) -> Self {
        Self {
            host: host.filter(|host| !host.is_empty()),
            port,
            exclusions: exclusions
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|pattern| !pattern.is_empty())
                .map(str::to_owned)
                .collect(),
            pac: pac.filter(|pac| !pac.is_empty()),
        }
    }

    /// The configuration these settings describe.
    pub(super) fn config(&self) -> ProxyConfig {
        if self.pac.is_some() {
            ProxyConfig::Automatic
        } else if self.host.is_some() {
            ProxyConfig::Manual
        } else {
            ProxyConfig::Direct
        }
    }

    /// Where a connection to `host` goes, as the platform's own HTTP stack
    /// sends it: a PAC script is evaluated per request by the platform's
    /// local PAC proxy, so every request goes there, and direct until that
    /// proxy has a port; a fixed proxy carries every host its exclusions do
    /// not match.
    pub(super) fn hop(&self, host: &str) -> Hop {
        let Some(proxy) = &self.host else {
            return Hop::Direct;
        };
        let Ok(port) = u16::try_from(self.port) else {
            return Hop::Direct;
        };
        if port == 0
            || (self.pac.is_none() && self.exclusions.iter().any(|pattern| matches(pattern, host)))
        {
            return Hop::Direct;
        }
        if proxy.contains(':') {
            Hop::Proxy(format!("[{proxy}]:{port}"))
        } else {
            Hop::Proxy(format!("{proxy}:{port}"))
        }
    }
}

/// Whether `host` matches `pattern` whole, where `*` matches any run of
/// characters, ignoring ASCII case as host names do.
fn matches(pattern: &str, host: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::{Hop, Settings, matches};
    use crate::ProxyConfig;

    #[test]
    fn settings_map_to_the_configuration_they_describe() {
        assert_eq!(Settings::default().config(), ProxyConfig::Direct);
        let manual = Settings::new(Some("proxy.example".into()), 3128, None, None);
        assert_eq!(manual.config(), ProxyConfig::Manual);
        let pac = Settings::new(
            Some("localhost".into()),
            -1,
            Some(""),
            Some("http://wpad.example/proxy.pac".into()),
        );
        assert_eq!(pac.config(), ProxyConfig::Automatic);
        // `ProxyInfo` reports an absent host or script as empty.
        let empty = Settings::new(Some(String::new()), -1, Some(""), Some(String::new()));
        assert_eq!(empty, Settings::default().with_port(-1));
        assert_eq!(empty.config(), ProxyConfig::Direct);
    }

    #[test]
    fn a_fixed_proxy_carries_every_host_but_its_exclusions() {
        let settings = Settings::new(
            Some("proxy.example".into()),
            3128,
            Some("localhost, *.internal.example,10.*"),
            None,
        );
        assert_eq!(settings.exclusions.len(), 3);
        let proxied = Hop::Proxy("proxy.example:3128".into());
        assert_eq!(settings.hop("example.com"), proxied);
        assert_eq!(settings.hop("internal.example"), proxied);
        assert_eq!(settings.hop("localhost"), Hop::Direct);
        assert_eq!(settings.hop("api.internal.example"), Hop::Direct);
        assert_eq!(settings.hop("API.Internal.Example"), Hop::Direct);
        assert_eq!(settings.hop("10.0.0.1"), Hop::Direct);
    }

    #[test]
    fn a_pac_script_sends_every_host_to_the_local_pac_proxy_once_it_listens() {
        let starting = Settings::new(
            Some("localhost".into()),
            -1,
            Some("example.com"),
            Some("http://wpad.example/proxy.pac".into()),
        );
        assert_eq!(starting.hop("example.com"), Hop::Direct);
        let listening = starting.with_port(41234);
        // The script, not the exclusions, decides; the local proxy runs it.
        assert_eq!(
            listening.hop("example.com"),
            Hop::Proxy("localhost:41234".into())
        );
    }

    #[test]
    fn an_ipv6_proxy_is_bracketed_and_no_port_is_direct() {
        let settings = Settings::new(Some("2001:db8::1".into()), 8080, None, None);
        assert_eq!(
            settings.hop("example.com"),
            Hop::Proxy("[2001:db8::1]:8080".into())
        );
        assert_eq!(
            settings.clone().with_port(0).hop("example.com"),
            Hop::Direct
        );
        assert_eq!(settings.with_port(70_000).hop("example.com"), Hop::Direct);
    }

    #[test]
    fn patterns_match_whole_hosts_with_stars_anywhere() {
        assert!(matches("example.com", "example.com"));
        assert!(!matches("example.com", "www.example.com"));
        assert!(!matches("example.com", "example.co"));
        assert!(matches("*.example.com", "a.b.example.com"));
        assert!(!matches("*.example.com", "example.com"));
        assert!(matches("*", "anything"));
        assert!(matches("192.168.*.1", "192.168.10.1"));
        assert!(!matches("192.168.*.1", "192.168.10.2"));
        assert!(matches("*a*b*", "xxaxxbxx"));
        assert!(!matches("*a*b*", "xxbxxaxx"));
    }

    impl Settings {
        fn with_port(mut self, port: i32) -> Self {
            self.port = port;
            self
        }
    }
}
