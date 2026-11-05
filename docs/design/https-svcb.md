# HTTPS records

HTTPS records (RFC 9460, type 65) tell a client, before it connects, which protocols an origin serves (`alpn`), on which endpoints and ports, with which address hints, and with which Encrypted Client Hello configuration (`ech`). This document covers how the client resolves them through each platform's own resolver, what it keeps of them, and what they decide today. Happy Eyeballs v3 (#15) and HTTP/3 (#18) consume them; until those land, nothing outside the tests calls the resolver (`crates/euripos/src/dns/mod.rs`).

## Record resolution

### Problem

The scope lists "what the server announces (ALPN, `Alt-Svc`, HTTPS/SVCB records)" among the cues the client picks a protocol from, and its "DNS (system resolver first)" column requires each platform's own resolver, so that VPN split DNS, DNS64, Android's Private DNS and enterprise DNS profiles keep working (issue #14). A query that bypasses the system resolver gets a different answer, or none, on exactly the networks those features exist for.

### Constraints

- One implementation per platform (Principle 2), the one the scope names: hickory over the system configuration on Linux, dnssd on macOS and iOS, `android_res_nquery` per network on Android, `DnsQueryEx` on Windows ([ADR 0002](../adr/0002-windows-dns.md)).
- No public item: the records are an input to transport decisions, not a fact a caller reads.
- An answer is valid only on the network that gave it, for its TTL.
- A missing, slow or failed answer must never stop a connection: without records, the client connects as it would without HTTPS records.
- Tests have no internet dependency (CONTRIBUTING.md, "Tests").

### Alternatives

1. **Each platform returns wire-format RDATA; one parser in shared code.** The platform module answers one query with `dns::Answer` (`Records` of RDATA and TTL, or `NoRecords` with an optional negative TTL); `dns::https_rr` parses RFC 9460's wire form, and `dns::Resolver` follows aliases and caches.
2. **Each platform parses with whatever its API offers**: hickory's `SVCB` type on Linux, `DNS_SVCB_DATA` on Windows, hand-parsing on Apple and Android.
3. **hickory on every platform**, sending its own queries to the servers the platform lists.

### Choice

1. Every platform hands back RDATA. Linux re-encodes the `SVCB` hickory decoded (with hickory's own cache off, so the only cache is the per-network one); Android reads the whole response `android_res_nresult` returns with `dns::message`, which takes the answer section's type-65 records and the negative TTL from the authority section's SOA (RFC 2308); dnssd's reply already carries RDATA.

The lookup of origin `host:port` queries `host` on port 443 and `_PORT._https.host` otherwise (RFC 9460, section 9.1), and none for an IP address. It follows AliasMode records up to four hops; where a name's records hold an AliasMode record, its ServiceMode records are ignored (section 2.4.1); a record that is malformed, or that makes a key mandatory which the parser does not know, is ignored (section 8). The result is `HttpsRecords::Services`, the ServiceMode records lowest priority first with the name they were found at, or `HttpsRecords::Unknown`.

### Rationale

Option 1 puts the RFC's validation rules (strictly increasing keys, `mandatory`, `no-default-alpn` without `alpn`, the value shapes) in one place with one set of tests, including the RFC's own test vectors, and makes every platform's answer mean the same thing. Option 2 has four parsers that would each need those rules, and on Windows a structure whose presence depends on the Windows build (#57). Option 3 is the bypass the scope's column exists to prevent.

### Validated by

`dns::https_rr::tests` (RFC 9460 appendix D vectors, every malformed shape, truncation at every byte), `dns::message::tests`, `dns::tests::aliases_are_followed_with_the_least_ttl`, `an_alias_overrides_service_records_in_its_set`, `alias_chains_are_bounded`, `records_are_published_at_the_port_prefixed_name`, and per platform below.

## Cache

### Problem

A lookup costs a round trip to a resolver that is often across the access network. The answer must be reused, but only where it still holds.

### Constraints

- An answer from one network is not valid on the next: a VPN's split DNS, a captive portal or a different carrier's resolver answers differently (issue #14: "cached per `NetworkId` with their TTL").
- The cache is bounded.

### Alternatives

1. A cache keyed by `(Option<NetworkId>, name)`, each entry for its TTL.
2. One cache for all networks, cleared on a network change.
3. The platform resolver's own cache alone.

### Choice

1. `dns::cache::Cache` keys entries by the network the lookup ran under (where the platform names one) and the lowercase query name. An entry lives for the least TTL of the records along its alias chain, capped at a day (RFC 8767). The absence of records is kept for the negative TTL where the platform reports one (an SOA on Android and Linux) and 60 seconds where it does not (dnssd), capped at three hours (RFC 2308). A failed or timed-out query is not kept. A zero TTL keeps nothing. At 256 entries, expired entries are dropped, then the one closest to expiring.

### Rationale

The key holds only as far as the query went to the network the key names, and that differs by platform. On Android the lookup's network is routed into the query: it is sent on that network's handle, and where the callback no longer reports that network the query is not sent and nothing is cached, so an answer there cannot cross networks whatever the ordering. On Linux and on Apple platforms the system resolver answers for the network it is on and cannot be asked about another, so a lookup that straddles a change, or that runs after the change and before the client reports it, can cache the new network's answer under the old `NetworkId`. This is a known gap on those platforms: the entry serves lookups keyed to the old network, including after a return to it, for the entry's TTL at most (capped at a day). Answers cached under the new network's own `NetworkId` come from queries made after the client reported it, on the resolver the system then holds. Option 2 has the same straddling window, from its other side: an answer stored after the change but before its event is cleared with everything else, while one stored after the event but answered by the old network's resolver survives as the new network's. It also discards answers for a network the client returns to, which option 1 keeps for their TTL. Option 3 is not per network on Linux (hickory's cache, which is turned off) and is not observable on the other platforms. Failures are not cached, so a resolver that was briefly unreachable does not hide the records for the negative TTL.

### Validated by

`dns::cache::tests`, `dns::tests::answers_are_cached_per_network_for_their_ttl`, `no_records_are_cached_for_the_negative_ttl_and_failures_not_at_all`.

## What the records decide

### Problem

The records' uses are protocol selection (`alpn`), alternative endpoints (target and `port`), address hints, and ECH. Each departs from a plain hyper client, so each is evidence-gated (Principle 3), except where RFC 9460 makes it a rule.

### Constraints

- Issue #14: a record that lists no `h3` means HTTP/3 is not attempted (Happy Eyeballs v3); ECH configuration is recorded, and enabling ECH in rustls is a separate, evidence-gated decision.
- Principle 4: no records is not the same as records without `h3`.

### Alternatives

1. A three-way answer, `Http3::{Offered, NotOffered, Unknown}`: `NotOffered` only where ServiceMode records exist and none lists `h3`, `Unknown` where none exist or none could be read.
2. A boolean "HTTP/3 allowed", false without records.

### Choice

1. `HttpsRecords::http3()`. A record with no `alpn` offers `http/1.1` alone (RFC 9460, section 7.1.1), so it counts as not offering `h3`; a draft identifier such as `h3-29` is not `h3`. `ServiceRecord::ech` keeps the `ECHConfigList` bytes as received and nothing reads them. Targets, ports and hints are kept in `ServiceRecord` for #15; no connection uses them yet.

### Rationale

Option 2 would turn every origin without records, most of the web, into "no HTTP/3", where `Alt-Svc` should still decide (#18). With option 1 only the origin's own statement excludes HTTP/3.

### Validated by

`dns::tests::a_record_without_h3_means_http3_is_not_attempted`, `a_record_with_h3_offers_http3`, `no_records_and_failures_are_unknown`; `dns::https_rr::tests::mandatory_alpn_and_ipv4_hint_vector` (`h3-19` is not `h3`), `ech_is_recorded_as_received`.

## Linux

### Problem

Linux has no system API for arbitrary record types that honours the system's resolver configuration; `getaddrinfo` answers addresses only.

### Constraints

The scope names hickory over the system configuration. The answer must follow `/etc/resolv.conf` as a network change rewrites it (NetworkManager, systemd-resolved's stub at 127.0.0.53, a VPN client).

### Alternatives

1. hickory-resolver with `system-config`, built for each query from a fresh read of `/etc/resolv.conf`, its cache off.
2. One hickory resolver for the process.
3. glibc's `res_nquery`.

### Choice

1. `platform::linux::dns::query_https`, with `hickory-resolver` 0.26 (`system-config`, `tokio`; no DNSSEC, no encrypted transport of its own). A name with no records (`NoRecordsFound`) is `NoRecords` with hickory's negative TTL; any other error is no answer.

### Rationale

Option 2 keeps the servers it read at startup after a network change. Option 3 is not on musl and blocks a thread. Building the resolver costs a file read and no I/O beyond the query; behind systemd-resolved the stub then applies the per-link DNS servers and split-DNS routing domains a VPN configured.

### Validated by

`platform::linux::dns::tests::records_come_back_as_their_wire_rdata`, `no_records_and_nxdomain_are_no_records` and `a_server_failure_is_no_answer`, against a loopback name server the test runs; `the_system_configuration_is_queried_without_panicking` on the Linux runner's own configuration.

## macOS and iOS

### Problem

Network.framework resolves addresses as part of a connection and exposes no query for other record types.

### Constraints

The scope names dnssd for HTTPS records. Issue #14 leaves open whether dnssd returns HTTPS records to third-party callers on every supported iOS version.

### Alternatives

1. `DNSServiceQueryRecord` (type 65, any interface) with `kDNSServiceFlagsReturnIntermediates` and `kDNSServiceFlagsTimeout`, its socket registered with tokio and `DNSServiceProcessResult` called when it is readable.
2. `DNSServiceSetDispatchQueue`, with replies sent to the async side over a channel.

### Choice

1. `platform::apple::dnssd::query_https`. A reply that adds an HTTPS record in class `IN` is kept, and one that adds any other record, such as a CNAME on the way to it, is not; `kDNSServiceErr_NoSuchRecord` is no records (no negative TTL is reported, so it is kept for 60 seconds); any other error is no answer. The answer is taken once a reply arrives without `kDNSServiceFlagsMoreComing`. Dropping the lookup deallocates the query, after its socket is deregistered.

The open question is settled for this run as **no HTTPS record means `Unknown`**: if some iOS version withholds type 65 from apps, the lookup sees no records, the origin's records are `Unknown`, and connections fall back to A/AAAA exactly as without HTTPS records. Nothing is lost against a client without HTTPS records; the feature is absent on such a version. Device verification, per iOS version, is #58.

### Rationale

Option 1 needs no dispatch queue and no thread hop, and its replies are processed on the task that awaits them, so the context the callback writes to is borrowed for the call only. Option 2 adds a queue and a channel to the same effect. Treating an absent answer as `Unknown` rather than probing some other way keeps one implementation (Principle 2) and cannot make a connection worse.

### Validated by

`platform::apple::dnssd::tests` on the macOS runner and the iOS simulator: the reply bookkeeping on constructed replies, and a real query that completes, one that is refused before it starts, and one dropped mid-flight. Records from a public origin on devices are #58.

## Android

### Problem

Android's resolver answers per network, honouring Private DNS (DNS over TLS) and each network's own servers; a query from the process's default network is wrong when the client's connections are bound to another one (`android_setsocknetwork`, [android.md](android.md)).

### Constraints

API 29 is the floor (#12), the level `android_res_nquery` arrived at.

### Alternatives

1. `android_res_nquery` on the network the callback last reported (`NETWORK_UNSPECIFIED` where none is), its descriptor registered with tokio, `android_res_nresult` when it is readable.
2. `DnsResolver.rawQuery` over JNI.

### Choice

1. `platform::android::dns::query_https`. It is sent only where the network the lookup names is still the one the callback reports, on that network's handle, and otherwise is not sent, which is no answer. The response is read by `dns::message`; `NXDOMAIN` and `NOERROR` without records are no records, with the SOA's negative TTL; any other response code, or an error from the platform, is no answer. Dropping the lookup calls `android_res_cancel`.

### Rationale

Option 2 is the same resolver behind a JNI call, a Java callback and an `Executor`, and needs the app's `Context`, which test binaries and some embedders lack. The C API needs none of them.

### Validated by

`platform::android::dns::tests` on the emulator: a query sent only to the network it names (`a_query_goes_only_to_the_network_it_names`), a query on the default network that completes, a value that is not a network handle refused, a name that cannot be sent, and a query dropped mid-flight; the response reading is `dns::message::tests` on every platform. Records from a public origin on a device, over Private DNS, are not exercised by CI.

## Windows

`DnsQueryEx` is the source ([ADR 0002](../adr/0002-windows-dns.md)), and is not yet wired: which shape `DnsQueryEx` returns type-65 data in (`DNS_SVCB_DATA` or opaque bytes) depends on the Windows build, and the hosted runners cannot show what client builds return. Until #57 lands, the lookup has no answer on Windows and every origin's records are `Unknown`, which is the fallback the rest of this document already handles.
