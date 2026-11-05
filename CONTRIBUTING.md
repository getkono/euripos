# Contributing

[docs/scope.md](docs/scope.md) is normative: read it before any change. This file holds the conventions every change follows; [AGENTS.md](AGENTS.md) is the short form. A review checks the whole tree against both, and drift it finds is fixed in that pull request or filed as an issue.

## Setup

1. Install [mise](https://mise.jdx.dev) and [rustup](https://rustup.rs).
2. `mise trust && mise install` installs the tools pinned in `mise.toml`. `rust-toolchain.toml` pins the Rust toolchain, which rustup installs on first use.
3. `hk install` installs the git hooks: formatting and clippy on pre-commit, convco on commit-msg.

## Tasks

mise is the task interface. CI runs these same tasks, so a green `mise run check` locally is the same gate CI applies on the host.

| Task | What it runs |
|---|---|
| `mise run check` | `lint`, `test`, `docs:check` and `api` |
| `mise run fix` | `cargo fmt` and `cargo clippy --fix` |
| `mise run lint` | `cargo fmt --check`, clippy pedantic with warnings denied (default features, then all features), `cargo deny check` over all features |
| `mise run test` | Host unit, integration and doc tests, with default features and with all features |
| `mise run docs:check` | rustdoc with warnings denied |
| `mise run api` | `api:check` and `api:cfg` |
| `mise run api:check` | The facade crate's public API through `cargo public-api` (pinned nightly), diffed against `crates/euripos/tests/public-api/euripos.txt` |
| `mise run api:bless` | Rewrites that snapshot; commit it with the change that alters the API |
| `mise run api:cfg` | Fails when the public API differs between the platforms this host renders (Linux and Android, with an NDK, on Linux; macOS and iOS on macOS; Windows on Windows) or between feature sets; CI's `api-cfg` job diffs every platform (`api:cfg:render`, `api:cfg:diff DIR`) |
| `mise run api:semver [base]` | `cargo-semver-checks` against `base` (default `origin/main`); breaking only where a commit declares it |
| `mise run gates:selftest` | Proves each API gate and clippy pedantic reject a seeded violation |
| `mise run msrv` | `cargo check` on the declared `rust-version` |
| `mise run size [target]` | Stripped size of the minimal client for a target; CI holds it to a budget |
| `mise run commits:check [base]` | Conventional Commits on the branch since `base` |

`test:ios-sim` and `test:android` run the test binaries on a simulator or an emulator; CI uses them, and they work locally with Xcode or with an Android NDK and a device on adb.

`android:dex` compiles the Android monitor's Java (`crates/euripos/src/platform/android/NetworkMonitor.java`) into the `monitor.dex` beside it, which the library embeds. It needs `ANDROID_HOME` with the platform and build-tools that `mise.toml` pins, and the JDK it pins. Commit the dex with the Java that produced it; CI's `android` job rebuilds it and fails when they differ.

## Commits and pull requests

- **Conventional Commits**, checked by convco against `.convco`. Types: `feat` `fix` `docs` `test` `refactor` `perf` `build` `ci` `chore` `revert`. A scope names the area: `feat(dns): ...`, `ci(repo): ...`.
- **Squash merge only.** The pull request title becomes the one commit on `main`, so the title is a Conventional Commit, and CI checks it. The release changelog is built from those titles.
- **Atomic commits within a pull request.** Every commit on the branch builds and passes tests, and CI checks each commit message.
- **Breaking changes** use `!` after the type or scope and a `BREAKING CHANGE:` footer, in the commit and in the pull request title and body.
- **Bug fixes** land as a red test commit first, then the fix.
- **Update a branch** from `main` by merging it; never force-push a branch under review.
- **No `TODO` without an issue**: write `TODO(#NN)`.

## MSRV

Edition 2024. The minimum supported Rust version is `rust-version` in the root `Cargo.toml`: the stable release two minor versions behind the current stable when it was last set (1.97 while 1.99 is stable). The `msrv` CI job checks it.

Raising it is a minor change, not a breaking one: use `build(msrv): ...`, never `feat!`, and the changelog records it. The development toolchain in `rust-toolchain.toml` is pinned separately and moves on its own commit.

## Design documents

Design precedes code. A change that adds or alters a behaviour lands a design section in `docs/` before or with the code, with these headings:

```markdown
## <Concern>

### Problem
### Constraints
### Alternatives
### Choice
### Rationale
### Validated by
```

`Validated by` names the test, CI job or netlab result that shows the choice holds. A second implementation of one concern on one platform, or a departure from `docs/scope.md` that is not a clarification, needs an ADR in `docs/adr/NNNN-<slug>.md` with the same headings.

## Code layout

- `crates/euripos` is the facade crate and the only published one. An internal crate is added only where a `docs/scope.md` principle needs it.
- Platform code lives in one internal module per platform behind `cfg(target_os = ...)`. Everything else branches on the capabilities that module reports, never on `cfg`.
- No `cfg` on a public item. A platform fact is a typed value that may be `Unknown`.

## Tests

- Unit tests sit beside the code in a `#[cfg(test)] mod tests`.
- Integration tests of the public API go in `crates/euripos/tests/`, one file per behaviour.
- Network tests use loopback servers the test starts; CI has no internet dependency in tests.
- Platform tests live in the platform module and run in that platform's CI job: native runners for Linux, macOS and Windows, the simulator for iOS, the emulator for Android. Device-only behaviour is named as a coverage gap and tracked in an issue.
- A timeout or timing test bounds its own runtime and never sleeps for the full duration it asserts.

## API style

The [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html) apply, checked by clippy pedantic, `cargo public-api` and `cargo-semver-checks`. [docs/design/api.md](docs/design/api.md) holds the public API's design, the checklist and the gates.

- **Naming** follows RFC 430: `UpperCamelCase` types, `snake_case` functions, `as_`/`to_`/`into_` conversions by cost, no `get_` prefix on getters.
- **Errors**: one public `Error` type per crate, implementing `std::error::Error + Send + Sync + 'static`, with a `kind()` returning a `#[non_exhaustive]` `ErrorKind`. Messages are lowercase with no trailing punctuation, and the cause is reachable through `source()` rather than repeated in the message. No `unwrap` or `expect` outside tests, except on an invariant a comment states.
- **Public enums and structs that may grow** are `#[non_exhaustive]`.
- Every public item is documented (`missing_docs` warns, CI denies warnings).

## Feature flags

- Features are additive and never change a public type or signature: no `cfg(feature)` on a public item, the same rule as `cfg(target_os)`.
- Feature names are kebab-case. A choice between implementations of one concern is `<concern>-<choice>`, for example `crypto-ring`; the concern stays open to a second choice without renaming the first.
- Default features are the smallest set that makes a working client on every supported platform.
- An optional behaviour is a feature named for what it adds, for example `gzip`, `brotli` and `zstd` for the response content codings ([docs/design/parity.md](docs/design/parity.md)). Code behind a feature is tested and linted by `mise run test` and `mise run lint`, which run with all features as well as the defaults.
- One crypto provider (ring) and one runtime (tokio) are in the dependency graph; `deny.toml` bans the alternatives. Admitting another moves it out of the ban list in the change that adds its feature. [ADR 0001](docs/adr/0001-crypto-provider.md) sets how provider features combine.

## Dependencies

`cargo deny check` runs in `mise run lint`. It allows permissive licenses compatible with MIT OR Apache-2.0, crates.io as the only source, and no banned crate. A new dependency states in its pull request why it is needed.

The `size-budget` workflow fails when the stripped minimal client on any target exceeds its budget, the size first measured plus 10%. Raising a budget needs a commit whose message says what grew, by how much and why; see [ADR 0001](docs/adr/0001-crypto-provider.md).

## Releases

release-plz opens a release pull request on every push to `main`, with the version bump, the changelog entry built from the Conventional Commits since the last tag, and a `cargo-semver-checks` report. A maintainer merges it; the release job then tags `vX.Y.Z` and creates the GitHub release. Nothing is published to crates.io until a maintainer enables it in `release-plz.toml`.
