//! HTTPS records through hickory over the system's resolver configuration
//! (`/etc/resolv.conf`), read afresh for every query so a network change
//! that rewrites it is followed. See docs/design/https-svcb.md, "Linux".

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::rr::{RData, RecordType};
use hickory_resolver::proto::serialize::binary::BinEncodable;
use hickory_resolver::system_conf::read_system_conf;

use crate::NetworkId;
use crate::dns::{Answer, Record};

/// Query `name`'s HTTPS records through the configuration the system holds
/// now. `None` where the system configuration cannot be read or the query
/// fails.
///
/// `network` is not read: Linux has one resolver configuration, for the
/// network it is on, so a query cannot be put to another network. During a
/// network change the answer may come from the new network; the cache's
/// bound on that is in docs/design/https-svcb.md, "Cache".
pub(crate) async fn query_https(_network: Option<NetworkId>, name: &str) -> Option<Answer> {
    let (config, options) = read_system_conf().ok()?;
    query_with(config, options, name).await
}

/// Query `name`'s HTTPS records through the name servers `config` lists.
async fn query_with(
    config: ResolverConfig,
    mut options: ResolverOpts,
    name: &str,
) -> Option<Answer> {
    // The caller caches per network; a second cache here would outlive a
    // network change. `/etc/hosts` holds no HTTPS records.
    options.cache_size = 0;
    let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
    *builder.options_mut() = options;
    let resolver = builder.build().ok()?;
    // A fully qualified name: the search list does not apply.
    let lookup = match resolver.lookup(format!("{name}."), RecordType::HTTPS).await {
        Ok(lookup) => lookup,
        Err(NetError::Dns(DnsError::NoRecordsFound(no_records))) => {
            return Some(Answer::NoRecords {
                negative_ttl: no_records.negative_ttl,
            });
        }
        Err(_) => return None,
    };
    let records: Vec<Record> = lookup
        .answers()
        .iter()
        .filter_map(|record| match &record.data {
            RData::HTTPS(https) => Some(Record {
                rdata: https.0.to_bytes().ok()?,
                ttl: record.ttl,
            }),
            _ => None,
        })
        .collect();
    Some(if records.is_empty() {
        Answer::NoRecords { negative_ttl: None }
    } else {
        Answer::Records(records)
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use hickory_resolver::config::{NameServerConfig, ResolverConfig, ResolverOpts};
    use tokio::net::UdpSocket;

    use super::query_with;
    use crate::dns::message::{CLASS_IN, TYPE_HTTPS};
    use crate::dns::{Answer, Record};

    /// RDATA of `svc.example.com` at priority 1, offering `h3` and `h2` on
    /// port 8443 with an IPv4 hint and an ECH configuration.
    const RDATA: &[u8] = b"\x00\x01\x03svc\x07example\x03com\x00\
        \x00\x01\x00\x06\x02h3\x02h2\
        \x00\x03\x00\x02\x20\xfb\
        \x00\x04\x00\x04\xc0\x00\x02\x01\
        \x00\x05\x00\x04\x00\x02\xfe\x0d";

    /// A name server on loopback that answers one query: with [`RDATA`]
    /// where `answer` is set, otherwise with `rcode` and no records.
    async fn server(answer: bool, rcode: u8) -> (IpAddr, u16, tokio::task::JoinHandle<()>) {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = socket.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut query = [0; 512];
            let (length, peer) = socket.recv_from(&mut query).await.unwrap();
            let query = &query[..length];
            // The question ends after its name and four bytes of type and class.
            let mut end = 12;
            while query[end] != 0 {
                end += 1 + usize::from(query[end]);
            }
            end += 5;
            let mut response = query[..2].to_vec();
            response.extend_from_slice(&[
                0x81,
                0x80 | rcode,
                0,
                1,
                0,
                u8::from(answer),
                0,
                0,
                0,
                0,
            ]);
            response.extend_from_slice(&query[12..end]);
            if answer {
                response.extend_from_slice(&[0xc0, 0x0c]);
                response.extend_from_slice(&TYPE_HTTPS.to_be_bytes());
                response.extend_from_slice(&CLASS_IN.to_be_bytes());
                response.extend_from_slice(&300_u32.to_be_bytes());
                response.extend_from_slice(&u16::try_from(RDATA.len()).unwrap().to_be_bytes());
                response.extend_from_slice(RDATA);
            }
            socket.send_to(&response, peer).await.unwrap();
        });
        (address.ip(), address.port(), task)
    }

    fn config(ip: IpAddr, port: u16) -> ResolverConfig {
        let mut server = NameServerConfig::udp(ip);
        server.connections[0].port = port;
        ResolverConfig::from_name_servers(vec![server])
    }

    #[tokio::test]
    async fn records_come_back_as_their_wire_rdata() {
        let (ip, port, server) = server(true, 0).await;
        let answer = query_with(config(ip, port), ResolverOpts::default(), "example.com").await;
        server.await.unwrap();
        assert_eq!(
            answer,
            Some(Answer::Records(vec![Record {
                rdata: RDATA.to_vec(),
                ttl: 300
            }]))
        );
    }

    #[tokio::test]
    async fn no_records_and_nxdomain_are_no_records() {
        for rcode in [0, 3] {
            let (ip, port, server) = server(false, rcode).await;
            let answer = query_with(config(ip, port), ResolverOpts::default(), "example.com").await;
            server.await.unwrap();
            assert!(
                matches!(answer, Some(Answer::NoRecords { .. })),
                "{rcode}: {answer:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_server_failure_is_no_answer() {
        let (ip, port, server) = server(false, 2).await;
        let mut options = ResolverOpts::default();
        options.attempts = 1;
        let answer = query_with(config(ip, port), options, "example.com").await;
        server.await.unwrap();
        assert_eq!(answer, None);
    }

    #[tokio::test]
    async fn the_system_configuration_is_queried_without_panicking() {
        // Hosted runners resolve through their own configuration; whatever
        // it answers for a reserved name, the query completes.
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            super::query_https(None, "euripos.invalid"),
        )
        .await;
        assert!(
            answer.is_ok(),
            "the query is bounded by the resolver's own timeouts"
        );
    }
}
