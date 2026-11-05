# reqwest parity

The features an application expects from a reqwest-level client, so euripos can replace it (#8). Each section cites the RFC its behaviour follows. None of them is adaptive: each behaves the same on every network, so none departs from the baseline in a way Principle 3 of [docs/scope.md](../scope.md) gates, and the baseline's plain request path is unchanged when a caller uses none of them. The public API they add is recorded in [api.md](api.md) and in the snapshot.

Every feature here is in the public API on every platform and in every feature set: `cfg(feature)` on a public item is the same violation as `cfg(target_os)` (CONTRIBUTING.md, "Feature flags"), and `mise run api:cfg` renders each feature set to prove it. A Cargo feature therefore only ever adds behaviour behind an API that is always there.

## Streaming bodies

### Problem

A request body that is produced while it is sent (a file, a generated stream, another response) must not have to sit in memory whole, and a response body has to be readable piece by piece or whole without the caller polling `http_body::Body` by hand.

### Constraints

- `Body` stays `Send + Sync + Unpin` and not `UnwindSafe` (api.md, "Requests, responses and bodies").
- No second runtime or stream abstraction: `http_body::Body` is already a public dependency; `futures`' `Stream` is not.
- RFC 9112 §6: a body whose length is known is sent with `Content-Length`; otherwise HTTP/1.1 sends it chunked (§7.1).

### Alternatives

1. `Body::wrap(impl http_body::Body<Data = Bytes>)`, the constructor api.md anticipated, plus `chunk`, `bytes` and `text` readers on `Body`.
2. `Body::from_stream(impl Stream<Item = Result<Bytes, E>>)`, as reqwest does.
3. Accept any `http_body::Body` in `Client::send`.

### Choice

1. `Body::wrap<B>(B) -> Body` for `B: http_body::Body<Data = Bytes> + Send + Sync + 'static` whose error converts into a boxed error. `Body::chunk(&mut self) -> Result<Option<Bytes>, Error>`, `Body::bytes(self) -> Result<Bytes, Error>` and `Body::text(self) -> Result<String, Error>`. Two error kinds are added: `ErrorKind::Body` when a wrapped source fails (its error is the `source()`), and `ErrorKind::Decode` when a body is not what the caller asked for (`text` on bytes that are not UTF-8).

### Rationale

`http_body::Body` is the trait hyper and tower already use, so any of their bodies wraps directly, and a `Stream` adapts through `http_body_util::StreamBody` without euripos depending on `futures` (option 2). The `Sync` bound keeps `Body: Sync` without an unsafe wrapper; a source that is not `Sync` can be wrapped in a mutex or a channel. Option 3 is the alternative api.md already rejected. A wrapped body's size hint is passed through, so an exact one becomes `Content-Length` and an unknown one is sent chunked, which is hyper's behaviour (RFC 9112 §6.3). `text` decodes UTF-8 only: charset sniffing from `Content-Type` is an encoding dependency with no request behind it. A failure of the caller's source is not a protocol error, so it gets its own kind with the source preserved. `bytes` holds the body in memory, bounded in time by the total timeout and not in size, as reqwest's is.

### Validated by

`tests/parity/body.rs`: a wrapped body of unknown length arrives chunked and of exact length with `Content-Length`; a failing source fails the request as `ErrorKind::Body` with its error in the chain; `chunk` yields pieces then `None`; `text` on non-UTF-8 fails as `ErrorKind::Decode`.
