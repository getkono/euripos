//! HTTPS records through `android_res_nquery` (API 29) on the network the
//! callback last reported, so the query takes that network's resolvers and
//! its Private DNS setting. See docs/design/https-svcb.md, "Android".

use std::ffi::{CString, c_char, c_int};
use std::os::fd::{AsRawFd, RawFd};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use crate::NetworkId;
use crate::dns::Answer;
use crate::dns::message::{self, CLASS_IN, TYPE_HTTPS};

/// The largest DNS message over TCP, which the platform resolver falls back
/// to for a truncated answer.
const MAX_MESSAGE: usize = 65_535;

/// Query `name`'s HTTPS records on `network`: the network the callback last
/// reported, or the process's default network where `network` is `None` and
/// none is reported. `None` where the query cannot be sent or fails, or where
/// `network` is no longer the one the callback reports, so that no answer
/// from another network is kept under `network`.
pub(crate) async fn query_https(network: Option<NetworkId>, name: &str) -> Option<Answer> {
    query_on(handle_for(network, super::last().handle)?, name).await
}

/// The handle to query `network` on, given the handle the callback last
/// reported (0 where none): that handle where it names `network`, as
/// `super::facts` derives a `NetworkId` from it, and `None` otherwise.
fn handle_for(network: Option<NetworkId>, last: u64) -> Option<u64> {
    let current = (last != 0).then(|| NetworkId::from_key(&last));
    (current == network).then_some(last)
}

/// Query `name`'s HTTPS records on the network `handle` names; 0
/// (`NETWORK_UNSPECIFIED`) is the process's default network.
async fn query_on(handle: u64, name: &str) -> Option<Answer> {
    let name = CString::new(name).ok()?;
    let class = c_int::from(CLASS_IN);
    let kind = c_int::from(TYPE_HTTPS);
    // SAFETY: `name` is a NUL-terminated string that outlives the call.
    let fd = unsafe { android_res_nquery(handle, name.as_ptr(), class, kind, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is open until `android_res_nresult` or
    // `android_res_cancel` closes it, and neither is called while it is
    // registered: `into_inner` deregisters it first.
    let Ok(registered) = (unsafe { AsyncFd::register_with_interest(Fd(fd), Interest::READABLE) })
    else {
        // SAFETY: `fd` is the query's, unanswered and not yet closed.
        unsafe { android_res_cancel(fd) };
        return None;
    };
    let mut pending = Pending(Some(registered));
    // The guard is dropped at once: the descriptor is read once, below.
    drop(pending.0.as_ref()?.readable().await.ok()?);
    // Deregistered from the runtime before `android_res_nresult` closes it.
    let fd = pending.0.take()?.into_inner().0;
    let mut rcode = 0;
    let mut buffer = vec![0; MAX_MESSAGE];
    // SAFETY: `fd` is the query's, answered and not yet closed; it is closed
    // by this call. `buffer` holds `buffer.len()` writable bytes.
    let length =
        unsafe { android_res_nresult(fd, &raw mut rcode, buffer.as_mut_ptr(), buffer.len()) };
    let length = usize::try_from(length).ok()?;
    message::answer(buffer.get(..length)?)
}

/// The query's file descriptor, which `android_res_nresult` or
/// `android_res_cancel` closes; it is only borrowed here.
struct Fd(RawFd);

impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// A query not yet answered, cancelled where the lookup is dropped first.
struct Pending(Option<AsyncFd<Fd>>);

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(registered) = self.0.take() {
            let fd = registered.into_inner().0;
            // SAFETY: `fd` is the query's, deregistered and not yet closed;
            // the call closes it.
            unsafe { android_res_cancel(fd) };
        }
    }
}

// API 29, `<android/multinetwork.h>`; `net_handle_t` is a `uint64_t`.
#[link(name = "android")]
unsafe extern "C" {
    fn android_res_nquery(
        network: u64,
        dname: *const c_char,
        ns_class: c_int,
        ns_type: c_int,
        flags: u32,
    ) -> c_int;
    fn android_res_nresult(fd: c_int, rcode: *mut c_int, answer: *mut u8, anslen: usize) -> c_int;
    fn android_res_cancel(nsend_fd: c_int);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{handle_for, query_https, query_on};
    use crate::NetworkId;

    // A query for one network is never put to another, so its answer is
    // never cached under the wrong network.
    #[test]
    fn a_query_goes_only_to_the_network_it_names() {
        let handle = 0x1_cafe_d00d_u64;
        let network = Some(NetworkId::from_key(&handle));
        assert_eq!(handle_for(network, handle), Some(handle));
        assert_eq!(handle_for(None, 0), Some(0), "the default network");
        assert_eq!(handle_for(network, 0), None, "the network went away");
        assert_eq!(handle_for(None, handle), None);
        let other = Some(NetworkId::from_key(&0x2_cafe_d00d_u64));
        assert_eq!(handle_for(other, handle), None, "the network changed");
    }

    // The emulator's resolver answers or fails; either way the query
    // completes and the descriptor is consumed. What a public name's records
    // are is not asserted: tests have no internet dependency.
    #[tokio::test]
    async fn a_query_on_the_default_network_completes() {
        let answer = tokio::time::timeout(
            Duration::from_secs(30),
            query_https(None, "euripos.invalid"),
        )
        .await;
        assert!(answer.is_ok(), "the platform resolver bounds its own query");
    }

    // A handle is a network id over `Network`'s magic, 0xcafed00d; a value
    // without it is refused before anything is sent.
    #[tokio::test]
    async fn a_value_that_is_not_a_handle_is_refused() {
        assert_eq!(query_on(1, "example.com").await, None);
    }

    #[tokio::test]
    async fn a_name_with_a_nul_is_not_queried() {
        assert_eq!(query_on(0, "exa\0mple.com").await, None);
    }

    #[tokio::test]
    async fn a_dropped_query_is_cancelled() {
        let query = query_https(None, "euripos.invalid");
        // Polled once so the query is sent and registered, then dropped.
        let _ = tokio::time::timeout(Duration::ZERO, query).await;
    }
}
