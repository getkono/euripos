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

## JSON

### Problem

Most API traffic is JSON: a caller should send a value as a JSON body and read a response body into a value without serializing by hand or setting the media type itself.

### Constraints

- RFC 8259: JSON exchanged between systems is UTF-8 (§8.1), and its media type is `application/json` with no `charset` parameter (§11).
- No `cfg(feature)` on a public item, so a JSON method cannot exist only behind a feature.
- serde is the ecosystem's serialization trait; its 1.x is stable (C-STABLE).

### Alternatives

1. `Body::from_json(&T) -> Result<Body, Error>` and `Body::json::<T>(self)`, always present, with serde and serde_json as dependencies of every build.
2. The same methods behind a `json` feature, as reqwest does.
3. The methods always present but returning an error unless a `json` feature is on.
4. No JSON helpers: callers serialize into `Vec<u8>` and parse `bytes()` themselves.

### Choice

1. `Body::from_json<T: Serialize + ?Sized>(&T) -> Result<Body, Error>` serializes with serde_json and fails as `ErrorKind::Body` when the value cannot be serialized; a request sent with that body carries `Content-Type: application/json` unless the request sets its own. `Body::json<T: DeserializeOwned>(self) -> Result<T, Error>` reads the body to its end and fails as `ErrorKind::Decode` when it is not JSON that fits `T`. serde (with only `std`) becomes a public dependency.

### Rationale

Option 2 is the `cfg(feature)` on a public item that the API gates forbid and `api:cfg` rejects; option 3 turns a build-time choice into a runtime failure a caller can only find by sending a request. The cost of option 1 is compile time: serde_json's code is generic, so a binary that never calls these methods links none of it, and the `size` job's minimal client is unchanged. The media type travels with the body rather than through a request builder: `Body` is what knows what it holds, and a caller who sets `Content-Type` (for example `application/vnd.api+json`) keeps it. `json` ignores the response's `Content-Type`, because servers mislabel JSON often enough that refusing it would only push callers back to parsing by hand.

### Validated by

`json::tests` (the media type, a map with non-string keys failing as `Body`, a mismatched shape failing as `Decode`); `tests/parity/json.rs`: a JSON body arrives as `application/json` and the response is read back into a value, and a caller's own `Content-Type` is kept.

## Multipart forms

### Problem

File uploads and HTML-style form posts are `multipart/form-data` (RFC 7578): named parts separated by a boundary, each with its own disposition and media type. Building one by hand means choosing a boundary, escaping names and getting the framing right.

### Constraints

- RFC 7578: each part has `Content-Disposition: form-data; name="..."` and optionally a `filename` (§4.2) and a `Content-Type`, `text/plain` when absent (§4.4); names and file names may be UTF-8 (§5.1).
- RFC 2046 §5.1.1: the boundary is at most 70 characters and must not occur in any part.
- A part may be large: it streams, like any `Body`.
- No `cfg(feature)` on a public item (see "JSON").

### Alternatives

1. `Form` and `Part` built in-house over `Body`: no dependency, the parts chained into one streaming body.
2. A multipart crate (`multer` parses; `common-multipart-rfc7578` and `mpart-async` build) behind a `multipart` feature.
3. No helper: callers frame the body themselves.

### Choice

1. `Form::new()`, `Form::text(name, value)`, `Form::part(name, Part)` and `Form::boundary()`; `Part::new(impl Into<Body>)`, `Part::file_name(name)` and `Part::content_type(HeaderValue)`; `impl From<Form> for Body`. The body is the parts in order, each head then its body, sent as one stream whose size is exact when every part's is (so HTTP/1.1 sends `Content-Length`), and the request carries `Content-Type: multipart/form-data; boundary=...` unless it sets its own. The boundary is `euripos-` and 32 hexadecimal digits drawn from the standard library's randomly keyed hasher. `"`, CR and LF in a name or file name are percent-encoded, as HTML's form submission does and RFC 7578 §2 allows; other characters, including non-ASCII ones, are sent as UTF-8.

### Rationale

The encoder is about a hundred lines, so a dependency would cost more API, licence and graph review than it saves, and option 2's feature gate is the `cfg(feature)` the API gates reject. A boundary needs to be unguessable to an attacker who controls a part's content, not cryptographically strong: 128 bits from `RandomState`, which the standard library keys from the operating system's randomness, does that without a `rand` or `getrandom` dependency. Checking parts for the boundary (RFC 2046's "must not occur") is impossible for a streaming part without buffering it, so the random boundary is what meets it, as in every streaming encoder. Percent-encoding keeps a name from breaking out of its quoted-string, which is the injection the framing has to prevent.

### Validated by

`multipart::tests` (exact framing and size, escaping, a fresh valid boundary per form, a streaming part leaving the size unknown); `tests/parity/multipart.rs`: a form with a text field and a file part arrives byte for byte with its media type and boundary.

## Redirects

### Problem

Servers move resources and answer POSTs with `303 See Other`. A plain hyper client returns every `3xx` to the caller, who then has to resolve `Location`, pick the method, decide what to resend and avoid leaking credentials to another origin.

### Constraints

- RFC 9110 §15.4: a client may follow `301`, `302`, `303`, `307` and `308` automatically. `303` is fetched with `GET` (§15.4.4); `307` and `308` keep the method and body (§15.4.8, §15.4.9); `301` and `302` may turn `POST` into `GET` (§15.4.2, §15.4.3). When it follows, it should remove headers it generated, consider removing `Authorization` and `Cookie`, and remove headers the method change invalidates.
- RFC 3986 §5: `Location` is a URI reference resolved against the request's URI.
- A streaming request body cannot be sent twice.
- The total timeout bounds the whole request, so it bounds the whole chain.

### Alternatives

1. Follow by default, up to `ClientBuilder::max_redirects(u32)` (default 10, `0` to follow none), with `ErrorKind::Redirect` past the limit.
2. A policy type (`Policy::none()`, `Policy::limited(n)`, `Policy::custom(closure)`), as reqwest has.
3. Never follow: leave redirects to the caller, as hyper does.

### Choice

1. `301`, `302`, `303`, `307` and `308` with an `http` or `https` `Location` are followed, `Location` resolved by RFC 3986 §5.2 (its fragment dropped, since the request URI has none). `303`, and `301` or `302` to a `POST`, become a `GET` (`HEAD` stays `HEAD` on `303`) with no body and without the `Content-*` and `Transfer-Encoding` headers; the others resend the method and body. A body held in memory (bytes, JSON, a form of in-memory parts) is copied before each hop that may need it; a streaming one cannot be, so the `307` or `308` that would need it is returned as the response. When the scheme, host or port changes, `Authorization`, `Proxy-Authorization`, `Cookie` and `Host` are dropped. `300`, `304`, `305`, `306`, a missing `Location`, and one that is not `http` or `https` are returned as the response. More than `max_redirects` hops fail as `ErrorKind::Redirect`; `max_redirects(0)` returns the first redirect as it is. Each hop's response body is dropped unread, and the whole chain runs under the one total timeout.

### Rationale

Following is what reqwest, curl's `-L` and every browser do, so a caller replacing reqwest expects it; the limit of 10 is reqwest's default and bounds a redirect loop. One integer covers reqwest's `none` and `limited` policies; a custom policy (option 2) is a public trait or closure type to keep stable, and nobody has asked for it. The `ClientBuilder` gains the knob because the right limit is the caller's (an API that never redirects wants `0`); api.md's Configuration table records it. Dropping credentials on an origin change is the safe reading of §15.4's "consider removing": a token for one origin must not reach another, and a cookie the store generated is regenerated for the new origin ("Cookies"). Downgrading `https` to `http` is followed, as reqwest and curl do, with credentials dropped since the origin changed. Dropping an intermediate body unread closes an HTTP/1.1 connection rather than reusing it; redirect bodies are small and rare enough that draining them is not worth a new wait. A non-ASCII `Location` is returned rather than percent-encoded: no server in the tests sends one, and RFC 9110 requires ASCII.

### Validated by

`redirect::tests` (every RFC 3986 §5.4 example an HTTP URI can meet, the method rule per status, unfollowed statuses and locations, credential and body-header stripping); `tests/parity/redirect.rs`: `303` and `302`-to-`POST` arrive as a bodiless `GET`, `307` and `308` resend method and body, a `307` with a streaming body is returned, credentials do not reach another origin, a chain past the limit fails as `ErrorKind::Redirect`, and `max_redirects(0)` returns the redirect.

## Cookies

### Problem

Sessions behind a login keep their state in cookies. A caller replacing reqwest's `cookie_store(true)` needs the client to keep what servers set and send it back, including across the redirect a login usually answers with.

### Constraints

- RFC 6265: parsing `Set-Cookie` (§5.2), dates by the algorithm of §5.1.1, domain and path matching (§5.1.3, §5.1.4), the storage model (§5.3) and the `Cookie` header (§5.4). §6.1 asks a store to hold at least 4096 bytes per cookie, 50 per domain and 3000 in all.
- A store must not let one site set cookies for another, or for a public suffix (§5.3 step 5, which defers to a public suffix list).
- No `cfg(feature)` on a public item (see "JSON").
- A plain client sends no cookie it was not given, and the baseline keeps that default.

### Alternatives

1. `ClientBuilder::cookie_store(bool)`, off by default, with an in-house RFC 6265 store shared by the client's clones.
2. The `cookie_store` crate behind a `cookies` feature, as reqwest does, with a public `CookieStore` trait for custom stores.
3. A public `Jar` type the caller can seed, read and share between clients.

### Choice

1. `ClientBuilder::cookie_store(bool)`, `false` by default. When on, every response's `Set-Cookie` headers, redirects included, are stored by §5.3, and each request (each hop of a redirect chain) carries the §5.4 `Cookie` header for its URI unless the caller set `Cookie` itself. The store is in memory, lives as long as the client and its clones, and holds at most 50 cookies a domain and 3000 in all, evicting the oldest, and refuses a cookie whose name and value exceed 4096 bytes. Dates parse by §5.1.1, which accepts IMF-fixdate, RFC 850, asctime and the dashed Netscape form; `Max-Age` wins over `Expires`; an expired cookie deletes its stored namesake. `HttpOnly` and `SameSite` are parsed and ignored: they govern scripts and sites, and a client has neither. A `Domain` must domain-match the host that set it, and a single-label one (`com`) is refused.

### Rationale

The store is the RFC's algorithms and a mutex, a few hundred lines with tests, against `cookie_store`'s graph (`url`, `idna`, `publicsuffix`, `time`, `cookie`), most of which is a public suffix list euripos would have to keep current. Without that list, a cookie for a multi-label public suffix such as `co.uk` is accepted, which the builder's documentation says; the single-label check stops the common case, and a client talks to the origins its caller chose, not to arbitrary sites as a browser does, so the cross-site leak a public suffix check prevents needs a hostile site the caller already trusts. A public store (options 2 and 3) is API to keep for features nobody has asked for yet; it can be added beside the switch without breaking it. Off by default keeps the baseline's behaviour and matches reqwest.

### Validated by

`cookies::tests` (every date format to the same instant and invalid dates refused, `Max-Age` precedence and deletion, ignored pairs, domain and path matching and default paths, host-only cookies, domain cookies kept from other and single-label domains, `Secure` and the §5.4 ordering, replacement and expiry, the per-domain limit); `client::tests::the_cookie_store_is_off_by_default_and_shared_by_clones`; `tests/parity/cookies.rs`: a cookie set on a `303` reaches the next hop and a clone's later request, no cookie is kept without the store, and a caller's `Cookie` is sent alone.
