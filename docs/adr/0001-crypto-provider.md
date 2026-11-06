# ADR 0001: ring as the one crypto provider, and a binary-size budget

Status: accepted. Issue #17; amended by #28 ("aws-lc-rs as a second provider").

## Crypto provider

### Problem

rustls and quinn each need a crypto provider, and both default to aws-lc-rs. Two providers in one graph, or the larger one by default, cost binary size on every platform: reqwest 0.13's move to aws-lc-rs added about 3 MB ([reqwest#3009](https://github.com/seanmonstar/reqwest/issues/3009)), and `docs/scope.md` lists one provider and a size budget as the answer to that gap. The choice also shapes the feature flags, which are public and checked by `cargo-semver-checks`.

### Constraints

- One provider in the dependency graph; rustls and quinn must agree on it (`docs/scope.md`, "Why not reqwest").
- Features are additive and never change a public type or signature (CONTRIBUTING.md, "Feature flags"). `mise run api:cfg` renders the public API with default, no and all features, and requires the renders to match.
- A second provider may be admitted later. Admitting it, and later changing which provider is the default, must not need a breaking change.
- Optimizations are evidence-gated (`docs/scope.md`, Principle 3): a provider chosen for speed needs a netlab win, and nothing measured here shows one yet.
- Every supported target builds the provider: Linux, macOS, Windows, iOS device and simulator, Android.

### Alternatives

1. **ring**, behind a default `crypto-ring` feature, with aws-lc-rs and the other providers banned in `deny.toml`.
2. **aws-lc-rs**, the rustls and quinn default. It brings post-quantum key exchange and a FIPS build, and may be faster on some handshakes and ciphers, at the size cost reqwest#3009 measured; its build also needs CMake (and NASM on some Windows targets) where prebuilt objects are missing.
3. **Both, chosen by feature** from the start, with no default.
4. **The platform's TLS** (Security.framework, SChannel, the Android stack) through native-tls. This is not a rustls provider. The table in `docs/scope.md` puts rustls with the platform verifier on Linux, Windows and Android, so it would be a second TLS stack on those platforms.

### Choice

1. ring is the baseline provider. The facade crate has a `crypto-ring` feature, on by default; the change that adds TLS makes it enable `rustls/ring`, and the change that adds HTTP/3 makes it enable `quinn?/rustls-ring`. Until then the feature selects nothing, because nothing in the graph uses a provider yet; it exists now so its name, and the scheme below, are fixed before anything depends on them. Putting ring in the graph first needs `mise run api:cfg` to render each target where ring's C code can build, which it cannot from one Linux host (#29).

The feature scheme, which any later provider follows:

- A provider feature is named `crypto-<provider>` (`crypto-ring`; a later one would be `crypto-aws-lc-rs`), the `<concern>-<choice>` rule in CONTRIBUTING.md.
- Provider features are additive and change no public item.
- With no `crypto-*` feature enabled, the crate still builds. The client then uses the process-wide rustls provider the application installed (`rustls::crypto::CryptoProvider::install_default`), and a request fails with `ErrorKind::Tls` if there is none. This leaves room for a provider euripos does not name.
- Where more than one is enabled, a provider other than the default wins over the default one, so `features = ["crypto-aws-lc-rs"]` takes effect even when another dependency of the application leaves default features on. The ADR that admits a second non-default provider sets the order between them.
- `deny.toml` bans every provider crate other than ring, and the rustls, tokio-rustls, hyper-rustls, quinn and quinn-proto features that would pull one in. Admitting a provider removes its ban in the change that adds its feature.

### Rationale

ring is the smaller provider, builds on every supported target with only a C compiler, and is a rustls and quinn provider, so one feature can switch both. aws-lc-rs's possible advantages are performance, which Principle 3 does not let euripos claim without a netlab result, and FIPS and post-quantum, which no issue in scope asks for. The evaluation of aws-lc-rs for speed against its size cost is recorded below, under "aws-lc-rs as a second provider"; if it wins, it is admitted as `crypto-aws-lc-rs` under the scheme above, with this ADR superseded or amended.

The scheme is shaped so that change is not breaking. Adding a feature is additive to `cargo-semver-checks`. A build with no provider feature is allowed now, so admitting a second provider never has to remove a `compile_error!`. Letting the non-default provider win means that admitting it, or later making it the default, never changes what an existing `crypto-ring` build gets, and a feature enabled through Cargo's feature unification cannot silently cancel a user's explicit choice. Option 3 has no default, so the plain `euripos = "0.1"` dependency would not work, against the rule that default features make a working client on every platform. Option 4 is a second TLS stack per platform, which Principle 2 rules out without an ADR of its own.

### Validated by

- `mise run lint:deny` (`cargo deny check`, in `mise run check` and the `check` CI job) fails if aws-lc-rs, aws-lc-sys, openssl, native-tls, boring or rustls-post-quantum enters the graph, or if rustls, tokio-rustls, hyper-rustls, quinn or quinn-proto has one of its aws-lc-rs or FIPS features enabled.
- `mise run api:semver` (the `semver` CI job) fails if `crypto-ring` is renamed or removed.
- `mise run api:cfg` fails if a provider feature changes the public API.
- The size budget below fails if a provider's code is added to the minimal client without its budget being raised.

## Binary-size budget

### Problem

A size claim is only kept if CI fails when it stops holding. Without a budget, a dependency or feature that adds megabytes, as reqwest#3009 did, passes review unnoticed.

### Constraints

- Every supported target is measured, since size matters most on iOS and Android and differs by target.
- The measured thing is stable and reproducible: the `minimal` example (`crates/euripos/examples/minimal.rs`) built with `mise run size`, release profile, symbols stripped, on the same hosted runners each time.
- A budget that is too tight fails on noise; one that is too loose finds nothing.

### Alternatives

1. A fixed byte budget per target: the stripped size first measured, plus 10%.
2. A relative check against the base branch's size on every pull request (for example, at most 2% growth).
3. One absolute ceiling for every target.

### Choice

1. The `size-budget` workflow (`.github/workflows/size-budget.yml`) builds the minimal client per target and fails when its stripped size exceeds that target's budget. Each budget is the stripped size first measured plus 10%, rounded up to a byte, and is written next to the measured size in the workflow's matrix.

Raising a budget needs a commit of its own, `build(size): ...` or the commit of the change that needs it, whose message says what grew, by how much, and why the growth is worth it; the new budget is the new measured size plus 10%. The change that wires TLS through `crypto-ring` raises them this way, since it adds ring's code to the client for the first time.

### Rationale

A fixed budget keeps slow growth visible: option 2 lets many small changes, each within its margin, add up without anyone deciding to accept the total. The 10% margin absorbs toolchain and linker drift, but not a second provider, which adds far more. Option 3 fits no target well, because the targets differ by more than any margin.

The first measurement is of the minimal client as it stands before TLS: it links the facade but no provider, so today the budget guards the baseline every later change starts from rather than ring itself. The sizes were measured by the `size` CI job on `main` at 0259678, and nothing since has changed that binary: a local `x86_64-unknown-linux-gnu` build is 332744 bytes stripped both at 0259678 and with this change applied. A local build differs from the hosted runner's by its linker and libc, so the CI figures are the ones recorded.

| Target | Stripped bytes measured | Budget |
|---|---|---|
| `x86_64-unknown-linux-gnu` | 331960 | 365157 |
| `aarch64-unknown-linux-gnu` | 332640 | 365905 |
| `aarch64-apple-darwin` | 342096 | 376306 |
| `aarch64-apple-ios` | 322432 | 354676 |
| `aarch64-apple-ios-sim` | 325088 | 357597 |
| `x86_64-pc-windows-msvc` | 123904 | 136295 |
| `aarch64-pc-windows-msvc` | 123904 | 136295 |
| `aarch64-linux-android` | 314800 | 346280 |
| `x86_64-linux-android` | 340424 | 374467 |

### Validated by

The `size (<target>)` jobs of the `size-budget` workflow on every pull request and push to `main`; each job prints the measured size, the budget and the share used, and fails above the budget.

## aws-lc-rs as a second provider

Issue #28, open: it is `evidence-gated`, so it is not done until the performance results exist. Status: not admitted for now, on the size measurement below; #60 makes the remaining measurements, and #28 closes only when they decide it.

### Problem

aws-lc-rs may be faster than ring on some handshakes and ciphers, and it is the provider rustls, tokio-rustls, hyper-rustls and quinn enable by default. Admitting it as `crypto-aws-lc-rs` is worth its size cost only if it wins on a named metric by more than that cost is worth.

### Constraints

- Optimizations are evidence-gated (`docs/scope.md`, Principle 3; CLAUDE.md): a provider admitted for speed names its metric and shows a netlab win over the plain hyper baseline and over reqwest.
- Size matters most on iOS and Android, and differs by target.
- Admission follows the feature scheme above: a `crypto-aws-lc-rs` feature, its crate and feature bans removed from `deny.toml` in the same change, and the size budgets raised in a commit that says by how much and why.
- Nothing in scope asks for FIPS or post-quantum key exchange; a request for either is a separate input to this decision.

### Measurement

The size aws-lc-rs adds over ring was measured on `x86_64-unknown-linux-gnu`, the one supported target a Linux host can link without a platform SDK. The minimal client of the size budget links no provider until #32 wires `crypto-ring` into rustls, and with an empty `main` it would drop a provider's code as dead anyway, so the measured program is a standalone TLS client whose provider code is reachable. It connects over TCP, builds a `rustls::ClientConfig` with `builder_with_provider` and `rustls_platform_verifier::Verifier`, completes a handshake through `tokio_rustls::TlsConnector` on a current-thread tokio runtime, and reads a response. Three builds differ only in the provider feature on rustls and tokio-rustls: none (TCP only, no rustls), `ring`, and `aws-lc-rs`. The versions are the ones `Cargo.lock` holds where it has them: rustls 0.23.46, tokio-rustls 0.26.6, rustls-platform-verifier 0.7.1, tokio 1.54.1, ring 0.17.14; and aws-lc-rs 1.19.0 with aws-lc-sys 0.47.0. Rust 1.99.1 (`rust-toolchain.toml`), GCC 16.2.1, release profile with symbols stripped as `mise run size` builds it.

| Build | Stripped bytes | With fat LTO, one codegen unit |
|---|---|---|
| no TLS | 548096 | |
| rustls with ring | 2165448 | 1783968 |
| rustls with aws-lc-rs | 4316640 | 3916928 |
| aws-lc-rs over ring | +2151192 (+99%) | +2132960 (+120%) |

That is about 2.1 MB per binary, in line with the 3 MB reqwest#3009 reported, and it roughly doubles a TLS client. LTO does not reduce it, since most of it is aws-lc's C and assembly, which Rust's LTO does not reach.

Handshake cost and bulk throughput need mid-range iOS and Android devices and the netlab workloads, which this repository has not yet built (#5, #10); the size on the other eight supported targets needs their hosted runners. Neither was measured here.

### Alternatives

1. **Admit `crypto-aws-lc-rs` now**, off by default, on the size measurement alone.
2. **Keep ring the only provider**, keep the bans in `deny.toml`, and re-decide once the performance results exist.
3. **Admit it for FIPS or post-quantum key exchange**, whatever its speed.

### Choice

2. aws-lc-rs is not admitted. `deny.toml` keeps banning `aws-lc-rs`, `aws-lc-sys`, `aws-lc-fips-sys` and `rustls-post-quantum`, and the rustls, tokio-rustls, hyper-rustls, quinn and quinn-proto features that pull them in; no `crypto-aws-lc-rs` feature exists and the size budgets are unchanged. #60 measures the rest, and #28 then either admits it under the scheme above or records that it lost.

### Rationale

The size cost is measured and large; the benefit is not measured at all, and Principle 3 lets euripos claim no speed it has not shown on netlab. Option 1 would put a provider in the public feature set on no evidence, and once a feature is public it cannot be removed without a breaking change; leaving it out costs nothing, because the scheme above admits it later without one. Option 3 answers a request nobody in scope has made. An application that needs aws-lc-rs today can still install it as the process-wide rustls provider and build euripos without `crypto-ring`, which the scheme allows (a build with no `crypto-*` feature); `deny.toml` keeps it out of euripos's own graph, not the application's.

### Validated by

- The measurement above, which #60 repeats per target once #32 has wired ring into the minimal client.
- `mise run lint:deny` fails if aws-lc-rs, aws-lc-sys or aws-lc-fips-sys enters the graph, or a feature that pulls one in is enabled.
