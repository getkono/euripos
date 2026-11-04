# Baseline client

The plain client every euripos optimization is measured against (`docs/scope.md`, Principle 3, and "Development is driven by measured paths"). It implements the public API in [docs/design/api.md](design/api.md) on hyper 1.x and hyper-util, with no adaptive behaviour: nothing in it reacts to the network, the path or the server beyond what HTTP and ALPN require. netlab runs it beside an unmodified hyper-util client and reqwest. A later change that departs from it names its metric and shows its win over this revision.

## Transport

### Problem

`Client::send` needs a transport: HTTP/1.1 and HTTP/2 over TCP, with TLS, that every later optimization can be compared with on equal terms.

### Constraints

- HTTP semantics run on hyper 1.x and hyper-util (scope); tokio only.
- No adaptive behaviour (#3): the baseline is what a careful user of hyper-util gets without tuning.
- The public API is fixed by #2; its snapshot must not change.
- One implementation per concern on each platform (Principle 2).

### Alternatives

1. hyper's `client::conn` handshakes, pooled by hyper-util's composable `client::pool` layers.
2. hyper-util's `client::legacy::Client` with its `HttpConnector`.
3. A pool of euripos's own over `client::conn`.

### Choice

1. `crates/euripos/src/pool.rs` builds the pool from `map`, `negotiate`, `cache` and `singleton` (see "Connection pool") over a connector that resolves the host with `tokio::net::lookup_host`, connects to each returned address in order until one accepts, and runs TLS for `https`. Sockets keep the operating system's options: no `TCP_NODELAY`, no keepalive, no happy-eyeballs race. `http` requests use HTTP/1.1 only; there is no HTTP/2 without TLS. HTTP/1.1 requests are sent in origin form with a `Host` header unless the caller set one; HTTP/2 requests keep their absolute URI. A URI that is not absolute `http` or `https` fails with `ErrorKind::Connect`, since there is nothing to connect to.

### Rationale

The composable pool is the hyper-util design meant to be extended layer by layer, which is how euripos will add network-keyed pools and alternative services later; the legacy client (option 2) is a closed box that would be replaced rather than extended, and it is already in netlab as the unmodified hyper-util comparison. Option 3 duplicates what hyper-util ships. Leaving socket options at their defaults and connecting in resolver order keep the baseline free of tuning a later change would have to show a win for: `TCP_NODELAY` and happy eyeballs are such changes.

### Validated by

`client::tests::tls::*` (HTTP/2 and HTTP/1.1 over loopback TLS), `pool::tests` (origin, request form and URI validation), and the last test in `crates/euripos/tests/timeouts.rs` (HTTP/1.1 over loopback TCP).

## Connection pool

### Problem

Requests to one origin should share connections: HTTP/2 multiplexes every request over one connection, and HTTP/1.1 reuses an idle connection rather than opening one per request.

### Constraints

- Which protocol a connection speaks is known only after its TLS handshake (ALPN).
- An HTTP/1.1 connection serves one exchange at a time, and is busy until the response body has been read.
- A connection that closed while idle must not fail the next request.

### Alternatives

1. One `negotiate` service per origin in a `map`: it connects, and ALPN sends the connection to a `singleton` (HTTP/2, one shared connection) or a `cache` (HTTP/1.1, a list of idle connections).
2. HTTP/1.1 only, with one `cache` per origin.
3. One HTTP/2 connection per origin, with HTTP/1.1 opened per request.

### Choice

1. The key is the origin: scheme, lower-cased host and port, with the scheme's default port filled in. An HTTP/1.1 connection is leased to one request until its response body ends, then returns to the cache. A body dropped or failed before its end, or a request abandoned before its head arrived, marks the connection unusable, so it is closed instead of being handed to the next request mid-exchange. Closed connections are pruned from the cache and the singleton before each checkout, and a connection that reports itself closed before anything was sent on it is replaced, up to three times. Nothing is evicted for being idle: a connection lives until the server or the network closes it.

### Rationale

Option 1 is how the hyper-util pool layers are meant to compose for ALPN, and it is the plain behaviour of a browser or of hyper-util's legacy client: one HTTP/2 connection per origin, HTTP/1.1 connections reused. Option 2 gives up HTTP/2, which the scope requires; option 3 opens an HTTP/1.1 connection per request. Leasing an HTTP/1.1 connection until its body ends keeps a concurrent request from queueing behind a body still being read. The retry is safe because no byte of the request was sent; it is not a retry policy, which is #7. An idle timeout is a tuning choice with no measured value yet, so the baseline has none.

### Validated by

`client::tests::tls::alpn_h2_shares_one_connection_between_requests` (three sequential and four concurrent requests over one accepted connection), `concurrent_first_requests_to_an_h2_origin_all_succeed`, `without_alpn_h2_the_client_falls_back_to_http1_and_reuses_it`, and in `crates/euripos/tests/timeouts.rs` `a_request_within_every_timeout_completes_and_reuses_its_connection`, `a_connection_whose_request_timed_out_before_its_head_is_not_reused` and `a_connection_whose_body_stalled_is_not_reused`. hyper also closes an HTTP/1.1 connection whose exchange was abandoned, so those two tests pass without the unusable mark; the mark closes the window before hyper's connection task has noticed, in which the next request could be handed the connection. No test reaches the replacement of a connection that reports itself closed at checkout.

## TLS and protocol negotiation

### Problem

`https` requests need TLS that trusts what the platform trusts, and a way to learn whether the server speaks HTTP/2.

### Constraints

- rustls with the platform verifier on Linux, Windows and Android (scope, "Platform integration"); one crypto provider, ring ([ADR 0001](adr/0001-crypto-provider.md)).
- ring cannot be in the normal dependency graph until `mise run api:cfg` renders each target where its C code builds (#29).
- No platform type or call in the public API.

### Alternatives

1. rustls, tokio-rustls and rustls-platform-verifier, offering `h2` then `http/1.1` by ALPN, on the provider ADR 0001 specifies.
2. The same, with ring wired in through `crypto-ring` now.
3. Each platform's native TLS.

### Choice

1. `crates/euripos/src/tls.rs` builds one rustls `ClientConfig` per client on its first `https` request: the platform verifier, the safe default protocol versions, and ALPN `h2`, `http/1.1`. Until #32 wires ring in, the provider is the process-wide default (`rustls::crypto::CryptoProvider::install_default`), the path ADR 0001 specifies for a build with no `crypto-*` feature; with none installed, an `https` request fails with `ErrorKind::Tls`. A failure to build the configuration is not kept, so a provider installed later is used.

### Rationale

Option 1 is what the scope and ADR 0001 fix. Option 2 is the ADR's end state, and #32 reaches it once #29 lands; doing it here would fail the `check` and `gates` jobs for every non-host target. Option 3 is a second TLS stack on the platforms where the scope names rustls. On Android the platform verifier needs the trust initialisation the Android integration (#12) adds; until then `https` there fails with `ErrorKind::Tls`.

### Validated by

`tls::tests::alpn_offers_h2_before_http1`, `client::tests::tls::*`, and `mise run api:cfg` (the public API is the same on every target and feature set). That the platform verifier accepts a public server's certificate is checked by hand, not in CI, which has no internet dependency in tests (CONTRIBUTING.md, "Tests").

## Default timeouts

### Problem

reqwest has no timeout by default and no stall timeout ([discussion #1776](https://github.com/seanmonstar/reqwest/discussions/1776)), so a request on a dead path hangs. The four timeouts in the public API need finite defaults and an exact meaning.

### Constraints

- Every timeout is finite and can be changed but not turned off ([docs/design/api.md](design/api.md), "Configuration").
- A default is a constant in one place, not repeated in code.
- Each fires as its own `TimeoutKind`.

### Alternatives

1. Connect 10 s, TLS 10 s, total 30 s, read stall 30 s.
2. reqwest's and hyper-util's: no timeouts.
3. Shorter defaults tuned for mobile paths.

### Choice

1. The constants are `DEFAULT_CONNECT`, `DEFAULT_TLS`, `DEFAULT_TOTAL` and `DEFAULT_READ_STALL` in `crates/euripos/src/timeout.rs`, crate-private; the builder's documentation states the values.

| Timeout | Default | Bounds | Fires as |
|---|---|---|---|
| connect | 10 s | name resolution and the TCP connection, for each new connection | `TimeoutKind::Connect` |
| TLS | 10 s | the TLS handshake, for each new `https` connection | `TimeoutKind::Tls` |
| total | 30 s | the whole request, from `send` to the end of the response body | `TimeoutKind::Total` |
| read stall | 30 s | the wait for the response head once a connection is ready (writing the request body included), and for each later body frame | `TimeoutKind::ReadStall` |

The total deadline also bounds connecting: when it falls before the connect or TLS timeout would, the request fails as `Total`. A duration too large to add to the clock is treated as about thirty years.

### Rationale

These are the defaults #3 asks for. Ten seconds is far beyond a healthy connect or handshake on any path in scope, including satellite, so it fires only on a dead or blocked path; thirty seconds of total time and of silence bound a hung request without cutting off ordinary pages and API calls. Option 2 is the gap the scope names. Option 3 is an optimization, so it waits for netlab evidence; a caller with a long transfer raises `total_timeout`. The read stall is measured between body frames, not socket reads, so another stream's bytes on a shared HTTP/2 connection do not keep a stalled request alive.

### Validated by

`timeout::tests::defaults_are_the_documented_values`, and `crates/euripos/tests/timeouts.rs`: one loopback test per timeout at the point it bounds (connect against a listener whose accept queue is full, TLS against a server that never answers the ClientHello, read stall before the head and mid-body, total before the head and on a body that trickles without end), each with a 300 ms timeout and a 10 s bound on its own runtime.

## Baseline program

### Problem

netlab (#5) must run the baseline against a URL, under a workload, and read its results.

### Constraints

- The workload suite: bulk download, many small requests, long-poll, upload (scope, "Development is driven by measured paths", step 4).
- Machine-readable output; nothing in the public API exists only for netlab.

### Alternatives

1. An example binary in the facade crate that prints one JSON object.
2. A separate workspace crate with a binary.
3. A library hook netlab calls.

### Choice

1. `crates/euripos/examples/baseline.rs`, run as `cargo run --release --example baseline -- <workload> <url> [--requests N] [--concurrency C] [--upload-bytes B]`. The workloads are `download` (sequential GETs, each body read to its end; a long poll is a download from a URL that answers late), `small` (many GETs, `--concurrency` at a time) and `upload` (sequential POSTs of `--upload-bytes`). It uses `Client::new()`, so the defaults are what is measured. It prints one JSON object with `client`, `version`, `workload`, `url`, `requests`, `concurrency`, `completed`, `failed`, `errors` (by `ErrorKind`), `statuses`, `bytes_sent`, `bytes_received`, `elapsed_ms`, and the nearest-rank `p50`, `p90`, `p99` and `max` of `head_ms` (send to response head) and `complete_ms` (send to body end), each `null` when no request reached it. It exits 0 once the workload has run and 2 on a usage error.

### Rationale

An example builds with the crate at any revision, so netlab can build the baseline from its commit with nothing else, and it adds nothing to the published API. A separate crate (option 2) needs a CONTRIBUTING.md reason it does not have; a library hook (option 3) is public API for one consumer.

### Validated by

The example's unit tests (`cargo test --example baseline`: argument parsing and percentiles), and `cargo build --workspace --all-targets` in every CI job.

## Baseline revision

The baseline is the commit on `main` that merges #33, the pull request that adds this file. Its SHA cannot be written in the commit that creates it; #34 records it here once #33 has merged. Until then the revision is the head of #33's branch, `feat/3-plain-hyper-baseline-client`. Tags are a maintainer's to cut.
