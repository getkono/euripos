# Scope

This document is normative. A change follows it or changes it in the same pull request; a departure that is not a clarification gets an ADR in `docs/adr/`.

## What euripos is

An async Rust HTTP client at the level of reqwest: a library other Rust code links, not a stack apps talk to over FFI. It has a platform-agnostic public API and first-class integration with every platform it supports:

| Platform | Targets |
|---|---|
| Linux | x86_64, aarch64 |
| macOS | aarch64 |
| Windows | x86_64, aarch64 |
| iOS | aarch64 (device), aarch64 simulator |
| Android | aarch64, x86_64 (API floor decided with the Android integration) |

HTTP semantics run on hyper 1.x and hyper-util (HTTP/1.1, HTTP/2) and h3 (HTTP/3). The client picks the protocol and tunes the transport from runtime cues:
- network and path changes;
- metered, constrained and ultra-constrained state;
- live telemetry (RTT, throughput, loss and reordering signals the transport exposes);
- what the server announces (ALPN, `Alt-Svc`, HTTPS/SVCB records).

## Principles

1. **Platform-agnostic public API.** No `cfg` on a public item. A platform fact surfaces as a typed value that may be `Unknown`, never as a platform type. API quality is checked statically: clippy pedantic with warnings denied, the Rust API Guidelines checklist, a `cargo public-api` snapshot and `cargo-semver-checks`.
2. **One implementation per platform: the one that behaves correctly there.** A second implementation of the same concern on one platform needs an ADR showing that no single option is better in every way.
3. **An optimization ships only with evidence.** Every behaviour that departs from a plain hyper/hyper-util client names its metric and shows a win over that baseline, and over reqwest, on the trace-replay lab (netlab). Without that evidence it stays off or is removed.
4. **Unknown is not a default.** A fact the platform cannot observe is `Unknown` (for example, metered on a Linux host without NetworkManager), never "not metered".
5. **Rust API only.** Apps embed euripos in their own Rust core and wrap that in their own uniffi/JNI layer.

## Why not reqwest

Each item is a gap documented in reqwest's own tracker. The claim euripos makes for each must be backed by netlab evidence (Principle 3) before it appears in the README.

| Gap in reqwest | euripos |
|---|---|
| Transfers hang after a Wi-Fi to cellular switch until the old network returns ([discussion #1775](https://github.com/seanmonstar/reqwest/discussions/1775), [#1519](https://github.com/seanmonstar/reqwest/issues/1519)) | Network-keyed connection pool, evicted or drained on network change |
| No timeouts by default and no low-speed/stall timeout ([discussion #1776](https://github.com/seanmonstar/reqwest/discussions/1776)) | Finite defaults for connect, TLS, total time and per-read stall |
| System proxy read once at build ([#2674](https://github.com/seanmonstar/reqwest/issues/2674)); Windows regression ([#2715](https://github.com/seanmonstar/reqwest/issues/2715)); no PAC | Live system proxy, PAC/WPAD on every platform |
| Android TLS needs an undocumented platform init ([#2966](https://github.com/seanmonstar/reqwest/issues/2966), [#2968](https://github.com/seanmonstar/reqwest/issues/2968), [#2924](https://github.com/seanmonstar/reqwest/issues/2924)) | Platform trust initialised internally; no platform-specific call in the public API |
| HTTP/3 behind `reqwest_unstable`, no `Alt-Svc` ([#1138](https://github.com/seanmonstar/reqwest/issues/1138)), no H3/H2 race ([#1810](https://github.com/seanmonstar/reqwest/issues/1810)) | HTTPS-record and `Alt-Svc` discovery, Happy Eyeballs v3 QUIC/TCP racing, per-network broken-alternative-service backoff, and a classifier that tells UDP blocked, UDP rate-limited and slow networks apart |
| No per-network socket binding ([#2571](https://github.com/seanmonstar/reqwest/issues/2571), [#2717](https://github.com/seanmonstar/reqwest/issues/2717)) | Connections bound to the network they were made on |
| No metered or constrained awareness | No periodic wakeups on metered or constrained paths, hedging off there, pools suspended in the background |
| reqwest 0.13 grew by about 3 MB ([#3009](https://github.com/seanmonstar/reqwest/issues/3009)) | One crypto provider in the dependency graph and a binary-size budget enforced in CI |

## Platform integration

One implementation per platform (Principle 2). The cells marked ADR are decided there, with evidence.

| Platform | Connection I/O under hyper | Network and path signals | DNS (system resolver first) | Proxy and trust |
|---|---|---|---|---|
| Linux | Sockets; `TCP_INFO` | rtnetlink link/route/address; NetworkManager `Metered` over D-Bus (absent: `Unknown`) | hickory over the system configuration, HTTPS RR | Environment variables; rustls with the platform verifier |
| macOS, iOS | Network.framework `nw_connection` (TCP and TLS) as hyper's `Read`/`Write`: connect-by-name, VPN On Demand, MPTCP, viability and better-path events (Apple TN3151) | `nw_path_monitor`: expensive, constrained, ultra-constrained | Network.framework resolution; dnssd for HTTPS RR | System proxy and Security.framework trust through Network.framework |
| Android | Sockets bound with `android_setsocknetwork`; `TCP_INFO` | `ConnectivityManager.NetworkCallback` over JNI (`NOT_METERED`, `TEMPORARILY_NOT_METERED`) | `android_res_nquery` per network (honours Private DNS) | `LinkProperties` proxy; rustls with the platform verifier, initialised through `ndk-context` |
| Windows | Winsock sockets | `NotifyIpInterfaceChange`, `NotifyRouteChange2`; `INetworkCostManager` | `DnsQueryEx` ([ADR 0002](adr/0002-windows-dns.md)) | WinHTTP proxy with PAC/WPAD; rustls with the platform verifier |

HTTP/3 uses h3 over quinn on every platform except Apple, where the QUIC transport (Network.framework QUIC or quinn over UDP) is decided by an ADR on device-lab evidence. Only one is chosen.

## Out of scope

- **wasm and browsers.** A browser exposes no path signals and `fetch` owns the transport, so nothing here would apply.
- **FreeBSD and other unlisted targets.**
- **Background transfers that outlive the app.** Apple allows those only through `URLSession` background sessions, and Android through WorkManager/DownloadManager. Apps use those for that case; the docs say so.
- **A blocking API in 0.x.**
- **Swift and Kotlin bindings.**
- **Server-side HTTP.** getkono/kynos covers that.
- **Runtimes other than tokio.**

## Development is driven by measured paths

[PathTrace](https://github.com/justin13888/PathTrace) records what real paths do to packets into PTF traces. netlab replays them:

1. Capture PTF traces of representative challenging paths: tethered cellular, satellite, congested Wi-Fi, UDP-policed networks, and paths that change mid-session.
2. Export each trace to Rattan or netem and replay it between network namespaces on Linux.
3. Calibrate: an emulated path is used only if reference flows through it match the trace's own reference-flow (VALD) results.
4. Run the workload suite (bulk download, many small requests, long-poll, upload) for euripos, the plain hyper baseline and reqwest.
5. On iOS and Android, route the device through a Linux gateway (Wi-Fi AP or USB tether) running the same replay.
6. Score runtime classifiers and estimators against the trace's ground truth: the per-transport policy matrix, reordering, path changes, NAT binding lifetime and capacity.
