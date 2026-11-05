# Network context

The network context is what the client knows about the network it is on: an identity for it, and facts about it, every one of which may be `Unknown`. The network-keyed pool (#6), alternative-service and estimator state, and Happy Eyeballs history key on it. This document covers the model, the platform-agnostic monitor that keeps it current, and its Linux, Windows and Apple implementations; the Android one is in [android.md](android.md). The public facts are also described in [api.md](api.md), "Network facts".

## Network identity

### Problem

A connection belongs to the network it was made on (scope, "Why not reqwest": transfers hang after a Wi-Fi to cellular switch). Consumers need to tell one network from the next without knowing what a network is on each platform.

### Constraints

- No platform type and no `cfg` on a public item; the identity names no vendor, SSID or address (issue #4).
- It changes when the default route's interface or the network behind it changes, and not otherwise: a change evicts connections (#6), so a spurious one costs every open connection.
- Principle 4: where the platform cannot name a network, the client says so rather than guessing.

### Alternatives

1. An opaque `NetworkId(u64)` hashed from what the platform reports about the route out (on Linux, the interface index, next hop and source address of the routes to the internet), exposed as `NetworkFacts::network() -> Option<NetworkId>`.
2. A counter that advances on each confirmed change, so a network rejoined later gets a new identity.
3. A public enum or string naming the network (SSID, interface name).
4. An `Unknown` value of `NetworkId` itself, in place of `Option`.

### Choice

1. `NetworkId` is `Copy + Eq + Hash`, with a `Debug` that prints only the hash. The hash is keyed once per process (a `RandomState` held for the process's life), so equal keys give equal identities within a process and unrelated ones across processes: an identity in a log cannot be matched by hashing candidate interfaces, gateways and addresses, which a fixed-key hash over private address space would allow. `NetworkFacts::network()` returns `None` where the client cannot name a network: no route to the internet, or a platform with no monitor yet.

On Linux the key is, for each IP version the kernel routes to the internet, the outgoing interface index, the gateway and the source address; an IPv6 source counts by its /64 prefix.

### Rationale

Option 1 gives equal identities for an unchanged route, so a reading that changes nothing compares equal and is never an event, and needs no state shared between the monitor's restarts. Option 2 makes every reading taken during a reconfiguration a new network. Option 3 is the vendor string the issue rules out, and leaks the SSID. `Option` (over option 4) is used because an identity has no likely value a caller could mistake absence for, which is what the explicit `Unknown` variants of the other facts guard against; two unknown identities comparing equal would also merge every unnamed network into one pool key.

The IPv6 source counts by prefix because temporary addresses (RFC 8981) rotate within a prefix every few hours without the network changing. An IPv4 source counts whole: connections bound to an address the host lost are dead, so a new address is a new network for the pool. Two networks with the same gateway address, reached through the same interface and handing out the same IPv4 address, share an identity; the reading on Linux carries nothing (an SSID, a gateway MAC) that could tell them apart without the vendor detail the issue rules out or a neighbour-table lookup that is itself racy.

### Validated by

`facts::tests::a_network_id_is_equal_for_equal_keys_and_differs_otherwise`, `a_network_id_is_not_the_fixed_key_hash`, `a_network_id_debug_names_no_address`; `platform::linux::tests::a_rotated_ipv6_temporary_address_keeps_the_network`, `a_new_ipv4_address_or_gateway_is_a_new_network`.

## Network facts

### Problem

Consumers and callers adapt to the kind of network (metered, constrained, which IP versions route, whether a proxy is configured), and each platform reports a different subset.

### Constraints

- Every fact may be `Unknown`; nothing reports its likely value (Principle 4).
- A fact can be added without a breaking change.

### Alternatives

1. Add `ipv4()`, `ipv6()` (`IpAvailability { Available, Unavailable, Unknown }`) and `proxy()` (`ProxyConfig { Direct, Manual, Automatic, Unknown }`) to the existing `NetworkFacts` getters, each enum `#[non_exhaustive]`.
2. One `ip() -> IpVersions` fact holding both IP versions.
3. Leave the proxy to the proxy integration of each platform (#11, #12, #13) and add only what the Linux monitor reads.

### Choice

1. On Linux: `interface()` from the link the route to the internet leaves through; `metering()` from NetworkManager; `constraint()` `Unknown`, since Linux has no low-data mode; `ipv4()` and `ipv6()` `Available` when the kernel routes a public address of that version, `Unavailable` when it does not; `proxy()` `Manual` when `http_proxy`, `https_proxy` or `all_proxy` (either case) is set and not empty, `Direct` otherwise. On Windows, as "Windows monitor" sets out, on macOS and iOS as "Apple monitor" does, and on Android as [android.md](android.md), "Android monitor", does.

### Rationale

Two getters match `interface()`, `metering()` and `constraint()`, one enum per fact, and Happy Eyeballs (#15) asks for each version separately. The proxy fact lands now because the issue names it in the model and its Linux source (the environment, per scope) needs no new dependency; the other platforms fill it with their proxy integrations. `Direct` on Linux with no variable set is an observation, not a guess: the environment is where euripos reads the Linux proxy from (scope, "Platform integration"), so with none set it connects directly. Desktop proxy settings (GNOME, KDE) are not read, as the scope does not name them.

### Validated by

`facts::tests::unknown_facts_report_unknown_rather_than_a_likely_value`; `platform::linux::tests::any_proxy_variable_is_a_manual_proxy_and_none_is_direct`, `interfaces_are_classified_by_devtype_then_hardware`, `the_host_network_is_named_exactly_when_a_route_exists`; `crates/euripos/tests/network_facts.rs`; `mise run api:cfg` shows the facts are the same on every platform.

## Change monitor

### Problem

Consumers need to hear that the network changed, that a fact about it changed, or that no network is left, as soon as it is true and never when it is not. Platforms report changes noisily: Tailscale's monitor saw interface lists that briefly report nothing (tailscale#17523), and floods of route messages it took for link changes. Either, published, evicts every connection for nothing.

### Constraints

- Platform-agnostic: one decision procedure for what counts as a change; platforms only supply readings and signals (CLAUDE.md, "Rules").
- No `cfg` and nothing new on the public API until a caller needs it (api.md, "Network facts").
- tokio only; `Client::network_facts` is synchronous and may be called with or without a runtime.
- A timing test never sleeps the time it asserts (CONTRIBUTING.md, "Tests").

### Alternatives

For what a signal means:
1. A signal is only a reason to read again. The monitor waits for signals to settle, reads, and publishes only a reading that differs from the last one published. A reading that names another network, or none, is published only once a second reading, taken a confirmation interval later, names the same network.
2. Publish each platform event as it arrives, filtered by type.
3. Debounce signals but publish every differing reading at once.

For where the monitor runs:
1. One process-wide monitor on a thread of its own with a current-thread tokio runtime, started on first use.
2. A task per `Client` on the caller's runtime.
3. One task on whichever runtime first asks.

### Choice

Option 1 for both. Signals settle when the platform has been quiet for 50 ms, or after 1 s of continuous signals; a new network or a loss is confirmed by a reading 500 ms after the first. A reading on the same network that differs in another fact (metering, say) is published at once. The internal `NetworkMonitor` holds the published facts in a `watch` channel; `Changes::next` folds them into `NetworkEvent::{Changed(NetworkId), FactsChanged, Resumed, Lost}`. `Resumed` is the end, on the same network, of a block the platform put on this process's traffic, an internal `blocked` fact only Android reports ([android.md](android.md), "Resume after Doze"). `Client::network_facts` starts the monitor and waits up to 500 ms for its first reading, once per process. The monitor and its events are internal: the pool (#6) is their first consumer.

### Rationale

Comparing readings rather than events makes a flood of signals that change nothing publish nothing, however the platform phrases them, and the settle window bounds the readings a flood causes to about one per second. Confirming a new network or a loss makes a reading taken mid-change, the empty interface list among them, invisible unless it lasts. Option 3 would still publish the empty list. Option 2 trusts each platform's event taxonomy, which is where both Tailscale bugs came from. A fact change on the same network skips confirmation because it evicts nothing.

The cost is latency: a real switch is published 550 ms or more after it happens. The pool (#6) is evidence-gated and its netlab result measures time to the first successful request after a change, which includes this delay; the constants are tuned there.

A thread of its own (where option 2 and 3 tie the monitor to a runtime that may shut down, as every `#[tokio::test]` does) keeps one monitor per process, which matches the platform state it reads, needs no runtime from the caller, and lets readings block briefly on a local system call. Network state is per process, so a per-client monitor (option 2) duplicates sockets and D-Bus connections for no gain.

### Validated by

`monitor::tests`, driven by a fake platform under paused tokio time: `a_brief_empty_reading_with_a_signal_back_is_not_a_change` and `a_brief_empty_reading_with_no_signal_back_is_not_a_change` (tailscale#17523), `a_signal_flood_that_changes_nothing_is_not_a_change` (100 000 signals over 10 s, at most 13 readings and no event), `a_brief_other_network_is_not_a_change`, `a_lasting_new_network_is_published_once_confirmed`, `a_lasting_loss_is_published_once_confirmed`, `a_fact_change_on_the_same_network_is_published_without_confirmation`, `the_settler_confirms_only_a_repeated_network`, `events_name_what_changed`, `the_end_of_a_block_on_the_same_network_is_a_resume`, `a_block_and_its_end_are_published_without_confirmation`, `changes_fold_published_facts_into_events`.

## Linux monitor

### Problem

The Linux kernel knows the route to the internet and the link it leaves through; whether the network is metered is known only to NetworkManager, where it runs.

### Constraints

- One implementation on Linux (Principle 2); scope names rtnetlink link, route and address groups, and NetworkManager `Metered` over D-Bus, absent meaning `Unknown`.
- Policy routing (WireGuard and Tailscale use rule tables) and split default routes (`0.0.0.0/1` with `128.0.0.0/1`, as OpenVPN installs) must resolve as a real connection would.
- The facts are read on a thread that also runs the D-Bus client.

### Alternatives

For the route to the internet:
1. Ask the kernel for the route to a public address (`RTM_GETROUTE` with `RTA_DST`, as `ip route get` does), once per IP version.
2. Dump the main routing table and pick the lowest-metric default route.

For the rtnetlink messages:
1. Encode and parse the four message layouts the monitor uses, over `netlink-sys` sockets.
2. `netlink-packet-route` and `rtnetlink`.

For D-Bus:
1. `zbus` with its tokio integration.
2. `dbus`, which links libdbus.

### Choice

Option 1 in each. The monitor subscribes to `RTMGRP_LINK`, `RTMGRP_IPV4_IFADDR`, `RTMGRP_IPV6_IFADDR`, `RTMGRP_IPV4_ROUTE` and `RTMGRP_IPV6_ROUTE`. A notification is a signal unless it is a wireless event on a link (scan results, which arrive in floods), a host- or link-scoped address, a cloned (cached) route, or a local, broadcast, anycast or multicast route or one in the local table; a lost notification (`ENOBUFS`) is a signal. A reading asks the kernel for its route to `8.8.8.8` and to `2001:4860:4860::8888`; no packet is sent. The interface kind comes from the link the route leaves through: sysfs `DEVTYPE` `wlan` (or a `wireless` directory) is Wi-Fi, `wwan` is cellular, Ethernet framing on a physical device is Ethernet, and anything else (loopback, tunnels, bridges, veth) is other.

NetworkManager's `Metered` property on `/org/freedesktop/NetworkManager` maps `NM_METERED_YES` (1) and `NM_METERED_GUESS_YES` (3) to metered, `NM_METERED_NO` (2) and `NM_METERED_GUESS_NO` (4) to unmetered, and `NM_METERED_UNKNOWN` (0), an absent NetworkManager and an absent system bus to `Unknown`. The source follows the owner of NetworkManager's bus name: when the name loses its owner (NetworkManager exits or restarts) metering becomes `Unknown`, and each new owner is read through a fresh proxy, because a zbus proxy keeps its cached `Metered` after the owner leaves and its property stream never ends. Opening the source waits up to 250 ms for the first answer (NetworkManager's value, or `Unknown` at once where its name has no owner or there is no system bus), so the first reading, the one the 500 ms startup wait returns, carries metering; an answer slower than that arrives as a change.

### Rationale

Asking for the route applies policy rules and the longest-prefix match the way a connection will, which a table dump (option 2) re-implements and gets wrong for rule-based VPNs and split defaults. With the route asked for, every route change outside the filtered kinds may move it, so the filter drops only kinds that never carry traffic to the internet, and the monitor's comparison absorbs the rest. Cloned routes and local-table routes are the Linux shape of a route-miss flood.

Two request types and four reply fields do not justify `netlink-packet-route`, which models the whole protocol; `netlink-sys` keeps the sockets free of `unsafe` here. `zbus` is pure Rust on the tokio runtime the monitor already runs; `dbus` needs libdbus and its headers on every Linux build host and cross target.

A guess counts as the value it guesses. NetworkManager guesses yes from evidence on the network itself (an Android hotspot's vendor DHCP option, a cellular or Bluetooth device). Treating a likely-metered link as unmetered spends the user's data, while treating a likely-unmetered one as metered only defers background work, so a guess of yes is followed; a guess of no is followed too, because NetworkManager guesses for every connection it has no explicit setting for, and reporting `Unknown` there would leave all of those without an answer. NetworkManager's global `Metered` is that of its primary connection, which on a host it manages carries the default route.

Coverage gaps, tracked in #45: the hosted CI runners do not run NetworkManager, so the D-Bus path is covered by its value mapping and by an owner that leaves a name on the session bus, and a real NetworkManager restart is not tested; a Wi-Fi or cellular interface is classified from fixtures, not from real hardware.

### Validated by

`platform::linux::netlink::tests` (`cloned_local_and_multicast_routes_are_not_signals`, `a_flood_of_cloned_routes_in_one_datagram_is_not_a_signal`, `wireless_link_events_are_not_signals_and_other_link_events_are`, `only_global_address_changes_are_signals`, parsing of route and link replies, and `the_kernel_routes_loopback_through_lo` against the host kernel); `platform::linux::networkmanager::tests::networkmanager_metered_values_map_as_documented`, and `metering_is_answered_with_no_owner_and_unknown_once_the_owner_leaves` against a name the test owns on the session bus (it skips where there is no session bus, except under CI, where it fails; CI runs every Linux test job under `dbus-run-session`); `platform::linux::tests::the_host_network_is_named_exactly_when_a_route_exists` and `the_source_opens_with_the_first_metering_answer_and_reads` on the Linux CI runners.

## Windows monitor

### Problem

Windows reports interface and route changes through IP Helper, how much traffic costs through the Network List Manager, and the proxy through WinHTTP, each its own API with its own change signal. The proxy is where reqwest goes wrong on Windows: it reads the system proxy once, when the client is built ([reqwest#2674](https://github.com/seanmonstar/reqwest/issues/2674)), and does not evaluate PAC scripts at all ([reqwest#2715](https://github.com/seanmonstar/reqwest/issues/2715)).

### Constraints

- One implementation on Windows (Principle 2); scope names `NotifyIpInterfaceChange`, `NotifyRouteChange2`, `INetworkCostManager`, WinHTTP's proxy with PAC and WPAD, and rustls with the platform verifier (`docs/scope.md`, "Platform integration").
- The same identity and facts as on Linux, so the monitor's decision procedure and the pool see one model.
- The cost is `Unknown` where the cost manager cannot be read; the proxy is re-read when it may have changed, not once.
- Callbacks arrive on system thread-pool threads; the monitor reads on its own thread.

### Alternatives

For the route to the internet:
1. `GetBestRoute2` to a public address per IP version, as Linux asks the kernel.
2. The lowest-metric default route from `GetIpForwardTable2`.

For the bindings:
1. The `windows` crate, which wraps COM interfaces and implements COM event sinks.
2. `windows-sys`, already in the graph through tokio and rustls-platform-verifier, which has no COM support, so `INetworkCostManager` would be called through a hand-written vtable.

For a change of proxy settings that is not a network change:
1. A registry change notification on the current user's `Internet Settings` key, which WinHTTP reads the settings from.
2. `WinHttpRegisterProxyChangeNotification`, which exists only from Windows 11 and Server 2022.
3. Re-read only on a network signal.

### Choice

Option 1 in each. Every IP Helper notification is a signal; the monitor's comparison absorbs those that change nothing. A reading asks `GetBestRoute2` for the route to `8.8.8.8` and to `2001:4860:4860::8888`; any failure is no route for that version. The network key is Linux's: interface index, next hop and source address, an IPv6 source by its /64. The interface kind comes from `GetIfEntry2`: `IF_TYPE_IEEE80211` or the native 802.11 medium is Wi-Fi, `IF_TYPE_WWANPP`, `IF_TYPE_WWANPP2` or the wireless WAN medium is cellular, `IF_TYPE_ETHERNET_CSMACD` backed by hardware is Ethernet, and anything else (loopback, tunnels, VPN adapters, Hyper-V's virtual switch) is other.

The monitor's thread joins the multithreaded COM apartment and opens `INetworkCostManager`; a reading calls `GetCost` for the machine's connections, and the `CostChanged` and `DataPlanStatusChanged` events are signals. `NLM_CONNECTION_COST_UNRESTRICTED` is unmetered and `FIXED` or `VARIABLE` metered; over the data limit or roaming the constraint is `Constrained`, and any other known cost `Unconstrained`; a cost with no level, or one that cannot be read, is `Unknown` for both. Approaching the data limit and congestion change neither fact.

The proxy fact is read on every reading from `WinHttpGetIEProxyConfigForCurrentUser`: automatic detection or a PAC URL is `Automatic`, otherwise a proxy server is `Manual`, otherwise `Direct`; where the user has no settings (a service account), `WinHttpGetDefaultProxyConfiguration`, the machine's WinHTTP proxy, decides between `Manual` and `Direct`. A change under `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings` is a signal: the notification is thread-agnostic and re-armed in its wait callback before the monitor is woken, so a change made between two notifications is still read.

An internal resolver evaluates the automatic configuration per URL with `WinHttpGetProxyForUrl` (WPAD by DHCP and DNS, then the PAC URL), and returns the ordered hops the script chose; discovery that finds no script is direct, as in WinHTTP and the browsers. When it is asked on a network other than the last, it calls `WinHttpResetAutoProxy` so the session and the WinHTTP service discard the script and discovery they cached, and the proxy is re-read per network rather than once. It never offers the user's default credentials to the server the script is fetched from (`fAutoLogonIfChallenged` is off): WPAD lets whoever answers discovery on the local network name that server, so a script behind Windows authentication fails to resolve instead. Its caller is the proxy path (#49), which also owns a fixed proxy's bypass list; until then only its tests call it.

rustls-platform-verifier verifies through CryptoAPI on Windows and needs no initialisation, so the platform reports its verifier available, as on Linux.

### Rationale

`GetBestRoute2` applies the route table, interface metrics and the source address selection a connection gets, which a table scan (option 2) re-implements and gets wrong for VPNs that install more specific routes. The `windows` crate's COM support is what makes the cost manager's event sink a struct with two methods; with `windows-sys` it would be a hand-built vtable and reference count, the kind of `unsafe` the crate exists to remove. Both crates share `windows-link`, and the minimal client does not link the monitor, so the size budget is unaffected.

A proxy that changes without a network change is the case reqwest#2674 reports; re-reading only on network signals (option 3) would miss it. `WinHttpRegisterProxyChangeNotification` (option 2) would leave Windows 10 without one. The registry key is where WinHTTP and WinINet read the user's settings from, and the watch is noisy (other Internet Settings change under it), which costs a reading that compares equal and publishes nothing.

Over the data limit and roaming map to `Constrained` because Microsoft's guidance for those costs is that apps transfer only what the user asks for, the meaning `Constrained` has on every platform. A cost with flags but no level contradicts itself and is reported `Unknown`.

Coverage gaps, tracked in #51: the hosted runners have one virtual wired adapter, an unrestricted cost and no proxy, so the interface kinds and the cost mapping are tested on fixtures, WPAD discovery and a reset on a real network change are not exercised, and the settings watch is tested on a key the test owns rather than through Settings.

### Validated by

On the Windows CI runners (x86_64 and aarch64): `platform::windows::tests::the_host_network_is_named_exactly_when_a_route_exists` (IPv4 routed, a network named, the same one on a second reading, the cost manager open, or the cost `Unknown` where the edition provides no Network List Manager cost interface, failing with `REGDB_E_CLASSNOTREG` or `E_NOINTERFACE` as the hosted Windows Server 2025 runner does), `loopback_routes_through_loopback`, `the_platform_verifier_rejects_an_untrusted_certificate`; `platform::windows::change::tests::both_notifications_register_and_cancel`; `platform::windows::cost::tests::the_host_cost_opens_subscribes_and_reads`; `platform::windows::proxy::tests::a_pac_script_picks_the_hops_per_url` (WinHTTP fetches a PAC script from a loopback server and runs it per URL, then again after a network change), `a_change_under_the_watched_key_wakes_the_watch` (twice, so the re-arm holds), `no_automatic_configuration_is_direct` and `the_host_configuration_reads`. On fixtures: `interfaces_are_classified_by_type_medium_and_hardware`, `cost_levels_and_flags_map_as_documented`, `user_settings_map_to_the_configuration_they_describe`, `a_user_without_settings_falls_back_to_the_machine`, `proxy_lists_parse_in_order`, `a_rotated_ipv6_temporary_address_keeps_the_network`, `a_new_ipv4_address_or_gateway_is_a_new_network`. `crates/euripos/tests/network_facts.rs` reads known IPv4 availability through the public API on Windows.

## Apple monitor

### Problem

On macOS and iOS the system decides which path a new connection takes, and Network.framework's path monitor reports that path: its interfaces, its gateways, and whether it is expensive, constrained (Low Data Mode) or ultra-constrained. Routes are not the model there: the system may bring an interface up on demand (VPN On Demand), and iOS exposes no routing table to an app.

### Constraints

- One implementation on Apple (Principle 2); scope names `nw_path_monitor` for expensive, constrained and ultra-constrained.
- The same identity and facts as on Linux and Windows.
- Ultra-constrained exists from macOS and iOS 26; the library must still load on earlier releases.
- The bindings are the ones [ADR 0003](../adr/0003-apple-network-framework.md) chose.

### Alternatives

For the identity:
1. The path's first interface (the one traffic leaves through) and the addresses of its gateways (`nw_path_enumerate_gateways`).
2. The first interface alone.
3. A routing-table read through `sysctl` (`NET_RT_DUMP`), as on the other platforms.

For ultra-constrained:
1. Look `nw_path_is_ultra_constrained` up at run time, and report constrained or not from `nw_path_is_constrained` where it is absent.
2. Link it weakly through the SDK.

### Choice

Option 1 in each. The monitor starts on the monitor's thread and delivers each path on the platform's serial dispatch queue; the handler copies what it needs out of the path and wakes the source, so no Network.framework object outlives the callback. Opening the source waits up to 250 ms for the first path, so the first reading names the network, as on Linux. A path that is not satisfied (unsatisfied, or satisfiable only once something such as VPN On Demand brings an interface up) names no network, and its interface, metering and constraint are `Unknown`. A satisfied path's identity is its first interface's index and its gateways' addresses, sorted; its interface kind is the first interface's type (Wi-Fi, cellular, wired as Ethernet, anything else, such as a VPN's `utun`, as other); expensive is metered and not expensive unmetered; ultra-constrained, then constrained, then neither. `ipv4()` and `ipv6()` are the path's `has_ipv4` and `has_ipv6` in every status, except that a satisfied path reporting neither (as the iOS simulator's does) leaves both `Unknown`: a satisfied path routes traffic, so it has not said which version does. The proxy fact is `Unknown`: the path does not carry it.

### Rationale

The path is what the system will use for the next connection, so reading it, rather than routes (option 3), agrees with what `nw_connection` does by construction, and works on iOS, where the routing table is not available to apps. The interface alone (option 2) would keep one identity across every Wi-Fi network joined through `en0`, which is exactly the switch the pool must see. Gateways stand in for the source address the other platforms key on, which the path does not report; two networks that hand out the same gateway address on the same interface share an identity, as on Linux where they also share the address.

Expensive is the system's own judgement of cost (cellular, and a Wi-Fi network that is a personal hotspot), the meaning `Metered` has on every platform. A path that is not satisfied says nothing about the network it would use, so its flags are `Unknown` rather than a likely value (Principle 4). Looking ultra-constrained up at run time (option 1) keeps the minimum releases where they are; a weak link (option 2) needs an SDK that declares the symbol on every build host, and the hosted runners' Xcode may predate it. A system without the function has no ultra-constrained mode, so constrained or not is then the whole answer.

The path carries no proxy configuration. The system's proxy settings are a CFNetwork dictionary with no change signal on the path; the client's proxy support (#49) is where they will be read, and the fact can be filled from there.

Coverage gaps: the hosted macOS runner and the iOS simulator have one wired or virtual network, no cellular path, no Low Data Mode and no VPN, so the interface kinds, the expensive and constrained mapping and a change of network are tested on fixtures; ultra-constrained is untested on a system that has it. The simulator's path reports no IP version, so the IPv4 and IPv6 mapping of a device's path is tested on fixtures only. The device lab (#10) measures a real handover.

### Validated by

On the macOS job and the iOS simulator job: `platform::apple::path::tests::the_host_network_is_named_and_its_flags_known` (a satisfied path names a network, the same on a second reading, with IPv4 available on macOS and not reported unavailable on the simulator); on fixtures, `a_satisfied_path_reports_its_interface_flags_and_ip_versions`, `a_satisfied_path_with_no_ip_version_leaves_both_unknown`, `ultra_constrained_is_reported_where_the_system_has_it`, `an_unsatisfied_path_names_no_network_and_no_flags`, `the_network_is_the_interface_and_its_gateways`, `socket_addresses_parse_by_family`. `crates/euripos/tests/network_facts.rs` reads known IPv4 availability through the public API on macOS, and on the iOS simulator that IPv4 is not reported unavailable.
