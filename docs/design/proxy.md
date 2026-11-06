# Proxies

Requests through an HTTP proxy, the credentials presented to it, and where the proxy comes from (#49). The last item of the reqwest-parity list ([parity.md](parity.md)), "Proxy authentication", needs a proxy path to authenticate on, so both land here. None of it is adaptive: a proxied request behaves the same on every network, and a client with no proxy configured sends exactly what the baseline sends.

## Proxy path

### Problem

A client on a network that reaches the internet only through an HTTP proxy cannot send anything: the connector resolves and connects to every origin directly. docs/scope.md, "Platform integration", gives each platform a proxy source, and `NetworkFacts::proxy()` reports whether one is configured, but nothing routed through it.

### Constraints

- RFC 9112 §3.2.2: a request to a proxy carries the absolute URI as its target, and `Host` still names the origin (§3.2).
- RFC 9110 §9.3.6: an `https` request is tunnelled with `CONNECT host:port`, whose `2xx` answer turns the connection into a byte stream to the origin; TLS and ALPN then run end to end with the origin, so HTTP/2 works through the tunnel.
- The pool key is the origin ([baseline.md](../baseline.md), "Connection pool"); a connection to a proxy, or a tunnel through one, must never serve a request it was not opened for.
- One implementation per platform; platform code stays in its module behind `cfg(target_os)`, and the rest branches on a capability (CONTRIBUTING.md, "Code layout").
- No `cfg` on a public item, and the four timeouts stay the only bounds.
- The Linux proxy source is the environment (docs/scope.md). The scope asks for a live system proxy, not one read once when the client is built (reqwest #2674).

### Alternatives

1. hyper-util's legacy `Client` with its `Intercept`/proxy connector, as reqwest does.
2. A proxy step in euripos's own connector: connect to the proxy instead of the origin, then for `https` send `CONNECT` through hyper's HTTP/1.1 client connection and take the upgraded stream; `http` requests sent in absolute form over a connection to the proxy.
3. The same, with a hand-written `CONNECT` exchange on the raw socket.

### Choice

2. `Dst`, the pool key, gains the proxy (`Option<Hop>`: host, port and the `Proxy-Authorization` value), so a proxied origin never shares a connection with the direct one or with the same origin through other credentials. The connector connects to the proxy; for `https` it sends `CONNECT` with hyper's `http1` client connection `with_upgrades`, and runs the TLS handshake with the origin's name over `hyper::upgrade::Upgraded`. For `http`, the HTTP/1.1 sender keeps the absolute URI as the target (user information removed), sets `Host` and adds the proxy's `Proxy-Authorization`. The connect timeout covers the TCP connection to the proxy and the `CONNECT` exchange; the TLS timeout the handshake through the tunnel.

`ClientBuilder::proxy(Proxy)` sends every request through one proxy; `ClientBuilder::no_proxy()` sends every request directly. Without either, each request asks the platform when it is sent ("Platform proxy" below). On Linux the platform's configuration is the environment: `https_proxy` or `http_proxy` by the request's scheme, then `all_proxy`, lowercase before uppercase, unless `no_proxy` names the host (`*`, a name and the hosts under it, an IP address, or `ADDRESS/BITS`). `HTTP_PROXY` is ignored when `REQUEST_METHOD` is set (CVE-2016-5385, "httpoxy"). A value without a scheme is an `http` proxy.

### Rationale

Option 1 replaces the pool this crate composes from hyper-util's `client::pool` layers (#37) and its per-platform connector, so it would be a second connection path rather than a step in the one there is. Option 3 re-implements HTTP/1.1 response parsing, which hyper already does, including keeping bytes the proxy sent after its response head. Keying the pool by the proxy keeps the invariant that one key is one route; per-origin keys for `http` through a proxy cost a connection per origin where one per proxy would do, which is the plain behaviour and needs no evidence.

Reading the environment for each request costs a few `getenv` calls and makes a change to it take effect at the next request, which is the scope's live proxy. A proxy variable that is set but unusable (not UTF-8, an `https` or SOCKS proxy, no host) fails the request as `ErrorKind::Connect` rather than sending it around the proxy the user configured, as curl does. The order of variables, the lowercase preference and the httpoxy rule are curl's and reqwest's, so a Linux user's existing configuration means the same thing here.

A proxy reached over TLS (`https://` proxy URI) and SOCKS are not supported; neither is in the scope, and each fails as `ErrorKind::Connect` rather than being ignored.

### Validated by

`crates/euripos/tests/proxy/`: through a loopback proxy the test starts, an `http` request arrives in absolute form with `Host`, and an `https` request (in `client.rs`'s TLS tests, which trust a test certificate) arrives at the origin through a `CONNECT` tunnel the proxy saw, over HTTP/2 when the origin offers it; `no_proxy()` bypasses the proxy; a proxy that refuses the tunnel fails as `ErrorKind::Connect`; on Linux, a redirect from a host the environment's proxy serves to one `no_proxy` names reaches its target directly, so each hop chooses its own route. Unit tests in `src/platform/linux/proxy.rs` cover the environment's variable order, `no_proxy` matching and the httpoxy rule; `src/pool.rs` covers the pool key and the absolute form.

## Platform proxy

### Problem

Without a caller's proxy, a request should take the route the platform's own HTTP stack would give it (docs/scope.md, "Platform integration"): the environment on Linux, `LinkProperties`' proxy on Android, the WinHTTP proxy with PAC and WPAD on Windows, and the system proxy on macOS and iOS. Each source was read for `NetworkFacts::proxy()`, and Android's and Windows' could already name a proxy for a host, but only Linux's routed anything (#63). A PAC script answers with an ordered list ("`PROXY a:3128; PROXY b:3128; DIRECT`"), which a browser walks until one entry connects.

### Constraints

- One implementation per platform, in its module; the proxy path calls one capability, `platform::proxy_routes`, and never branches on `cfg`.
- The proxy is read for each request, so a change applies from the next one (reqwest #2674); no settings are cached across requests by the client.
- WinHTTP's `WinHttpGetProxyForUrl` and CFNetwork's PAC execution block while a script is fetched and run, and must not hold up a runtime worker.
- The four timeouts stay the only bounds on a request.
- The pool key stays one route per key ("Proxy path").

### Alternatives

1. Each platform returns the ordered list of routes for the request's origin, resolved off the runtime where it blocks; the pool tries them in order, moving on when one cannot be connected.
2. Each platform returns one route, the list's first; a dead first proxy fails the request.
3. Leave the sources unread and document `ClientBuilder::proxy` as the way to use the platform's proxy.

### Choice

1. `platform::proxy_routes(tls, host, port)` returns the routes, each a `Hop` or `None` for the origin directly; `proxy::platform_routes` turns a platform's list of `host[:port]` entries into them, skipping an entry that names no `http` proxy (SOCKS, a TLS proxy) and failing as `ErrorKind::Connect` where nothing else is left, as an unusable environment proxy does. `Pool::send` takes the routes and checks out a connection by the first; one that fails to connect (`ErrorKind::Connect`, or the connect timeout) gives way to the next, each with its own connect timeout, while a TLS failure or a `407` is reported as it is. The lookup runs inside the total timeout.

- **Linux**: the environment, as above, moved into `platform/linux/proxy.rs`.
- **Android**: the default network's `getDefaultProxy()` as the callback last reported it: the fixed proxy unless one of its exclusions matches the host, or the platform's local PAC proxy (`localhost:port`) once it listens, which runs the script itself; direct where no callback runs. One proxy serves `http` and `https`.
- **Windows**, on a blocking thread: the current user's WinHTTP settings, or the machine's default where the user has none. An automatic configuration (WPAD, a PAC URL, or both) is resolved for the request's origin through the process's one `Resolver` (`docs/design/network-context.md`), which discards its cache when the network changes; its whole list is returned. Where it gives none (discovery finds no script, or the script cannot be fetched or fails), the fixed proxy applies: the list's `https=` or `http=` entry for the request's scheme, or else its entry with no scheme, unless the bypass list names the host. A bypass entry is a pattern in which `*` matches any run of characters, with an optional scheme before it and port after it; `<local>` names a host without a dot; loopback (`localhost`, `127.0.0.0/8`, `::1`) is bypassed unless the list holds `<-loopback>`.
- **macOS and iOS**, on a blocking thread: `CFNetworkCopySystemProxySettings`, applied to the request's origin by `CFNetworkCopyProxiesForURL`, which applies the exceptions and picks the `http` or `https` proxy. A PAC entry, by URL or by script, is run with `CFNetworkExecuteProxyAutoConfigurationURL` or `…Script` on that thread's run loop and replaced by the script's list; a script that cannot be fetched or run, or one that has not answered after the default total timeout, gives nothing and the request goes direct. Network.framework keeps `nw_parameters_set_prefer_no_proxy` ([ADR 0003](../adr/0003-apple-network-framework.md)): the client connects to the proxy it chose, and the framework must not proxy that connection again. `NetworkFacts::proxy()` stays `Unknown` here: the settings send no change signal a path reading could follow.

The script is asked about the origin (`scheme://host:port/`), never the path or query.

### Rationale

Option 1 is what a PAC list means and what the browsers and WinHTTP's own clients do with it; option 2 turns a dead first proxy, the case the list exists for, into a failed request. Option 3 leaves the scope's "Live system proxy, PAC/WPAD on every platform" undone and asks every caller to reimplement it. Trying the next route only after a failure to connect keeps a request from being sent twice: nothing has been written when a checkout fails. Resolving the list for the origin rather than the full URL keeps one list per origin, matches what Chrome passes for `https`, and keeps paths and queries out of a script fetched from whoever answered WPAD.

Falling back from an automatic configuration that gives nothing to the fixed proxy is Windows' own behaviour, and matters because "Automatically detect settings" is on by default: a user who also sets a fixed proxy expects it to apply where discovery finds no script. The implicit loopback bypass is Windows' too. On Apple, CFNetwork already implements the settings' semantics (exceptions, simple hostnames, per-scheme proxies), so the client asks it rather than reading the dictionary's keys itself. Each blocking lookup runs on tokio's blocking pool, and its result arrives within the request's total timeout; the run-loop wait on Apple is bounded so that a script that never answers does not hold a blocking thread for ever.

### Validated by

`src/pool.rs`: a route that cannot connect gives way to the next proxy and to the origin directly; the last one's failure fails the request as `ErrorKind::Connect`; only a failure to connect fails over. `src/proxy.rs`: a platform list keeps its order and skips what is no `http` proxy; a value without a scheme is an `http` proxy; the wildcard patterns. `src/platform/android/mod.rs`: reported settings become the route, including an excluded host and an IPv6 proxy, and without an Android `Context` the route is direct (on the emulator). `src/platform/windows/proxy.rs`: the per-scheme entry of a fixed proxy list, every form of bypass entry and the loopback rule, the user-to-machine fallback, a PAC script served on loopback resolved per URL through WinHTTP, and the hosted runner's settings giving a route. `src/platform/apple/proxy.rs`: a PAC script run by CFNetwork, inline and fetched from loopback, read back in its order with `DIRECT` and SOCKS entries; a script that cannot be fetched gives nothing; the hosted runner's settings give a route.

Not covered on hosted CI: a real WPAD discovery, a Windows user's fixed proxy and bypass list read from the registry, a macOS or iOS system proxy configured in Settings, and Android's `getDefaultProxy()` from a running app; each needs a configured device, and the parsing and selection each feeds is tested from its inputs above.

## Proxy authentication

### Problem

A proxy that requires credentials answers `407 Proxy Authentication Required` (RFC 9110 §15.5.8) until the client sends `Proxy-Authorization`. The client needs a way to be given credentials, to send them on every proxied request and `CONNECT`, and to tell the caller when the proxy refused them.

### Constraints

- RFC 7617: Basic credentials are `base64(user ":" password)`, the user-id containing no `:`, in UTF-8.
- RFC 9110 §11.7.2: `Proxy-Authorization` is for the next proxy; it must not reach an origin that did not ask for it. Redirects already drop it when the origin changes (`redirect::strip`).
- Errors are classified by `ErrorKind`, `#[non_exhaustive]`, so a new kind is a minor change.
- Credentials must not appear in `Debug` output or logs.

### Alternatives

1. Basic only, from the proxy URI's user information or `Proxy::basic_auth`, and a `407` reported as its own `ErrorKind::ProxyAuthRequired`.
2. Also challenge-driven schemes (Digest, NTLM, Negotiate), retrying the request after the `407` with the scheme the proxy offered.
3. Report a `407` as `ErrorKind::Connect`, or return it as a response.

### Choice

1. `Proxy::new(uri)` takes `http://[user:password@]host[:port]`, the user information percent-decoded into Basic credentials; `Proxy::basic_auth(user, password)` replaces them. The header value is marked sensitive and `Debug` prints only whether credentials are set. It is sent on every `CONNECT` and every request sent to the proxy in absolute form, unless the caller's request sets its own `Proxy-Authorization` (absolute form only). A `407` to a `CONNECT`, or to a request sent to the proxy in absolute form, fails the request as `ErrorKind::ProxyAuthRequired`, and its connection is closed; a `407` from a direct connection is an origin's response and is returned as one.

### Rationale

Basic is what the URI form and every proxy environment variable can carry, and what the issue asks for. A challenge-driven scheme (option 2) needs a second round trip and per-scheme state, NTLM and Negotiate need platform SSPI/GSSAPI, and none has a request behind it; retrying with credentials the caller did not give would be guessing (#49). A distinct kind (option 1 over 3) lets a caller tell "fix your credentials" from "the proxy is down" without reading statuses: a `407` from the proxy is not the origin's answer, so returning it as a response would make the caller parse the proxy's error page as the resource. A `Proxy-Authorization` the caller set on an absolute-form request wins over the configured credentials: it is the only way to send a scheme this client does not compute (a token, or Digest or NTLM worked out by the caller), and replacing it would send credentials the caller chose not to. On a `CONNECT` the client builds the request itself, so the configured credentials are the only ones.

### Validated by

`crates/euripos/tests/proxy/`: credentials from the URI and from `basic_auth` arrive as `Proxy-Authorization: Basic …` on the absolute-form request and on the `CONNECT`; a proxy answering `407` fails the request as `ErrorKind::ProxyAuthRequired`, both for `http` and for the `https` tunnel. Unit tests in `src/proxy.rs` check the encoding against RFC 7617's example and that `Debug` prints no credentials; one in `src/pool.rs` checks that `absolute_form` keeps a caller-set `Proxy-Authorization` and adds the configured one otherwise.
