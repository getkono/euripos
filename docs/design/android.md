# Android

The Android integration: how euripos reaches the JVM it runs in, the monitor that follows the default network, sockets bound to that network, its proxy, the platform verifier's initialisation, and the end of Doze. The facts it reports are the model of [network-context.md](network-context.md); the API floor is API 29.

## Embedding

### Problem

Everything Android reports about the network, and the trust store rustls-platform-verifier checks certificates against, is reached through Java. Native code can call Java only with the process's `JavaVM` and the application's `Context`, which an app holds and a library does not. reqwest leaves this to an undocumented initialisation call ([reqwest#2966](https://github.com/seanmonstar/reqwest/issues/2966)); scope ("Why not reqwest") asks for no platform-specific call in the public API.

### Constraints

- No `cfg` and no Android type on a public item (Principle 1).
- Scope names `ndk-context` as where the JVM comes from ("Platform integration").
- Callbacks need a Java class: `ConnectivityManager.NetworkCallback` is an abstract class, which JNI cannot subclass and `java.lang.reflect.Proxy` cannot implement.
- API 29 is the floor: per-network DNS (#14, `android_res_nquery`) and `onBlockedStatusChanged` need it.

### Alternatives

For the JVM and `Context`:
1. `ndk-context`, which `android-activity` and other app glue initialise, and an app embedding Rust through its own JNI initialises with one call.
2. A public `euripos::android::init(env, context)`.
3. `JNI_GetCreatedJavaVMs` and `ActivityThread.currentApplication()` by reflection.

For the callback's Java:
1. A dex compiled from Java in this repository, embedded in the library and loaded with an `InMemoryDexClassLoader` (API 26).
2. An Android library (AAR) the app adds to its Gradle build.
3. Compile the Java in `build.rs`.
4. Poll `ConnectivityManager` from native code instead of registering a callback.

### Choice

Option 1 in each. The JVM and `Context` are read from `ndk-context` on first use: by `Client::network_facts`, which starts the monitor, and by the first `https` request, which initialises the verifier. `ndk-context` panics when asked before the app initialised it, and has no way to ask without; the panic is caught, so an app that never initialises it gets `Unknown` facts and an `ErrorKind::Tls` error. Built with `panic = "abort"`, the same app aborts with `android context was not initialized`.

`NetworkMonitor.java` beside the platform module is compiled by `mise run android:dex` into `monitor.dex` (about 5 KB), which the library includes. The monitor loads it with an `InMemoryDexClassLoader` whose parent is the app's class loader, binds the class's one native method with `RegisterNatives`, and calls its `start`. The build pins the JDK, the SDK platform and the build-tools, and is reproducible; CI's `android` job rebuilds the dex and fails when the committed one differs from it.

What the embedding app provides:
- `ndk_context::initialize_android_context(vm, context)` before the first request, with the process's `JavaVM` and a global reference to its `Context`. Apps built on `android-activity` already have it.
- `android.permission.INTERNET` and `android.permission.ACCESS_NETWORK_STATE` in its manifest. Without the second the monitor does not start and the facts are `Unknown`.
- rustls-platform-verifier's Kotlin component in its Gradle build, as [that crate's Android section](https://github.com/rustls/rustls-platform-verifier#android) describes. Without it, certificate verification fails as `ErrorKind::Tls`.

### Rationale

`ndk-context` is the convention Rust on Android already shares, so an app that uses another crate needing the JVM initialises it once for both, and the public API stays the same on every platform (option 2 would put an Android-only function on it). Reflection on `ActivityThread` (option 3) is hidden API that newer releases restrict.

The embedded dex (option 1) is the only way that needs nothing from the app's build: an AAR (option 2) is a second artifact to version alongside the crate, and `build.rs` (option 3) would need a JDK and the Android SDK on every machine that builds an app, not only on the one that changes the Java. Committing the dex costs a binary file in review, which the reproducibility check in CI answers. Polling (option 4) wakes the process periodically, which scope rules out on metered and constrained paths, and still misses changes between polls.

The rustls-platform-verifier component remains the app's: it is that crate's Kotlin, versioned with it, and folding it into this dex would fork it.

### Validated by

`platform::android::tests::without_an_android_context_nothing_is_observed_or_verified` on the emulator, where the test binaries run outside any app; the `android` CI job's dex check. The JNI path itself runs only inside an app: #55.

## Android monitor

### Problem

The monitor ([network-context.md](network-context.md), "Change monitor") needs readings of the network traffic leaves through and a signal when it may have changed. On Android an app cannot read routes or interfaces itself: netlink route sockets are closed to apps targeting Android 11 and later, and the system decides which network an app's traffic uses.

### Constraints

- One implementation on Android (Principle 2); scope names `ConnectivityManager.NetworkCallback` over JNI with `NOT_METERED` and `TEMPORARILY_NOT_METERED`.
- The same identity and facts as on the other platforms, every fact `Unknown` until the platform reports it (Principle 4).
- Callbacks arrive on the platform's connectivity thread, broadcasts on the main thread; the monitor reads on its own.

### Alternatives

For which network:
1. `registerDefaultNetworkCallback`: the network the app's traffic uses, VPN included.
2. `registerNetworkCallback` for every network with `NET_CAPABILITY_INTERNET`, choosing among them.

For what crosses JNI:
1. The Java side flattens each callback into primitives (a handle, a transport bitmask, flag bits, the proxy's strings) and calls one native method; the mapping to facts is Rust.
2. The Java side passes the `Network`, `NetworkCapabilities` and `LinkProperties` objects, and Rust calls their getters.

For the identity:
1. `Network.getNetworkHandle()`.
2. The interface and gateway from `LinkProperties`, as the route key is on Linux.

For the constraint:
1. Data Saver: `getRestrictBackgroundStatus()` `ENABLED` on a metered network is `Constrained`.
2. `Unknown` always.

### Choice

Option 1 in each. The callback keeps the default network's last `NetworkCapabilities`, `LinkProperties` and blocked status, forgets them when another network becomes the default or the network is lost, and reports after every callback; a receiver for `ACTION_RESTRICT_BACKGROUND_CHANGED` and `PROXY_CHANGE_ACTION` reports too, as Data Saver and a global proxy change without a callback. Each report replaces the last one and wakes the monitor. Opening the source waits up to 250 ms for the first report, as on Linux.

From a report:
- `network()`: the handle, hashed; `None` with no default network.
- `interface()`: a VPN is `Other`, as a tunnel is on Linux and Windows; otherwise `TRANSPORT_WIFI` is Wi-Fi, `TRANSPORT_CELLULAR` cellular, `TRANSPORT_ETHERNET` Ethernet, any other transport `Other`.
- `metering()`: `NOT_METERED` is unmetered, `TEMPORARILY_NOT_METERED` (API 30) temporarily unmetered, and neither metered, as `isActiveNetworkMetered` reads it.
- `constraint()`: `Constrained` where Data Saver is on for the app and the network is metered; `Unconstrained` otherwise.
- `ipv4()` and `ipv6()`: `Available` where `LinkProperties` has a default route of that version, `Unavailable` where it has none.
- `proxy()`: as "Proxy" below.

Until the capabilities or the link properties arrive, the facts they give are `Unknown`; with no default network, both IP versions are `Unavailable` and the rest `Unknown`.

### Rationale

The default network is the one sockets use unless bound, so it is the one the pool's connections are on; choosing among all networks (option 2) re-implements the system's ranking and would disagree with it. Flattening in Java (option 1) keeps JNI to one call per change with primitive arguments and keeps the mapping in Rust, where it is tested without a JVM; option 2 would make the monitor's thread call back into Java on every reading.

The network handle names exactly one network for as long as it exists and a new one when the system brings a network up again, which is what the pool keys on; it carries no address or SSID. A route key (option 2) would merge two networks that hand out the same gateway, which the handle tells apart.

Data Saver is the Android form of the user asking apps to limit their traffic, which is what `Constrained` means on every platform, and it applies only to metered networks. Reporting `Unknown` (option 2) would discard a fact the platform states.

### Validated by

On the emulator: `platform::android::tests::no_default_network_routes_nothing_and_names_nothing`, `a_network_is_named_by_its_handle`, `facts_not_yet_reported_are_unknown`, `transports_map_to_interface_kinds`, `capabilities_map_to_metering`, `data_saver_constrains_metered_networks_only`, `default_routes_map_to_ip_availability`, `a_blocked_network_is_reported_blocked`, `the_proxy_fact_follows_the_reported_settings`. Reports from a device's real callback: #55.

## Per-network sockets

### Problem

A connection belongs to the network it was made on (scope, "Why not reqwest"). An unbound socket takes whichever network is the default at the moment it connects, which may already differ from the network the client read; reqwest offers no binding at all ([reqwest#2571](https://github.com/seanmonstar/reqwest/issues/2571)).

### Constraints

- Scope names `android_setsocknetwork` on every socket before it connects.
- The other socket platforms connect as before.

### Alternatives

1. Bind each socket to the handle of the network the callback last reported; leave it unbound where none is reported.
2. Bind the process with `bindProcessToNetwork`.
3. Leave sockets unbound.

### Choice

Option 1. The socket connector creates each socket, asks the platform to bind it, then connects; only Android binds. A binding the platform refuses (the network was lost since it was reported) fails that address's attempt, and so the connect as `ErrorKind::Connect` once no address is left. Names are resolved by the system resolver on the process's default network; per-network resolution is #14.

### Rationale

Binding the socket makes the network it uses the one the facts named when it was made, which the pool (#6) keys on. Binding the process (option 2) would change the network every other library in the app uses. A refused binding is reported rather than retried unbound, because an unbound socket would land on a network the client has not yet read. Until a caller starts the monitor (`Client::network_facts`, and the pool once #6 lands) nothing is reported, and sockets go out unbound, as before.

### Validated by

On the emulator: `platform::android::tests::sockets_bind_to_a_network_by_handle` (the unspecified handle unbinds, a value that is not a handle and a handle naming no network are refused, the bound socket connects) and `an_unreported_network_leaves_sockets_unbound`; the socket platforms' connect tests on Linux and Windows.

## Proxy

### Problem

The proxy the platform's own HTTP stack uses on a network, including a PAC script, should be the one the client uses (#49), and should be re-read when it changes rather than once ([reqwest#2674](https://github.com/seanmonstar/reqwest/issues/2674)).

### Constraints

- Scope names the `LinkProperties` proxy.
- A PAC script must be honoured; the floor is API 29.

### Alternatives

For the source:
1. `ConnectivityManager.getDefaultProxy()`: a global proxy where a device policy set one, else the default network's `LinkProperties` proxy.
2. `LinkProperties.getHttpProxy()` alone.

For a PAC script:
1. Send requests to the platform's local PAC proxy (`localhost` at the port `ProxyInfo` reports), which runs the script per request.
2. Evaluate the script in process.
3. `PacProxyManager`.

### Choice

Option 1 in each. `proxy()` is `Automatic` with a PAC URL, `Manual` with a fixed host, and `Direct` with neither. An internal resolver gives the hop for a host: with a PAC URL, the local PAC proxy once it reports a port, and direct until then; with a fixed proxy, the proxy unless one of its exclusion patterns (`*` matches any run of characters, ASCII case ignored) matches the whole host. Its caller is the proxy path (#49).

### Rationale

`getDefaultProxy()` is what the platform's own Java stack routes through, so the client agrees with the rest of the app, and it honours a global proxy that the network's `LinkProperties` (option 2) do not carry. The platform's local PAC proxy is how Android runs a PAC script for every app; evaluating the script in process (option 2) would add a JavaScript engine, against the size budget, and `PacProxyManager` (option 3) needs API 33.

### Validated by

On the emulator: `platform::android::proxy::tests::settings_map_to_the_configuration_they_describe`, `a_fixed_proxy_carries_every_host_but_its_exclusions`, `a_pac_script_sends_every_host_to_the_local_pac_proxy_once_it_listens`, `an_ipv6_proxy_is_bracketed_and_no_port_is_direct`, `patterns_match_whole_hosts_with_stars_anywhere`. A network configured with a proxy or a PAC URL: #55.

## Platform verifier

### Problem

rustls-platform-verifier on Android verifies through the platform's trust store over JNI, and panics while verifying when it was never given the JVM ([reqwest#2968](https://github.com/seanmonstar/reqwest/issues/2968)).

### Constraints

- Scope: rustls with the platform verifier, initialised through `ndk-context`, with no platform call in the public API.

### Alternatives

1. Initialise it on the first `https` request, from the `Context` "Embedding" reads, and report it unavailable where there is none.
2. Initialise it when a `Client` is built.

### Choice

Option 1, once per process, with `rustls_platform_verifier::android::init_with_env`. The `jni` dependency is the major version the verifier takes, so both share the one `JavaVM`.

### Rationale

`Client::new` cannot fail (api.md, "Client and builder"), and an app may build a client before it initialises `ndk-context`; platform work that can fail reports through the first request's error. Reporting the verifier unavailable without a JVM keeps the verifier from panicking.

### Validated by

`platform::android::tests::without_an_android_context_nothing_is_observed_or_verified` and `tls::tests::an_unavailable_platform_verifier_fails_as_tls`. Verification against the device's trust store: #55.

## Resume after Doze

### Problem

In Doze and App Standby the platform blocks the app's network traffic. Connections pooled before the block may be dead after it (NAT and server idle timeouts run on), and a request sent on one waits for the read-stall timeout before it fails.

### Constraints

- The monitor's events are internal, and the pool consumes them (#6).
- What the pool does on resume (revalidate or evict) is a departure from a plain hyper client, and evidence-gated with the rest of the lifecycle policy (#16).

### Alternatives

1. `NetworkCallback.onBlockedStatusChanged` (API 29), reported as an internal `blocked` fact, whose end on the same network the monitor publishes as `NetworkEvent::Resumed`.
2. The `ACTION_DEVICE_IDLE_MODE_CHANGED` broadcast.

### Choice

Option 1. A block and its end are published at once, without the confirmation a new network needs, as any fact on the same network is. The end of a block on another network is that network's `Changed`.

### Rationale

The blocked status is the platform's own statement that this app's traffic is blocked, and it covers Doze, App Standby and background restrictions alike, where the idle broadcast (option 2) covers Doze alone and says nothing about this app. Publishing the end as its own event lets the pool tell a resume from a metering change without comparing facts. The pool's response, and the netlab evidence for it, are #16.

### Validated by

`monitor::tests::the_end_of_a_block_on_the_same_network_is_a_resume` and `a_block_and_its_end_are_published_without_confirmation`; `platform::android::tests::a_blocked_network_is_reported_blocked`. Real Doze: #55.
