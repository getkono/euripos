# ADR 0003: Network.framework connections on macOS and iOS

Status: accepted. Issue #11. The path monitor this ADR's bindings also serve is in [network-context.md](../design/network-context.md), "Apple monitor".

## Connection I/O

### Problem

The baseline client resolves a name with `getaddrinfo` and connects a BSD socket to each address in turn (`docs/baseline.md`, "Transport"). Apple's TN3151, "Choosing the right networking API", says a stack built that way on Apple platforms cannot trigger VPN On Demand, misses stack reconfigurations unless it watches each connection, and has no Multipath TCP. `docs/scope.md` therefore puts Network.framework's `nw_connection` under hyper on macOS and iOS. Two questions are open: how Rust reaches Network.framework, and whether the connection also runs TLS or carries the bytes TLS runs over.

### Constraints

- hyper runs over tokio's `AsyncRead` and `AsyncWrite` (through hyper-util's `TokioIo`), as on every platform; tokio only.
- The four finite timeouts each fire and report their own kind (`docs/baseline.md`, "Default timeouts"): connect and TLS separately. `crates/euripos/tests/timeouts.rs` runs on the macOS job and on the iOS simulator.
- One implementation per platform (Principle 2), and one crypto provider in the graph ([ADR 0001](0001-crypto-provider.md)).
- Trust is the system's: Security.framework decides, so user- and MDM-installed roots and revocation policy apply.
- No `cfg` on a public item; the platform shows only in what `NetworkFacts` reports.
- The library still type-checks and lints from Linux (`cargo clippy --target aarch64-apple-darwin`), where the Apple SDK cannot be used ([api.md](../design/api.md), "Where each OS renders"), and builds on the hosted macOS runners with the Xcode they ship.
- The stripped minimal client stays within its size budget on `aarch64-apple-darwin`, `aarch64-apple-ios` and `aarch64-apple-ios-sim` (ADR 0001).
- Multipath TCP is an optimization: off until netlab shows a win (Principle 3).

### Alternatives

For the bindings:

1. cidre's `nw` module.
2. Declarations of the C functions the client calls, written here, with `block2` for the block ABI the callbacks use.
3. A Swift shim, compiled by a build script and called over a C interface.

For TLS:

1. Network.framework's TLS: `nw_parameters_create_secure_tcp` with TLS options, ALPN through `sec_protocol_options_add_tls_application_protocol`.
2. Network.framework for TCP only, and rustls with rustls-platform-verifier over the connection, as on every other platform. The verifier evaluates the chain with Security.framework's `SecTrust`.

### Choice

Bindings option 2 and TLS option 2.

`crates/euripos/src/platform/apple/ffi.rs` declares the Network.framework and libdispatch functions the platform calls, and `connection.rs` wraps an `nw_connection` as a `Stream` the connector hands to TLS and hyper. `platform::connect(host, port)` creates an endpoint by name (`nw_endpoint_create_host`), so Network.framework resolves the host, races its addresses and brings up VPN On Demand, with TCP's default options and TLS disabled. The connection is ready when Network.framework says so; one that enters `waiting` or `failed` first fails as `ErrorKind::Connect` with Network.framework's error, its POSIX code kept as the `io::Error`'s OS error. The client does not wait for connectivity: the connect timeout bounds the attempt, and dropping it cancels the connection. TLS then runs in `crate::tls` over the stream, under the TLS timeout, exactly as over a socket.

The stream keeps one receive outstanding, up to 64 KiB, while a read waits. A write copies its bytes into dispatch data and hands them to `nw_connection_send`; once 256 KiB are handed over and not yet reported sent, a write waits. A flush waits for every send to complete, and a shutdown sends the end of the stream (`NW_CONNECTION_FINAL_MESSAGE_CONTEXT`) after them. A peer that closes its side reads as the end of the stream. Dropping the stream cancels the connection. Every connection and the path monitor deliver their callbacks on one serial dispatch queue; a callback only records what it was given and wakes a task.

Multipath TCP stays at Network.framework's default for new parameters, which is disabled; enabling it (`nw_parameters_set_multipath_service`) is an evidence-gated change. The viability and better-path handlers are not registered until the network-keyed pool (#6) consumes them, and the transfer metrics the estimator (#21) would read are left to it; both are tracked in #53.

### Rationale

**Bindings.** The client calls about thirty C functions of a stable API. cidre (option 1) builds Objective-C sources with `xcrun` in its build script, so the library could no longer be checked from Linux, and its `nw` feature pulls in its Foundation bindings; it is a large 0.x crate whose releases break its API. A Swift shim (option 3) puts `swiftc` into every Apple build, a second language into the crate, and a call through a C interface the shim would define anyway. Hand-written declarations need nothing beyond the frameworks every Apple build links; `block2`, from the objc2 project, implements the block ABI in Rust with no build script. The `unsafe` this costs is confined to `platform/apple`, with each call's contract stated where it is made.

**TLS.** Network.framework establishes TCP and TLS as one step and reports only that the connection is ready: no event marks the TCP handshake done. With its TLS (option 1), a handshake that stalls cannot be told from a connection that does not open, so the connect and TLS timeouts would merge into one and `tls_timeout_fires_while_the_server_never_answers_the_client_hello` would fail on Apple. Option 1 would also read ALPN through its own metadata and configure trust through its own verify block, a second path for HTTP/2 negotiation and for every TLS test, on two platforms only. Option 2 keeps one TLS stack and one provider everywhere, and leaves trust to Security.framework through the platform verifier.

What TN3151 names is the connection's: connect-by-name, VPN On Demand, reconfiguration, viability and better-path events, and Multipath TCP all belong to `nw_connection` with TLS disabled, and this keeps them. What option 2 gives up is what the system TLS stack adds on top: its session cache and App Transport Security's per-domain policy, which apply to URLSession and to Network.framework's TLS but not to rustls; and whatever proxy handling Network.framework applies to a TLS connection. Proxies are the client's own concern on every platform (#49), so the parameters also set `nw_parameters_set_prefer_no_proxy`: a system-configured proxy does not carry the TCP connection without the client knowing, and the client's proxy support, when it reads the system's settings, decides the route.

No option dominates. Network.framework's TLS would bring ATS and the system's session cache; it is not chosen because the separate TLS timeout and one TLS path are commitments of the baseline every platform shares.

### Validated by

On the macOS job (`aarch64-apple-darwin`) and the iOS simulator job (`mise run test:ios-sim`): `platform::apple::connection::tests::bytes_go_both_ways_and_each_end_sees_the_other_close` (a megabyte each way through a loopback server, more than the send window, and each end's close), `a_refused_connection_fails_as_connect`, `a_host_with_a_nul_fails_as_connect`; `platform::apple::tests::errors_keep_their_posix_code_and_name_the_others`. `client::tests::tls::*` (ALPN, a shared HTTP/2 connection, HTTP/1.1 reuse) and `crates/euripos/tests/timeouts.rs` (each timeout, with connect and TLS told apart) run over Network.framework connections on those targets. `cargo clippy --target aarch64-apple-darwin` and `--target aarch64-apple-ios` check the library from Linux.

Coverage gaps: the hosted runners have one wired network and no VPN, so VPN On Demand, a Wi-Fi to cellular handover and Multipath TCP are not exercised; the netlab device-lab result (#10) measures the handover.
