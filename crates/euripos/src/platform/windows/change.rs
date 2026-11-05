//! The IP Helper change notifications: `NotifyIpInterfaceChange` and
//! `NotifyRouteChange2`, each a signal that the route to the internet may
//! have moved.

use std::ffi::c_void;
use std::io;
use std::sync::Arc;

use tokio::sync::Notify;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, MIB_IPFORWARD_ROW2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
    NotifyIpInterfaceChange, NotifyRouteChange2,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;

use super::check;

/// The interface and route notifications, both registered for IPv4 and
/// IPv6. Each wakes the [`Notify`] it was opened with.
pub(super) struct Changes {
    interface: HANDLE,
    route: HANDLE,
    /// The context both callbacks read, from [`Arc::into_raw`]; released on
    /// drop, once neither callback can run.
    context: *const Notify,
}

impl Changes {
    /// Register both notifications, waking `notify` on each.
    pub(super) fn open(notify: &Arc<Notify>) -> io::Result<Self> {
        let context = Arc::into_raw(Arc::clone(notify));
        let mut changes = Self {
            interface: HANDLE::default(),
            route: HANDLE::default(),
            context,
        };
        // SAFETY: `context` is a live `Notify` until `drop`, which cancels
        // both registrations before releasing it; the callbacks only read
        // it. Each handle is written by the call that registers it.
        unsafe {
            check(NotifyIpInterfaceChange(
                AF_UNSPEC,
                Some(on_interface),
                Some(context.cast::<c_void>()),
                false,
                &raw mut changes.interface,
            ))?;
            check(NotifyRouteChange2(
                AF_UNSPEC,
                Some(on_route),
                context.cast::<c_void>(),
                false,
                &raw mut changes.route,
            ))?;
        }
        Ok(changes)
    }
}

impl Drop for Changes {
    fn drop(&mut self) {
        // SAFETY: each handle is null or a registration this value made.
        // `CancelMibChangeNotify2` returns once no callback of the
        // registration is running, so `context` is released after the last
        // read of it.
        unsafe {
            for handle in [self.interface, self.route] {
                if !handle.is_invalid() {
                    let _ = CancelMibChangeNotify2(handle);
                }
            }
            drop(Arc::from_raw(self.context));
        }
    }
}

/// `PIPINTERFACE_CHANGE_CALLBACK`: an interface was added, removed, or
/// changed a parameter.
unsafe extern "system" fn on_interface(
    context: *const c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: `context` is the `Notify` `Changes::open` registered, alive
    // while the registration is.
    unsafe { wake(context) }
}

/// `PIPFORWARD_CHANGE_CALLBACK`: a route was added, removed or changed.
unsafe extern "system" fn on_route(
    context: *const c_void,
    _row: *const MIB_IPFORWARD_ROW2,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: as in `on_interface`.
    unsafe { wake(context) }
}

/// Wake the `Notify` behind `context`.
///
/// # Safety
///
/// `context` points to a live `Notify`.
pub(super) unsafe fn wake(context: *const c_void) {
    // SAFETY: the caller's contract.
    unsafe { &*context.cast::<Notify>() }.notify_one();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::Changes;

    #[test]
    fn both_notifications_register_and_cancel() {
        let notify = Arc::new(Notify::new());
        let changes = Changes::open(&notify).unwrap();
        assert_eq!(Arc::strong_count(&notify), 2);
        drop(changes);
        assert_eq!(Arc::strong_count(&notify), 1);
    }

    #[tokio::test]
    async fn a_wake_reaches_the_waiter() {
        let notify = Arc::new(Notify::new());
        let context = Arc::as_ptr(&notify).cast();
        // SAFETY: `notify` is alive for the call.
        unsafe { super::wake(context) };
        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .unwrap();
    }
}
