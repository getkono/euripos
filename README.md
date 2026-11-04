# euripos

> Pre-release. Nothing is published yet; the scope is settled in [docs/scope.md](docs/scope.md) and the work is tracked in this repository's issues.

euripos is an async Rust HTTP client that knows which network it is on. It sits at the level of reqwest, with a platform-agnostic public API and first-class integration on Linux, macOS, Windows, iOS and Android. HTTP runs on hyper (HTTP/1.1, HTTP/2) and h3 over QUIC (HTTP/3). The client chooses the protocol and tunes the transport from what it observes at runtime (network changes, metered and constrained paths, transport telemetry) and from what the server announces (ALPN, `Alt-Svc`, HTTPS records).

The name is the Euripus, the Greek strait whose current reverses several times a day: the current turns, and so does the transport.

## Why

reqwest is the right default for a server or a CLI on a stable network. It was not built for an app that moves from Wi-Fi to cellular mid-request, runs on a metered or constrained link, or meets a network that blocks or rate-limits UDP. [docs/scope.md](docs/scope.md#why-not-reqwest) lists the specific gaps euripos closes.

## Evidence over assertion

Every behaviour that departs from a plain hyper client must show a measured win over that baseline, and over reqwest, on real network paths recorded with [PathTrace](https://github.com/justin13888/PathTrace) and replayed in emulation. A behaviour without that evidence stays off.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
