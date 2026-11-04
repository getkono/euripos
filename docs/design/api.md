# Public API

The public API of the `euripos` crate, and the static gates that keep it idiomatic and platform-agnostic. `docs/scope.md` Principle 1 (no `cfg` on a public item), Principle 3 (every knob names its evidence) and Principle 4 (Unknown is not a default) bind every section. The crate ships this API as signatures before the transport exists: `Client::send` panics until the baseline client (#3) lands behind it, and the snapshot in `crates/euripos/tests/public-api/euripos.txt` is the contract that transport meets.

## Client and builder

### Problem

Callers need one handle to send requests through, shared across tasks, and a way to change the few settings the client exposes.

### Constraints

- tokio only (scope); the handle is spawned into tasks, so it is `Send + Sync + 'static` and cheap to clone.
- No `cfg` and no platform type in any signature.
- Defaults are the product; a setting exists only with the evidence that justifies it (see "Configuration").

### Alternatives

1. A `Client` with a consuming `ClientBuilder` whose `build` is infallible (`Client`).
2. The same, with `build` returning `Result<Client, Error>` as reqwest does.
3. Free functions over a global client.

### Choice

1. `Client::new()` and `Default` give the defaults; `Client::builder()` and `ClientBuilder::new()` start a builder whose setters take and return `self`, and `build(self) -> Client`. `Client` is `Clone + Debug + Send + Sync`; clones share connections. `Client` is neither `UnwindSafe` nor `RefUnwindSafe`. `Client::send` takes `&self`, so one client serves many concurrent requests.

### Rationale

Every current setting is a finite `Duration` that cannot be invalid, so nothing can fail at build time. Platform work that can fail (trust store initialisation on Android, network monitors) is initialised lazily and reports through the first request's `Error`, so the caller handles one failure path instead of two. Option 2 adds an error path no setting can reach; option 3 hides shared state and cannot be configured per use. Changing `build` to return a `Result` later is a breaking change, recorded in the pull request that adds this API. `Client` opts out of `UnwindSafe` and `RefUnwindSafe` through a private marker field: the pooled hyper-util client the transport (#3) puts behind it holds shared state that is neither, and removing an auto trait when it lands would be a breaking change, while adding the traits back later is not.

### Validated by

`client::tests::builder_records_each_timeout_and_leaves_the_rest_default` and `client_is_send_sync_and_static`; the auto-trait lines of the public API snapshot (`impl Send for euripos::Client`, ...) fail `mise run api:check` if a later field breaks them.

## Requests, responses and bodies

### Problem

Requests and responses need a representation that interoperates with the rest of the ecosystem (tower, axum, hyper users) without euripos re-inventing headers, methods or URIs.

### Constraints

- HTTP semantics run on hyper 1.x (scope), whose types are the `http` 1.x crate's.
- A response body streams; it is not buffered by default.
- Callers must not need to name a second crate's version to build a request.

### Alternatives

1. `http::Request<B>` in, `http::Response<euripos::Body>` out, with `http` re-exported as `euripos::http`.
2. Own `Request`/`Response`/`RequestBuilder` types wrapping `http`, as reqwest does.
3. Accept any `B: http_body::Body` for the request body.

### Choice

1. `Client::send<B: Into<Body>>(&self, Request<B>) -> impl Future<Output = Result<Response<Body>, Error>> + Send + 'static`. `Body` implements `http_body::Body<Data = Bytes, Error = euripos::Error>`, is `Send + Sync + Unpin` but neither `UnwindSafe` nor `RefUnwindSafe`, and converts from `Bytes`, `Vec<u8>`, `&'static [u8]`, `String`, `&'static str` and `()`; `Body::empty()` and `Default` give an empty body.

### Rationale

The `http` types are what hyper speaks, so no conversion sits on the hot path and callers reuse what they know. Re-exporting `http` makes its major version part of euripos's API, which it already is through `send`'s signature; the re-export lets callers write `euripos::http::Request` without a matching dependency. `Into<Body>` keeps the request side simple (`Request::get(uri).body(())`) and keeps `http_body`'s generic bounds out of the signature. A streaming request body is a later constructor on `Body` (for example `Body::wrap(impl http_body::Body)`), which is additive. Option 2 duplicates `http`; option 3 exposes `http_body`'s trait bounds and error conversion in every call. The future is `Send + 'static` so it can be spawned, which commits the transport to owning what it needs. Convenience builders (`client.get(url)`) are reqwest parity, decided with #8. `Body` opts out of `UnwindSafe` and `RefUnwindSafe` through a private marker field, for the same reason as `Client`: hyper's `Incoming`, which the transport (#3) puts behind it, is neither, and removing an auto trait then would be breaking.

### Validated by

`body::tests` (bytes yielded once, exact size hints, empty bodies at their end, `Send + Sync + Unpin`); the crate-level doctest compiles a full request; the snapshot records `send`'s exact signature.

## Errors

### Problem

A caller deciding what to do about a failure needs to know what failed, not which dependency noticed it: hyper, rustls, a platform API and the pool all fail in ways the caller handles alike.

### Constraints

- One public `Error` per crate, `std::error::Error + Send + Sync + 'static`, with `kind()` returning a `#[non_exhaustive]` `ErrorKind` (CONTRIBUTING.md, "API style").
- Underlying error types are not part of the API: they change with dependency versions and differ per platform.
- A timeout says which timeout, since each has its own setter and its own remedy.

### Alternatives

1. An opaque `Error` with `kind() -> ErrorKind`; `ErrorKind` is a `Copy` enum whose `Timeout` variant carries a `TimeoutKind`; the cause is reachable through `source()` only.
2. A public `enum Error` with a variant per failure, carrying sources.
3. `is_connect()`, `is_timeout()`, ... predicates on an opaque `Error`, as reqwest does.

### Choice

1. `ErrorKind::{Connect, Tls, Timeout(TimeoutKind), Protocol, NetworkChanged, Cancelled}` and `TimeoutKind::{Connect, Tls, Total, ReadStall}`, both `#[non_exhaustive]`, `Copy + Eq + Hash`. `Display` is a lowercase message with no trailing punctuation that names the kind (`"read timed out"`) and never repeats the source. No `is_retryable`-style predicate ships.

### Rationale

An opaque struct can carry a source and later context (the request's URI, the network it ran on) without a breaking change; a public enum (option 2) cannot. Matching on one `Copy` kind is exhaustive-checkable apart from the wildcard and needs no predicate per variant (option 3). Whether a request may be retried depends on the kind *and* the request's idempotency and how far it got, so no kind alone decides it: retry classification is designed with the retry policy (#7), which may add a predicate where the kind does decide. `NetworkChanged` is its own kind because the remedy differs from `Connect`: the request may succeed at once on the new network. `Error` is not `UnwindSafe`, because its boxed source is not; adding it later is not breaking.

### Validated by

`error::tests` (message names the timeout, the cause is reachable through `source()` and not repeated, the bounds hold); the snapshot records the variants and their attributes.

## Network facts

### Problem

Callers adapt to the network too (defer a sync on a metered link, shrink an upload on a constrained one), and euripos already observes it per platform.

### Constraints

- Principle 4: a fact the platform cannot observe is `Unknown`, never its likely value.
- No platform type and no `cfg`: the same facts exist on every platform, with `Unknown` where one cannot be read (metered on Linux without NetworkManager, constraint off Apple).
- Facts will be added as platforms report more.

### Alternatives

1. A `NetworkFacts` struct with private fields and a getter per fact, each fact an enum with an explicit `Unknown` variant.
2. `Option<T>` per fact, `None` meaning unknown.
3. A generic `Fact<T> { Known(T), Unknown }`.

### Choice

1. `Client::network_facts(&self) -> NetworkFacts`, with `interface() -> InterfaceKind { Wifi, Cellular, Ethernet, Other, Unknown }`, `metering() -> Metering { Unmetered, TemporarilyUnmetered, Metered, Unknown }` and `constraint() -> Constraint { Unconstrained, Constrained, UltraConstrained, Unknown }`. Every enum is `#[non_exhaustive]`. `NetworkFacts` is `Clone` but not `Copy`, and has no public constructor. Until the platform monitors land (#4 on Linux, #11, #12 and #13), every fact is `Unknown`.

### Rationale

An explicit `Unknown` variant makes the caller name the unknown case where it matches, so it cannot be mistaken for absence the way `None` (option 2) is read as "no". Option 3 nests every match one level deeper for no gain. `TemporarilyUnmetered` is Android's `TEMPORARILY_NOT_METERED`, which a caller treats differently from `Unmetered`; `UltraConstrained` is Apple's ultra-constrained path. Private fields and getters let a fact be added without a breaking change; not being `Copy` lets a later fact hold data that is not. Change notifications are left out until a consumer needs them: they are an additive method.

### Validated by

`facts::tests::unknown_facts_report_unknown_rather_than_a_likely_value`; `mise run api:cfg` shows the facts are the same on every platform.

## Lifecycle

### Problem

Mobile apps are suspended. A client that keeps pools and timers alive in the background wastes radio time and wakes, and one that resumes with stale connections fails its first requests (scope: "pools suspended in the background").

### Constraints

- No platform type: the app, not euripos, observes its own lifecycle (UIKit, Android `Lifecycle`) and tells the client.
- Suspending pools is a departure from a plain hyper client, so it is evidence-gated (Principle 3).

### Alternatives

1. `Client::suspend(&self)` and `Client::resume(&self)`, synchronous, idempotent, taking nothing platform-specific.
2. A lifecycle trait the app implements and registers.
3. Observe the app lifecycle inside euripos per platform.

### Choice

1, shaped here and added with the lifecycle and power policy (#16). The facade does not ship them yet.

### Rationale

Two methods on the shared handle need no platform type and no callback registration, and every platform's lifecycle maps onto them. Option 2 adds a trait and an object-safety contract for two calls; option 3 needs app-level platform hooks (an `Activity`, a `UIApplication`) a library linked into a Rust core does not have. Adding methods is additive, so shipping them with the behaviour behind them costs nothing; shipping them as no-ops would promise a behaviour with no evidence.

### Validated by

The design only, until #16; its pull request names the netlab metric.

## Configuration

### Problem

Every knob is API surface to keep and a default a caller may get wrong. Scope Principle 3 requires each to name the evidence that justifies it.

### Constraints

- Defaults are the product; knobs are the exception.
- scope, "Why not reqwest": finite defaults for connect, TLS, total time and per-read stall.

### Alternatives

1. The four timeouts and nothing else, each finite.
2. Also an option to disable a timeout (`Option<Duration>`).
3. reqwest's builder surface.

### Choice

1. `connect_timeout`, `tls_timeout`, `total_timeout` and `read_stall_timeout`, each taking a `Duration`. None can be turned off. The default values are named constants set by the baseline client (#3): connect 10 s, TLS 10 s, total 30 s, per-read stall 30 s.

| Knob | Evidence |
|---|---|
| `connect_timeout` | reqwest has no default and no separate connect bound; hangs on dead paths ([discussion #1776](https://github.com/seanmonstar/reqwest/discussions/1776)) |
| `tls_timeout` | A TLS handshake stalled by a middlebox outlives the connect bound; it is reported as its own `TimeoutKind::Tls` |
| `total_timeout` | The caller's own deadline for one request; the only bound on a slow but live transfer |
| `read_stall_timeout` | reqwest has no low-speed/stall timeout ([discussion #1776](https://github.com/seanmonstar/reqwest/discussions/1776)); transfers hang after a network switch ([#1775](https://github.com/seanmonstar/reqwest/discussions/1775)) |

### Rationale

Each of the four bounds a distinct failure the scope names, and each has a remedy only the caller knows (a long upload needs a longer total). An unbounded request is the reqwest gap the scope lists, so disabling one (option 2) is not offered: a caller who needs a very long bound sets one. Every other knob waits for its evidence.

### Validated by

The builder test here; each timeout's behaviour is tested on loopback with #3.

## Static gates

### Problem

API quality and platform-agnosticism are checked statically (Principle 1), or they drift one pull request at a time.

### Constraints

- Every gate is a mise task that CI runs (CONTRIBUTING.md, "Tasks").
- A gate that never fails proves nothing: each is shown to reject a violation.
- `cargo public-api` reads rustdoc JSON, which needs a pinned nightly (`EURIPOS_API_NIGHTLY`).

### Alternatives

For the `cfg` lint:
1. Render the public API on one target per supported OS and on each feature set, and require every render to be identical.
2. Scan the source for `cfg` attributes on `pub` items.
3. Read `#[cfg]` attributes from `cargo public-api` output: it does not print them.

### Choice

| Gate | Task | CI job | Seeded violation (`mise run gates:selftest`) |
|---|---|---|---|
| clippy pedantic, warnings denied | `lint:clippy` | `check` | a public `fn -> u8` without `#[must_use]` (`must_use_candidate`, pedantic only) |
| rustdoc, warnings denied | `docs:check` | `check` | (rustc's own `-D warnings`) |
| Public API snapshot | `api:check`; `api:bless` rewrites it | `check` | a new public function |
| No `cfg` on a public item | `api:cfg` (`scripts/lint-public-cfg`) | `check` | a public function behind `cfg(target_os = "linux")`, and one behind `cfg(feature)` |
| cargo-semver-checks | `api:semver [base]` | `semver` | a baseline with one more public function (`function_missing`) |

`api:semver` checks the change as the largest non-breaking release (patch before 1.0, minor after) unless a commit since the base declares a breaking change (`type!:` or a `BREAKING CHANGE:` footer), when it checks it as major. release-plz runs cargo-semver-checks again on the release pull request. `gates:selftest` copies the working tree for each case, appends the violation to the copy, runs the gate's own task there, and requires it to fail with the message naming the violation; it runs in the `gates` CI job.

### Rationale

Option 1 checks what the principle is about, a caller seeing a different API on another platform, and catches `cfg_attr`, `cfg`'d re-exports and features alike; options 2 and 3 cannot see through macros or re-exports, and 3 has nothing to read. Its gap is a `cfg` that holds on every target rendered (an architecture-only `cfg`, or `cfg(any(unix, windows))`); it changes nothing a caller on a supported platform sees. Seeding a copy rather than committing a red commit keeps every commit green and re-proves the gates on every change.

### Validated by

The `gates` CI job (`mise run gates:selftest`), and `mise run check` passing on the unseeded tree.

## Rust API Guidelines checklist

Against the [checklist](https://rust-lang.github.io/api-guidelines/checklist.html), for the API above.

| Guideline | Status |
|---|---|
| C-CASE, C-CONV, C-GETTER, C-ITER, C-ITER-TY | Holds: RFC 430 naming, `kind()`/`interface()` getters with no `get_`; no iterators yet |
| C-FEATURE | Holds: `crypto-ring`, the default, names the provider it adds ([ADR 0001](../adr/0001-crypto-provider.md)); `api:cfg` renders every feature set |
| C-WORD-ORDER | Holds: `ErrorKind`, `TimeoutKind`, `InterfaceKind` |
| C-COMMON-TRAITS | Holds: `Debug` everywhere; `Clone`, `Eq`, `Hash` on the value types; `Copy` on the enums; `Default` on `Client`, `ClientBuilder`, `Body` |
| C-CONV-TRAITS | Holds: `From` into `Body` |
| C-SEND-SYNC | Holds: `Client`, `Body`, `Error` are `Send + Sync`; recorded in the snapshot |
| C-GOOD-ERR | Holds: `Error` implements `std::error::Error`, lowercase messages, cause through `source()` |
| C-NUM-FMT, C-SERDE | Not applicable yet |
| C-DEBUG, C-DEBUG-NONEMPTY | Holds |
| C-CRATE-DOC, C-EXAMPLE | Holds: crate example sends a request and matches on a timeout kind |
| C-QUESTION-MARK, C-FAILURE | Holds: `# Errors` and `# Panics` on `send` |
| C-LINK, C-METADATA | Holds: intra-doc links, rustdoc warnings denied; `Cargo.toml` metadata from #1 |
| C-HTML-ROOT | Not applicable: the crate is not published, and `html_root_url` is no longer needed by rustdoc |
| C-RELNOTES | Holds: release-plz's changelog |
| C-SMART-PTR, C-CONV-SPECIFIC, C-METHOD, C-OVERLOAD, C-DEREF, C-CTOR | Holds: no `Deref`; constructors are `new`/`builder`/`empty` |
| C-COLLECT, C-EXTEND, C-RW-VALUE, C-RW-REF | Not applicable |
| C-CALLER-CONTROL, C-INTERMEDIATE, C-OBJECT | Holds: `send` takes the request by value |
| C-BUILDER | Holds: consuming `ClientBuilder` |
| C-VALIDATE, C-DTOR-FAIL, C-DTOR-BLOCK | Holds: setters take only valid `Duration`s; no `Drop` |
| C-NEWTYPE, C-CUSTOM-TYPE, C-BITFLAG | Holds: enums, not `bool`s, for every fact |
| C-GENERIC | Holds: `B: Into<Body>` |
| C-SEALED, C-STRUCT-PRIVATE, C-NEWTYPE-HIDE, C-STRUCT-BOUNDS | Holds: every struct has private fields; no public traits |
| C-STABLE, C-PERMISSIVE | Holds: `http` 1.x, `http-body` 1.x and `bytes` 1.x are the only public dependencies; MIT OR Apache-2.0 |
