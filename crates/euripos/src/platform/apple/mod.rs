//! macOS and iOS: connections through Network.framework's `nw_connection`,
//! and the network from its path monitor.
//!
//! See docs/adr/0003-apple-network-framework.md and
//! docs/design/network-context.md, "Apple monitor".

mod connection;
mod ffi;
mod path;

use std::io;
use std::sync::OnceLock;

pub(crate) use connection::{Stream, connect};
pub(crate) use path::network_source;

/// An owned reference to a Network.framework object, released on drop.
struct Owned(ffi::nw_object_t);

// SAFETY: Network.framework objects are reference counted atomically, and
// the calls made on them here are documented as callable from any thread.
unsafe impl Send for Owned {}
// SAFETY: as above; nothing here mutates the object through `&Owned`
// except through Network.framework calls, which synchronise themselves.
unsafe impl Sync for Owned {}

impl Owned {
    /// Takes ownership of `object`, which a `_create` call returned. `None`
    /// where it is null.
    fn new(object: ffi::nw_object_t) -> Option<Self> {
        (!object.is_null()).then_some(Self(object))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `self` holds one reference, released once.
        unsafe { ffi::nw_release(self.0) }
    }
}

/// The serial dispatch queue every connection and the path monitor deliver
/// their callbacks on. Each callback only records what it was given and
/// wakes a task, so one queue serves the process.
fn queue() -> ffi::dispatch_queue_t {
    struct Queue(ffi::dispatch_queue_t);
    // SAFETY: a dispatch queue may be used from any thread.
    unsafe impl Send for Queue {}
    // SAFETY: as above.
    unsafe impl Sync for Queue {}
    static QUEUE: OnceLock<Queue> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            // SAFETY: the label is a NUL-terminated string; a null attribute
            // makes a serial queue. It is never released: it lives as long
            // as the process.
            Queue(unsafe {
                ffi::dispatch_queue_create(c"euripos.network".as_ptr(), std::ptr::null_mut())
            })
        })
        .0
}

/// What a Network.framework error says, kept so it can be reported to every
/// operation it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NwError {
    domain: u32,
    code: i32,
}

impl NwError {
    /// The error `error` holds, or `None` where it is null.
    ///
    /// # Safety
    ///
    /// `error` is null or a live `nw_error_t`.
    unsafe fn from_raw(error: ffi::nw_error_t) -> Option<Self> {
        if error.is_null() {
            return None;
        }
        // SAFETY: `error` is a live `nw_error_t`, per the contract.
        unsafe {
            Some(Self {
                domain: ffi::nw_error_get_error_domain(error),
                code: ffi::nw_error_get_error_code(error),
            })
        }
    }

    /// A POSIX error.
    fn posix(code: i32) -> Self {
        Self {
            domain: ffi::NW_ERROR_DOMAIN_POSIX,
            code,
        }
    }

    fn to_io(self) -> io::Error {
        match self.domain {
            ffi::NW_ERROR_DOMAIN_POSIX => io::Error::from_raw_os_error(self.code),
            ffi::NW_ERROR_DOMAIN_DNS => io::Error::other(format!(
                "name resolution failed (DNSServiceErrorType {})",
                self.code
            )),
            domain => io::Error::other(format!("network error {} in domain {domain}", self.code)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NwError, ffi};

    #[test]
    fn errors_keep_their_posix_code_and_name_the_others() {
        let refused = NwError::posix(libc::ECONNREFUSED).to_io();
        assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
        let dns = NwError {
            domain: ffi::NW_ERROR_DOMAIN_DNS,
            code: -65554,
        }
        .to_io();
        assert!(dns.to_string().contains("-65554"), "{dns}");
    }
}
