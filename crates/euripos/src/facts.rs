//! What the client knows about the network it is on, as typed values that may
//! be `Unknown`.

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
    interface: InterfaceKind,
    metering: Metering,
    constraint: Constraint,
}

impl NetworkFacts {
    /// Facts about a network nothing is known about.
    pub(crate) const UNKNOWN: Self = Self {
        interface: InterfaceKind::Unknown,
        metering: Metering::Unknown,
        constraint: Constraint::Unknown,
    };

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

#[cfg(test)]
mod tests {
    use super::{Constraint, InterfaceKind, Metering, NetworkFacts};

    #[test]
    fn unknown_facts_report_unknown_rather_than_a_likely_value() {
        let facts = NetworkFacts::UNKNOWN;
        assert_eq!(facts.interface(), InterfaceKind::Unknown);
        assert_eq!(facts.metering(), Metering::Unknown);
        assert_eq!(facts.constraint(), Constraint::Unknown);
    }
}
