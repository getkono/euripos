# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/getkono/euripos/releases/tag/v0.1.0) - 2026-11-07

### Bug Fixes

- *(core)* replace a closed shared HTTP/2 connection under its lock ([#44](https://github.com/getkono/euripos/pull/44))
- *(core)* keep a percent escape whose digits are not both hex ([#71](https://github.com/getkono/euripos/pull/71))

### Build

- *(repo)* ring as the one crypto provider and a binary-size budget ([#30](https://github.com/getkono/euripos/pull/30))

### CI

- *(platform)* run the Linux tests under a session bus and fail on skip ([#50](https://github.com/getkono/euripos/pull/50))

### Chores

- *(repo)* workspace, tooling and five-platform CI ([#25](https://github.com/getkono/euripos/pull/25))
- *(platform)* repoint the platform proxy consumer references from #49 to #63 ([#72](https://github.com/getkono/euripos/pull/72))

### Documentation

- *(repo)* scope, licenses and agent rules
- *(core)* say the read-stall clock covers writing the request body ([#42](https://github.com/getkono/euripos/pull/42))
- *(core)* name Shared in the Http2 doc comment ([#46](https://github.com/getkono/euripos/pull/46))

### Features

- *(api)* public API facade and its static gates ([#27](https://github.com/getkono/euripos/pull/27))
- *(core)* plain hyper client as the measured baseline ([#33](https://github.com/getkono/euripos/pull/33))
- *(platform)* network context model and the Linux monitor ([#43](https://github.com/getkono/euripos/pull/43))
- *(core)* reqwest-parity features ([#48](https://github.com/getkono/euripos/pull/48))
- *(platform)* Windows network monitor, connection cost and WinHTTP proxy with PAC ([#52](https://github.com/getkono/euripos/pull/52))
- *(platform)* Apple: Network.framework connections under hyper and path monitoring ([#54](https://github.com/getkono/euripos/pull/54))
- *(platform)* Android network callback, per-network binding, proxy and trust init ([#56](https://github.com/getkono/euripos/pull/56))
- *(platform)* HTTPS/SVCB resolution through the system resolver ([#59](https://github.com/getkono/euripos/pull/59))
- *(tls)* select ring through the crypto-ring feature ([#62](https://github.com/getkono/euripos/pull/62))
- *(core)* send requests through an HTTP proxy, with proxy authentication ([#64](https://github.com/getkono/euripos/pull/64))
- *(platform)* Windows: HTTPS records through DnsQueryEx ([#77](https://github.com/getkono/euripos/pull/77))
- *(platform)* route requests through the proxy Android, Windows and Apple report ([#78](https://github.com/getkono/euripos/pull/78))

### Tests

- *(proxy)* show each redirect hop choosing its own route ([#76](https://github.com/getkono/euripos/pull/76))
- *(core)* time out a CONNECT a silent proxy never answers ([#75](https://github.com/getkono/euripos/pull/75))
- *(core)* pin loopback test clients with no_proxy() ([#74](https://github.com/getkono/euripos/pull/74))
