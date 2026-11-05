//! What the client knows about the network it is on, as typed values that may
//! be `Unknown`.

use std::fmt;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::OnceLock;

/// A snapshot of what the platform reports about the network the client is
/// currently using.
///
/// Every fact may be `Unknown`: a fact the platform cannot observe is never
/// reported as its most likely value. Read one with
/// [`Client::network_facts`](crate::Client::network_facts). Facts are added
/// as getters, so the struct can grow without a breaking change; it is not
/// `Copy`, so a later fact need not be either.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetworkFacts {
    pub(crate) network: Option<NetworkId>,
    pub(crate) interface: InterfaceKind,
    pub(crate) metering: Metering,
    pub(crate) constraint: Constraint,
    pub(crate) ipv4: IpAvailability,
    pub(crate) ipv6: IpAvailability,
    pub(crate) proxy: ProxyConfig,
    /// Whether the platform blocks this process's traffic on the network
    /// now, as Android does in Doze, App Standby and Data Saver. Internal:
    /// the monitor reports the end of a block as a resume, after which
    /// pooled connections are suspect. Only Android reports one.
    pub(crate) blocked: bool,
}

impl NetworkFacts {
    /// Facts about a network nothing is known about.
    pub(crate) const UNKNOWN: Self = Self {
        network: None,
        interface: InterfaceKind::Unknown,
        metering: Metering::Unknown,
        constraint: Constraint::Unknown,
        ipv4: IpAvailability::Unknown,
        ipv6: IpAvailability::Unknown,
        proxy: ProxyConfig::Unknown,
        blocked: false,
    };

    /// The network traffic leaves through now.
    ///
    /// `None` when the client cannot name one: the platform reports no route
    /// to the internet, or it does not report networks at all. An identity
    /// has no likely value to mistake for it, so absence is `None` rather
    /// than an `Unknown` variant.
    #[must_use]
    pub fn network(&self) -> Option<NetworkId> {
        self.network
    }

    /// The kind of interface traffic leaves through.
    #[must_use]
    pub fn interface(&self) -> InterfaceKind {
        self.interface
    }

    /// Whether traffic on this network is metered.
    #[must_use]
    pub fn metering(&self) -> Metering {
        self.metering
    }

    /// Whether the user or the system asked apps to limit their traffic on
    /// this network.
    #[must_use]
    pub fn constraint(&self) -> Constraint {
        self.constraint
    }

    /// Whether the platform routes IPv4 to the internet.
    #[must_use]
    pub fn ipv4(&self) -> IpAvailability {
        self.ipv4
    }

    /// Whether the platform routes IPv6 to the internet.
    #[must_use]
    pub fn ipv6(&self) -> IpAvailability {
        self.ipv6
    }

    /// How the platform is configured to reach the internet through a proxy.
    #[must_use]
    pub fn proxy(&self) -> ProxyConfig {
        self.proxy
    }
}

/// An opaque identity for the network traffic leaves through.
///
/// Two values are equal when they name the same network: the same interface
/// and the same route out of it. The value changes when the interface or the
/// network behind it changes, so connections made under one `NetworkId` can
/// be told apart from those made under the next. It names no vendor, SSID or
/// address, and means nothing outside the process that read it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkId(u64);

impl NetworkId {
    /// The identity of the network `key` describes. Equal keys give equal
    /// identities within one process.
    ///
    /// The hash is keyed once per process, so an identity printed by one
    /// process cannot be matched against keys hashed in another, and the
    /// address behind it cannot be recovered by hashing candidates.
    pub(crate) fn from_key(key: &impl Hash) -> Self {
        static KEYS: OnceLock<RandomState> = OnceLock::new();
        Self(KEYS.get_or_init(RandomState::new).hash_one(key))
    }
}

impl fmt::Debug for NetworkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NetworkId({:016x})", self.0)
    }
}

/// The kind of interface traffic leaves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InterfaceKind {
    /// Wi-Fi.
    Wifi,
    /// A cellular radio.
    Cellular,
    /// Wired Ethernet.
    Ethernet,
    /// An interface the platform reports that is none of the above.
    Other,
    /// The platform does not say.
    Unknown,
}

/// Whether traffic on a network is metered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Metering {
    /// Traffic is not metered.
    Unmetered,
    /// Traffic is not metered for now, and may become metered without the
    /// network changing.
    TemporarilyUnmetered,
    /// Traffic is metered.
    Metered,
    /// The platform does not say.
    Unknown,
}

/// Whether apps are asked to limit their traffic on a network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Constraint {
    /// No limit is asked for.
    Unconstrained,
    /// Apps are asked to limit their traffic, as in a low data mode.
    Constrained,
    /// Apps are asked to send only what the user is waiting for, as on a
    /// satellite link.
    UltraConstrained,
    /// The platform does not say.
    Unknown,
}

/// Whether the platform routes one IP version to the internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IpAvailability {
    /// The platform has a route to the internet for this IP version.
    Available,
    /// The platform has no route to the internet for this IP version.
    Unavailable,
    /// The platform does not say.
    Unknown,
}

/// How the platform is configured to reach the internet through a proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProxyConfig {
    /// No proxy is configured: connections go to the origin directly.
    Direct,
    /// A fixed proxy is configured.
    Manual,
    /// A proxy auto-configuration (PAC) script or WPAD discovery picks the
    /// proxy per request.
    Automatic,
    /// The platform does not say.
    Unknown,
}

#[cfg(test)]
mod tests {
    use std::hash::{DefaultHasher, Hash, Hasher};

    use super::{
        Constraint, InterfaceKind, IpAvailability, Metering, NetworkFacts, NetworkId, ProxyConfig,
    };

    #[test]
    fn unknown_facts_report_unknown_rather_than_a_likely_value() {
        let facts = NetworkFacts::UNKNOWN;
        assert_eq!(facts.network(), None);
        assert_eq!(facts.interface(), InterfaceKind::Unknown);
        assert_eq!(facts.metering(), Metering::Unknown);
        assert_eq!(facts.constraint(), Constraint::Unknown);
        assert_eq!(facts.ipv4(), IpAvailability::Unknown);
        assert_eq!(facts.ipv6(), IpAvailability::Unknown);
        assert_eq!(facts.proxy(), ProxyConfig::Unknown);
    }

    #[test]
    fn a_network_id_is_equal_for_equal_keys_and_differs_otherwise() {
        assert_eq!(
            NetworkId::from_key(&(2_u32, "a")),
            NetworkId::from_key(&(2_u32, "a"))
        );
        assert_ne!(
            NetworkId::from_key(&(2_u32, "a")),
            NetworkId::from_key(&(3_u32, "a"))
        );
    }

    // A fixed-key hash would let anyone hash candidate addresses and match
    // a logged identity; the process-keyed one differs from it (but for a
    // 2^-64 collision).
    #[test]
    fn a_network_id_is_not_the_fixed_key_hash() {
        let key = (2_u32, "192.0.2.1");
        let mut fixed = DefaultHasher::new();
        key.hash(&mut fixed);
        assert_ne!(NetworkId::from_key(&key).0, fixed.finish());
    }

    #[test]
    fn a_network_id_debug_names_no_address() {
        let debug = format!("{:?}", NetworkId::from_key(&"192.0.2.1"));
        assert!(debug.starts_with("NetworkId("), "{debug}");
        assert!(!debug.contains("192.0.2.1"), "{debug}");
    }
}
