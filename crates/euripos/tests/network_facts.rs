//! `Client::network_facts` through the public API, on the host's real
//! network.

use euripos::{Client, IpAvailability};

#[test]
fn a_network_is_named_exactly_when_a_route_to_the_internet_is_not_ruled_out() {
    let facts = Client::new().network_facts();
    let routed =
        facts.ipv4() == IpAvailability::Available || facts.ipv6() == IpAvailability::Available;
    let unrouted =
        facts.ipv4() == IpAvailability::Unavailable && facts.ipv6() == IpAvailability::Unavailable;
    // Where both versions are known, a network is named exactly when one
    // routes. A platform that names a network without saying which version
    // routes (the iOS simulator) leaves both unknown.
    if routed {
        assert!(facts.network().is_some(), "{facts:?}");
    }
    if unrouted {
        assert!(facts.network().is_none(), "{facts:?}");
    }
}

#[test]
fn facts_are_read_without_a_runtime_and_agree_between_clients() {
    let first = Client::new().network_facts();
    let second = Client::new().network_facts();
    // One monitor per process: both clients read the same network.
    assert_eq!(first.network(), second.network());
}

#[tokio::test]
async fn facts_are_read_inside_a_runtime() {
    let facts = Client::new().network_facts();
    if cfg!(target_abi = "sim") {
        // The iOS simulator's path reports no IP version.
        assert_ne!(facts.ipv4(), IpAvailability::Unavailable, "{facts:?}");
    } else if cfg!(any(
        target_os = "linux",
        target_os = "windows",
        target_os = "macos",
        target_os = "ios"
    )) {
        // The Linux and Windows monitors read the system's routes, and the
        // Apple one its current path, so whether IPv4 is routed is known
        // whatever the host's network.
        assert_ne!(facts.ipv4(), IpAvailability::Unknown, "{facts:?}");
    } else {
        // Android's monitor asks the JVM of the app it runs in; these tests
        // run on the emulator outside any app, so nothing is known.
        assert_eq!(facts.ipv4(), IpAvailability::Unknown, "{facts:?}");
    }
}
