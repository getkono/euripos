//! The connection cost from the Network List Manager's
//! `INetworkCostManager`, and its `CostChanged` event.

use std::sync::Arc;

use tokio::sync::Notify;
use windows::Win32::Networking::NetworkListManager::{
    INetworkCostManager, INetworkCostManagerEvents, NLM_CONNECTION_COST, NLM_CONNECTION_COST_FIXED,
    NLM_CONNECTION_COST_OVERDATALIMIT, NLM_CONNECTION_COST_ROAMING,
    NLM_CONNECTION_COST_UNRESTRICTED, NLM_CONNECTION_COST_VARIABLE, NetworkListManager,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, IConnectionPoint,
    IConnectionPointContainer,
};
use windows::core::{Interface, Result};

use crate::{Constraint, Metering};
use sink::CostEvents;

/// The machine's connection cost, and the subscription that wakes a
/// [`Notify`] when it changes.
pub(super) struct Cost {
    manager: INetworkCostManager,
    /// The connection point and the cookie `Advise` returned; `None` where
    /// the event could not be subscribed to, and the cost is read only when
    /// another signal fires.
    events: Option<(IConnectionPoint, u32)>,
}

impl Cost {
    /// Open the cost manager on this thread, joining the multithreaded COM
    /// apartment, and subscribe `notify` to its `CostChanged` event.
    pub(super) fn open(notify: &Arc<Notify>) -> Result<Self> {
        // SAFETY: called on the monitor's own thread, which no other code
        // initialises COM on; the apartment lasts as long as the thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        // SAFETY: COM is initialised on this thread.
        let manager: INetworkCostManager =
            unsafe { CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL) }?;
        let events = subscribe(&manager, notify).ok();
        Ok(Self { manager, events })
    }

    /// The cost of the machine's connections now, as metering and
    /// constraint; `Unknown` for both where it cannot be read.
    pub(super) fn read(&self) -> (Metering, Constraint) {
        self.cost()
            .map_or((Metering::Unknown, Constraint::Unknown), classify)
    }

    /// The `NLM_CONNECTION_COST` bits of the machine's connections.
    fn cost(&self) -> Result<u32> {
        let mut cost = 0;
        // SAFETY: `cost` is written by the call; a null destination asks
        // for the machine-wide cost.
        unsafe { self.manager.GetCost(&raw mut cost, std::ptr::null()) }?;
        Ok(cost)
    }
}

impl Drop for Cost {
    fn drop(&mut self) {
        if let Some((point, cookie)) = self.events.take() {
            // SAFETY: `cookie` is the one `Advise` returned on `point`.
            let _ = unsafe { point.Unadvise(cookie) };
        }
    }
}

fn subscribe(
    manager: &INetworkCostManager,
    notify: &Arc<Notify>,
) -> Result<(IConnectionPoint, u32)> {
    let container: IConnectionPointContainer = manager.cast()?;
    // SAFETY: the IID is a valid GUID for the duration of the call.
    let point = unsafe { container.FindConnectionPoint(&INetworkCostManagerEvents::IID) }?;
    let sink: INetworkCostManagerEvents = CostEvents {
        notify: Arc::clone(notify),
    }
    .into();
    // SAFETY: `sink` is a live COM object; the connection point holds its
    // own reference until `Unadvise`.
    let cookie = unsafe { point.Advise(&sink) }?;
    Ok((point, cookie))
}

#[allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    reason = "in the code windows-core's `implement` macro generates"
)]
mod sink {
    use std::sync::Arc;

    use tokio::sync::Notify;
    use windows::Win32::Networking::NetworkListManager::{
        INetworkCostManagerEvents, INetworkCostManagerEvents_Impl, NLM_SOCKADDR,
    };
    use windows::core::{Result, implement};

    /// The `INetworkCostManagerEvents` sink: every event is a signal.
    #[implement(INetworkCostManagerEvents)]
    pub(super) struct CostEvents {
        pub(super) notify: Arc<Notify>,
    }

    impl INetworkCostManagerEvents_Impl for CostEvents_Impl {
        fn CostChanged(&self, _cost: u32, _destination: *const NLM_SOCKADDR) -> Result<()> {
            self.notify.notify_one();
            Ok(())
        }

        fn DataPlanStatusChanged(&self, _destination: *const NLM_SOCKADDR) -> Result<()> {
            self.notify.notify_one();
            Ok(())
        }
    }
}

/// Metering and constraint from an `NLM_CONNECTION_COST` bit set.
///
/// The level (`UNRESTRICTED`, `FIXED`, `VARIABLE`) is the metering. Over its
/// data limit or roaming, Microsoft asks apps to transfer only what the user
/// opts into, which is [`Constraint::Constrained`]; any other known cost is
/// unconstrained. A cost with no level is `Unknown` for both.
fn classify(cost: u32) -> (Metering, Constraint) {
    let has = |flag: NLM_CONNECTION_COST| {
        #[allow(clippy::cast_sign_loss, reason = "the flags are bits of a u32")]
        let bit = flag.0 as u32;
        cost & bit != 0
    };
    let metering = if has(NLM_CONNECTION_COST_FIXED) || has(NLM_CONNECTION_COST_VARIABLE) {
        Metering::Metered
    } else if has(NLM_CONNECTION_COST_UNRESTRICTED) {
        Metering::Unmetered
    } else {
        return (Metering::Unknown, Constraint::Unknown);
    };
    let constraint = if has(NLM_CONNECTION_COST_OVERDATALIMIT) || has(NLM_CONNECTION_COST_ROAMING) {
        Constraint::Constrained
    } else {
        Constraint::Unconstrained
    };
    (metering, constraint)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Notify;

    use super::{Cost, classify};
    use crate::{Constraint, Metering};

    const UNKNOWN: u32 = 0;
    const UNRESTRICTED: u32 = 0x1;
    const FIXED: u32 = 0x2;
    const VARIABLE: u32 = 0x4;
    const OVER_DATA_LIMIT: u32 = 0x1_0000;
    const CONGESTED: u32 = 0x2_0000;
    const ROAMING: u32 = 0x4_0000;
    const APPROACHING_DATA_LIMIT: u32 = 0x8_0000;

    #[test]
    fn cost_levels_and_flags_map_as_documented() {
        let unconstrained = |metering| (metering, Constraint::Unconstrained);
        let constrained = |metering| (metering, Constraint::Constrained);
        assert_eq!(classify(UNKNOWN), (Metering::Unknown, Constraint::Unknown));
        // Flags without a level say nothing reliable.
        assert_eq!(classify(ROAMING), (Metering::Unknown, Constraint::Unknown));
        assert_eq!(classify(UNRESTRICTED), unconstrained(Metering::Unmetered));
        assert_eq!(
            classify(UNRESTRICTED | CONGESTED),
            unconstrained(Metering::Unmetered)
        );
        assert_eq!(classify(FIXED), unconstrained(Metering::Metered));
        assert_eq!(classify(VARIABLE), unconstrained(Metering::Metered));
        assert_eq!(
            classify(FIXED | APPROACHING_DATA_LIMIT),
            unconstrained(Metering::Metered)
        );
        assert_eq!(
            classify(FIXED | OVER_DATA_LIMIT),
            constrained(Metering::Metered)
        );
        assert_eq!(classify(VARIABLE | ROAMING), constrained(Metering::Metered));
    }

    // The host's own Network List Manager: the cost reads, whatever it is,
    // and the event subscribes. On a thread of its own, which joins the
    // multithreaded apartment as the monitor's does.
    #[test]
    fn the_host_cost_opens_subscribes_and_reads() {
        let notify = Arc::new(Notify::new());
        std::thread::spawn(move || {
            let cost = Cost::open(&notify).unwrap();
            assert!(cost.events.is_some());
            cost.cost().unwrap();
        })
        .join()
        .unwrap();
    }
}
