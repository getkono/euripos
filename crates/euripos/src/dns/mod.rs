//! HTTPS records (RFC 9460) through each platform's own resolver: the
//! protocols an origin offers, its alternative endpoints and address hints,
//! and its Encrypted Client Hello configuration. See
//! docs/design/https-svcb.md.
//!
//! The platform module answers one query with the records' wire-format
//! RDATA ([`Answer`]); this module parses them, follows aliases, and caches
//! the result per network for its TTL.

// TODO(#15): Happy Eyeballs v3 reads the records when it races connection
// attempts, and HTTP/3 (#18) skips origins whose records exclude it. Until
// then nothing outside the tests calls the resolver.
#![allow(
    dead_code,
    reason = "the consumers are Happy Eyeballs v3 (#15) and HTTP/3 (#18)"
)]

mod cache;
mod https_rr;
pub(crate) mod message;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

pub(crate) use https_rr::ServiceRecord;

use crate::NetworkId;

/// How long one query may take before the lookup gives up and the origin's
/// records are [`HttpsRecords::Unknown`]. A connection does not wait on it:
/// Happy Eyeballs v3 starts on addresses while the query runs.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// How many `AliasMode` records a lookup follows before it gives up (RFC 9460,
/// section 2.4.2: clients limit the chain).
const MAX_ALIASES: usize = 4;

/// How long the absence of records is kept where the platform reports no
/// negative-caching TTL for it.
const DEFAULT_NEGATIVE_TTL: Duration = Duration::from_secs(60);

/// The longest the absence of records is kept (RFC 2308, section 5).
const MAX_NEGATIVE_TTL: Duration = Duration::from_hours(3);

/// A platform resolver's answer to a query for a name's HTTPS records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The records, at least one.
    Records(Vec<Record>),
    /// The name has no HTTPS records, or does not exist. `negative_ttl` is
    /// how long that may be cached, where the platform reports it.
    NoRecords { negative_ttl: Option<u32> },
}

/// One HTTPS record as the platform returned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    /// The wire-format RDATA (RFC 9460, section 2.2).
    pub(crate) rdata: Vec<u8>,
    /// Its TTL, in seconds.
    pub(crate) ttl: u32,
}

/// What an origin's HTTPS records say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpsRecords {
    /// The `ServiceMode` records of `owner`, the name the lookup reached after
    /// any aliases, lowest priority first. A record whose target is `None`
    /// is served at `owner`.
    Services {
        owner: Arc<str>,
        records: Arc<[ServiceRecord]>,
    },
    /// Nothing usable is known: the origin publishes no records, the
    /// platform cannot query them, or the query failed. A connection falls
    /// back to the origin's addresses, as without HTTPS records.
    Unknown,
}

/// Whether an origin's HTTPS records allow an HTTP/3 attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Http3 {
    /// A record lists `h3`.
    Offered,
    /// Records exist and none lists `h3`, so HTTP/3 is not attempted
    /// (Happy Eyeballs v3).
    NotOffered,
    /// No records are known; `Alt-Svc` decides.
    Unknown,
}

impl HttpsRecords {
    /// Whether these records allow an HTTP/3 attempt.
    pub(crate) fn http3(&self) -> Http3 {
        match self {
            Self::Services { records, .. } if records.iter().any(ServiceRecord::offers_h3) => {
                Http3::Offered
            }
            Self::Services { .. } => Http3::NotOffered,
            Self::Unknown => Http3::Unknown,
        }
    }
}

/// HTTPS record lookups through the platform's resolver, cached per
/// network.
#[derive(Debug, Default)]
pub(crate) struct Resolver {
    cache: cache::Cache,
}

impl Resolver {
    /// The HTTPS records of the origin `host`:`port`, looked up on
    /// `network`, the network the platform reports traffic leaves through.
    pub(crate) async fn lookup(
        &self,
        network: Option<NetworkId>,
        host: &str,
        port: u16,
    ) -> HttpsRecords {
        self.lookup_with(network, host, port, &async move |name: &str| {
            crate::platform::query_https(network, name).await
        })
        .await
    }

    /// [`lookup`](Self::lookup), with `query` as the platform resolver.
    async fn lookup_with(
        &self,
        network: Option<NetworkId>,
        host: &str,
        port: u16,
        query: &impl AsyncFn(&str) -> Option<Answer>,
    ) -> HttpsRecords {
        let Some(name) = query_name(host, port) else {
            return HttpsRecords::Unknown;
        };
        if let Some(records) = self.cache.get(network, &name) {
            return records;
        }
        let (records, ttl) = resolve(name.clone(), query).await;
        if let Some(ttl) = ttl {
            self.cache.insert(network, &name, records.clone(), ttl);
        }
        records
    }
}

/// The name an origin's HTTPS records are published at (RFC 9460, section
/// 9.1): the host itself on port 443, and `_PORT._https.HOST` on any other.
/// `None` for an IP address, which has no records.
fn query_name(host: &str, port: u16) -> Option<String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<IpAddr>().is_ok() {
        return None;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(if port == 443 {
        host
    } else {
        format!("_{port}._https.{host}")
    })
}

/// Query `name`, following `AliasMode` records, and return what the records
/// say with how long that may be cached; `None` where it may not be, because
/// a query failed.
async fn resolve(
    mut name: String,
    query: &impl AsyncFn(&str) -> Option<Answer>,
) -> (HttpsRecords, Option<Duration>) {
    let mut ttl = Duration::MAX;
    for _ in 0..=MAX_ALIASES {
        let answer = tokio::time::timeout(QUERY_TIMEOUT, query(&name))
            .await
            .ok()
            .flatten();
        let records = match answer {
            None => return (HttpsRecords::Unknown, None),
            Some(Answer::NoRecords { negative_ttl }) => {
                let negative = negative_ttl
                    .map_or(DEFAULT_NEGATIVE_TTL, |seconds| {
                        Duration::from_secs(u64::from(seconds))
                    })
                    .min(MAX_NEGATIVE_TTL);
                return (HttpsRecords::Unknown, Some(ttl.min(negative)));
            }
            Some(Answer::Records(records)) => records,
        };
        if let Some(least) = records.iter().map(|record| record.ttl).min() {
            ttl = ttl.min(Duration::from_secs(u64::from(least)));
        }
        // A record that is malformed or makes an unknown key mandatory is
        // ignored; with none left, the origin is as if it published none
        // (RFC 9460, section 8).
        let (aliases, mut services): (Vec<ServiceRecord>, Vec<ServiceRecord>) = records
            .iter()
            .filter_map(|record| https_rr::parse(&record.rdata).ok())
            .partition(ServiceRecord::is_alias);
        // An AliasMode record makes the recipient ignore every ServiceMode
        // record in the same set (section 2.4.1).
        if aliases.is_empty() && !services.is_empty() {
            services.sort_by_key(|record| record.priority);
            let records = HttpsRecords::Services {
                owner: name.into(),
                records: services.into(),
            };
            return (records, Some(ttl));
        }
        // An alias to `.`, to itself, or nothing usable: no service is
        // published (section 2.5.1).
        match aliases.into_iter().find_map(|alias| alias.target) {
            Some(target) if target != name => name = target,
            _ => return (HttpsRecords::Unknown, Some(ttl)),
        }
    }
    (HttpsRecords::Unknown, Some(ttl))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Duration;

    use super::{
        Answer, DEFAULT_NEGATIVE_TTL, Http3, HttpsRecords, MAX_ALIASES, QUERY_TIMEOUT, Record,
        Resolver, query_name,
    };
    use crate::NetworkId;

    /// RDATA for a `ServiceMode` record at `priority` on the owner name,
    /// offering `alpn`.
    fn service(priority: u16, alpn: &[&str]) -> Vec<u8> {
        let mut out = priority.to_be_bytes().to_vec();
        out.push(0);
        if !alpn.is_empty() {
            let mut value = Vec::new();
            for id in alpn {
                value.push(u8::try_from(id.len()).unwrap());
                value.extend_from_slice(id.as_bytes());
            }
            out.extend_from_slice(&1_u16.to_be_bytes());
            out.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
            out.extend_from_slice(&value);
        }
        out
    }

    /// RDATA for an `AliasMode` record to `target`.
    fn alias(target: &str) -> Vec<u8> {
        let mut out = vec![0, 0];
        for label in target.split('.') {
            out.push(u8::try_from(label.len()).unwrap());
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "a zone maps a name to an optional answer"
    )]
    fn records(rdata: &[Vec<u8>], ttl: u32) -> Option<Answer> {
        Some(Answer::Records(
            rdata
                .iter()
                .map(|rdata| Record {
                    rdata: rdata.clone(),
                    ttl,
                })
                .collect(),
        ))
    }

    /// A platform resolver that answers from `zone` and counts its queries.
    struct Zone {
        answers: HashMap<&'static str, Option<Answer>>,
        queries: RefCell<Vec<String>>,
    }

    impl Zone {
        fn new(answers: impl IntoIterator<Item = (&'static str, Option<Answer>)>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                queries: RefCell::new(Vec::new()),
            }
        }

        async fn lookup(
            &self,
            resolver: &Resolver,
            network: u32,
            host: &str,
            port: u16,
        ) -> HttpsRecords {
            let network = Some(NetworkId::from_key(&network));
            resolver
                .lookup_with(network, host, port, &async |name: &str| {
                    self.queries.borrow_mut().push(name.to_owned());
                    self.answers.get(name).cloned().flatten()
                })
                .await
        }

        fn queries(&self) -> Vec<String> {
            self.queries.borrow().clone()
        }
    }

    #[test]
    fn records_are_published_at_the_port_prefixed_name() {
        assert_eq!(
            query_name("Example.COM.", 443).as_deref(),
            Some("example.com")
        );
        assert_eq!(
            query_name("example.com", 8443).as_deref(),
            Some("_8443._https.example.com")
        );
        assert_eq!(query_name("192.0.2.1", 443), None);
        assert_eq!(query_name("[2001:db8::1]", 443), None);
        assert_eq!(query_name("::1", 443), None);
        assert_eq!(query_name(".", 443), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_record_without_h3_means_http3_is_not_attempted() {
        let zone = Zone::new([("example.com", records(&[service(1, &["h2"])], 300))]);
        let found = zone
            .lookup(&Resolver::default(), 1, "example.com", 443)
            .await;
        assert_eq!(found.http3(), Http3::NotOffered);

        let zone = Zone::new([("example.com", records(&[service(1, &[])], 300))]);
        let found = zone
            .lookup(&Resolver::default(), 1, "example.com", 443)
            .await;
        assert_eq!(
            found.http3(),
            Http3::NotOffered,
            "no alpn is http/1.1 alone"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_record_with_h3_offers_http3() {
        let rdata = [service(2, &["h2"]), service(1, &["h3", "h2"])];
        let zone = Zone::new([("example.com", records(&rdata, 300))]);
        let found = zone
            .lookup(&Resolver::default(), 1, "example.com", 443)
            .await;
        assert_eq!(found.http3(), Http3::Offered);
        let HttpsRecords::Services { owner, records } = found else {
            panic!("{found:?}");
        };
        assert_eq!(&*owner, "example.com");
        let priorities: Vec<u16> = records.iter().map(|record| record.priority).collect();
        assert_eq!(priorities, [1, 2], "lowest priority first");
    }

    #[tokio::test(start_paused = true)]
    async fn no_records_and_failures_are_unknown() {
        let zone = Zone::new([
            (
                "none.example",
                Some(Answer::NoRecords {
                    negative_ttl: Some(30),
                }),
            ),
            ("broken.example", records(&[b"\x00".to_vec()], 300)),
        ]);
        let resolver = Resolver::default();
        for host in ["none.example", "broken.example", "failing.example"] {
            let found = zone.lookup(&resolver, 1, host, 443).await;
            assert_eq!(found, HttpsRecords::Unknown, "{host}");
            assert_eq!(found.http3(), Http3::Unknown);
        }
        assert_eq!(
            zone.lookup(&resolver, 1, "192.0.2.1", 443).await,
            HttpsRecords::Unknown
        );
        assert_eq!(
            zone.queries(),
            ["none.example", "broken.example", "failing.example"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn answers_are_cached_per_network_for_their_ttl() {
        let zone = Zone::new([("example.com", records(&[service(1, &["h3"])], 300))]);
        let resolver = Resolver::default();
        zone.lookup(&resolver, 1, "example.com", 443).await;
        zone.lookup(&resolver, 1, "example.com", 443).await;
        assert_eq!(zone.queries().len(), 1, "the second lookup is cached");
        zone.lookup(&resolver, 2, "example.com", 443).await;
        assert_eq!(zone.queries().len(), 2, "another network queries again");
        tokio::time::advance(Duration::from_secs(300)).await;
        zone.lookup(&resolver, 1, "example.com", 443).await;
        assert_eq!(zone.queries().len(), 3, "the TTL has run out");
    }

    #[tokio::test(start_paused = true)]
    async fn no_records_are_cached_for_the_negative_ttl_and_failures_not_at_all() {
        let zone = Zone::new([(
            "none.example",
            Some(Answer::NoRecords { negative_ttl: None }),
        )]);
        let resolver = Resolver::default();
        zone.lookup(&resolver, 1, "none.example", 443).await;
        zone.lookup(&resolver, 1, "failing.example", 443).await;
        zone.lookup(&resolver, 1, "none.example", 443).await;
        zone.lookup(&resolver, 1, "failing.example", 443).await;
        assert_eq!(
            zone.queries(),
            ["none.example", "failing.example", "failing.example"]
        );
        tokio::time::advance(DEFAULT_NEGATIVE_TTL).await;
        zone.lookup(&resolver, 1, "none.example", 443).await;
        assert_eq!(zone.queries().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn aliases_are_followed_with_the_least_ttl() {
        let zone = Zone::new([
            (
                "_8443._https.example.com",
                records(&[alias("svc.example.net")], 60),
            ),
            ("svc.example.net", records(&[service(1, &["h3"])], 600)),
        ]);
        let resolver = Resolver::default();
        let found = zone.lookup(&resolver, 1, "example.com", 8443).await;
        let HttpsRecords::Services { owner, .. } = &found else {
            panic!("{found:?}");
        };
        assert_eq!(&**owner, "svc.example.net");
        tokio::time::advance(Duration::from_secs(60)).await;
        zone.lookup(&resolver, 1, "example.com", 8443).await;
        assert_eq!(zone.queries().len(), 4, "the alias's TTL bounds the entry");
    }

    #[tokio::test(start_paused = true)]
    async fn an_alias_overrides_service_records_in_its_set() {
        let rdata = [service(1, &["h2"]), alias("elsewhere.example")];
        let zone = Zone::new([
            ("example.com", records(&rdata, 60)),
            ("elsewhere.example", records(&[service(1, &["h3"])], 60)),
        ]);
        let found = zone
            .lookup(&Resolver::default(), 1, "example.com", 443)
            .await;
        let HttpsRecords::Services { owner, .. } = &found else {
            panic!("{found:?}");
        };
        assert_eq!(&**owner, "elsewhere.example");
        assert_eq!(found.http3(), Http3::Offered);
        assert_eq!(zone.queries(), ["example.com", "elsewhere.example"]);

        // AliasMode to `.`.
        let rdata = [service(1, &["h2"]), vec![0, 0, 0]];
        let zone = Zone::new([("example.com", records(&rdata, 60))]);
        let found = zone
            .lookup(&Resolver::default(), 1, "example.com", 443)
            .await;
        assert_eq!(
            found,
            HttpsRecords::Unknown,
            "an alias to `.` publishes none"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn alias_chains_are_bounded() {
        let zone = Zone::new([
            ("a.example", records(&[alias("b.example")], 60)),
            ("b.example", records(&[alias("a.example")], 60)),
            ("self.example", records(&[alias("self.example")], 60)),
            ("root.example", records(&[vec![0, 0, 0]], 60)),
        ]);
        let resolver = Resolver::default();
        assert_eq!(
            zone.lookup(&resolver, 1, "a.example", 443).await,
            HttpsRecords::Unknown
        );
        assert_eq!(zone.queries().len(), MAX_ALIASES + 1);
        assert_eq!(
            zone.lookup(&resolver, 1, "self.example", 443).await,
            HttpsRecords::Unknown
        );
        assert_eq!(
            zone.lookup(&resolver, 1, "root.example", 443).await,
            HttpsRecords::Unknown
        );
        assert_eq!(zone.queries().len(), MAX_ALIASES + 3);
    }

    // A lookup through the platform's resolver can run on a spawned task.
    #[test]
    fn a_platform_lookup_is_send() {
        fn send<T: Send>(_: &T) {}
        let resolver = Resolver::default();
        send(&resolver.lookup(None, "example.com", 443));
    }

    #[tokio::test(start_paused = true)]
    async fn a_query_that_hangs_times_out_as_unknown() {
        let resolver = Resolver::default();
        let started = tokio::time::Instant::now();
        let found = resolver
            .lookup_with(None, "example.com", 443, &async |_: &str| {
                std::future::pending::<Option<Answer>>().await
            })
            .await;
        assert_eq!(found, HttpsRecords::Unknown);
        assert_eq!(started.elapsed(), QUERY_TIMEOUT);
    }
}
