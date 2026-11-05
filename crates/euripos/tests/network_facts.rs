//! `Client::network_facts` through the public API, on the host's real
//! network.

use euripos::{Client, IpAvailability};

#[test]
fn a_network_is_named_exactly_when_a_route_to_the_internet_is_known() {
    let facts = Client::new().network_facts();
    let routed =
        facts.ipv4() == IpAvailability::Available || facts.ipv6() == IpAvailability::Available;
    assert_eq!(facts.network().is_some(), routed, "{facts:?}");
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
    if cfg!(target_os = "linux") {
        // The Linux monitor reads the kernel's routes, so whether IPv4 is
        // routed is known whatever the host's network.
        assert_ne!(facts.ipv4(), IpAvailability::Unknown, "{facts:?}");
    } else {
        // TODO(#11): Apple; TODO(#12): Android; TODO(#13): Windows.
        assert_eq!(facts.ipv4(), IpAvailability::Unknown, "{facts:?}");
    }
}
