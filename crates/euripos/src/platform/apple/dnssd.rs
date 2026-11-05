//! HTTPS records through dnssd's `DNSServiceQueryRecord`, answered by
//! mDNSResponder with the system's resolver configuration: VPN split DNS,
//! DNS64, and configuration profiles' encrypted DNS. See
//! docs/design/https-svcb.md, "macOS and iOS".
//!
//! Whether dnssd returns HTTPS records to an app on every supported iOS
//! version is unverified on devices (#58). Where it returns none, the
//! answer is that there are none, and the origin's records are `Unknown`.

#![allow(
    non_snake_case,
    reason = "the C API's own names, so each declaration can be checked against dns_sd.h"
)]

use std::cell::RefCell;
use std::ffi::{CString, c_char, c_int, c_void};
use std::os::fd::{AsRawFd, RawFd};
use std::ptr;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use crate::dns::message::{CLASS_IN, TYPE_HTTPS};
use crate::dns::{Answer, Record};

/// `DNSServiceRef`.
type DNSServiceRef = *mut c_void;

/// `kDNSServiceFlagsMoreComing`: another reply is already queued.
const FLAGS_MORE_COMING: u32 = 0x1;
/// `kDNSServiceFlagsAdd`: the reply adds a record.
const FLAGS_ADD: u32 = 0x2;
/// `kDNSServiceFlagsReturnIntermediates`: report that no record exists,
/// rather than waiting for one to appear.
const FLAGS_RETURN_INTERMEDIATES: u32 = 0x1000;
/// `kDNSServiceFlagsTimeout`: mDNSResponder ends the query with
/// `kDNSServiceErr_Timeout` when it gets no answer.
const FLAGS_TIMEOUT: u32 = 0x1_0000;

/// `kDNSServiceErr_NoError`.
const NO_ERROR: i32 = 0;
/// `kDNSServiceErr_NoSuchRecord`.
const NO_SUCH_RECORD: i32 = -65_554;

/// Query `name`'s HTTPS records on any interface. `None` where the query
/// cannot be started or fails.
pub(crate) async fn query_https(name: &str) -> Option<Answer> {
    let name = CString::new(name).ok()?;
    let replies = Box::new(RefCell::new(Replies::default()));
    // `replies` is declared before `query`, so it is dropped after the query
    // is deallocated, and no reply is delivered after that.
    let query = Query::start(&name, &replies)?;
    // SAFETY: `query` holds a live reference.
    let fd = unsafe { DNSServiceRefSockFD(query.0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: the socket stays open until `query` is deallocated, and
    // `registered`, declared after it, is dropped and deregistered first.
    let registered = unsafe { AsyncFd::register_with_interest(Fd(fd), Interest::READABLE) }.ok()?;
    loop {
        let mut ready = registered.readable().await.ok()?;
        loop {
            // SAFETY: `query` holds a live reference, and its socket is
            // readable, so the call does not block. No borrow of `replies`
            // is held across it.
            if unsafe { DNSServiceProcessResult(query.0) } != NO_ERROR {
                return None;
            }
            let replies = replies.borrow();
            if !replies.more_coming {
                break;
            }
        }
        ready.clear_ready();
        match replies.borrow().outcome() {
            Outcome::Pending => {}
            Outcome::Answered(answer) => return Some(answer),
            Outcome::Failed => return None,
        }
    }
}

/// Where a query stands after a complete batch of replies.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// No reply has answered yet.
    Pending,
    Answered(Answer),
    /// A reply carried an error.
    Failed,
}

/// The replies delivered so far.
#[derive(Default)]
struct Replies {
    records: Vec<Record>,
    /// A reply said no record exists.
    none: bool,
    /// A reply carried an error.
    failed: bool,
    /// The last reply said another is queued.
    more_coming: bool,
}

impl Replies {
    /// Where the query stands, given the replies so far.
    fn outcome(&self) -> Outcome {
        if self.more_coming {
            Outcome::Pending
        } else if self.failed {
            Outcome::Failed
        } else if !self.records.is_empty() {
            Outcome::Answered(Answer::Records(self.records.clone()))
        } else if self.none {
            Outcome::Answered(Answer::NoRecords { negative_ttl: None })
        } else {
            Outcome::Pending
        }
    }

    /// Take in one reply, a record of type `kind` in `class`. A record that
    /// is not an HTTPS record in `IN`, such as a CNAME on the way to one, is
    /// not kept, as `dns::message::answer` keeps none.
    fn take(&mut self, flags: u32, error: i32, (kind, class): (u16, u16), rdata: &[u8], ttl: u32) {
        self.more_coming = flags & FLAGS_MORE_COMING != 0;
        match error {
            NO_ERROR if flags & FLAGS_ADD == 0 => {}
            NO_ERROR if kind == TYPE_HTTPS && class == CLASS_IN => self.records.push(Record {
                rdata: rdata.to_vec(),
                ttl,
            }),
            NO_ERROR => {}
            NO_SUCH_RECORD => self.none = true,
            _ => self.failed = true,
        }
    }
}

/// `DNSServiceQueryRecordReply`: called by `DNSServiceProcessResult` on the
/// thread that calls it.
unsafe extern "C" fn reply(
    _service: DNSServiceRef,
    flags: u32,
    _interface: u32,
    error: i32,
    _name: *const c_char,
    kind: u16,
    class: u16,
    length: u16,
    rdata: *const c_void,
    ttl: u32,
    context: *mut c_void,
) {
    // SAFETY: `context` is the `RefCell<Replies>` `query_https` passed,
    // alive for as long as the query is.
    let replies = unsafe { &*context.cast_const().cast::<RefCell<Replies>>() };
    let rdata = if rdata.is_null() || length == 0 {
        &[][..]
    } else {
        // SAFETY: dnssd passes `length` readable bytes at `rdata` for the
        // duration of the call.
        unsafe { std::slice::from_raw_parts(rdata.cast::<u8>(), usize::from(length)) }
    };
    // Never unwinds into C: a borrow already held drops the reply.
    if let Ok(mut replies) = replies.try_borrow_mut() {
        replies.take(flags, error, (kind, class), rdata, ttl);
    }
}

/// A live query, deallocated on drop, which closes its socket.
struct Query(DNSServiceRef);

// SAFETY: a `DNSServiceRef` may be used from any thread, one at a time; the
// query is only used through `&mut` access to the future that owns it.
unsafe impl Send for Query {}

impl Query {
    /// Start the query for `name`'s HTTPS records, whose replies go to
    /// `replies`, which must outlive the query.
    fn start(name: &CString, replies: &RefCell<Replies>) -> Option<Self> {
        let mut service: DNSServiceRef = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call; `replies`
        // outlives the query, as the caller ensures.
        let error = unsafe {
            DNSServiceQueryRecord(
                &raw mut service,
                FLAGS_RETURN_INTERMEDIATES | FLAGS_TIMEOUT,
                0,
                name.as_ptr(),
                TYPE_HTTPS,
                CLASS_IN,
                reply,
                ptr::from_ref(replies).cast_mut().cast(),
            )
        };
        (error == NO_ERROR && !service.is_null()).then_some(Self(service))
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: `self` holds the reference, released once.
        unsafe { DNSServiceRefDeallocate(self.0) }
    }
}

/// The query's socket, which `DNSServiceRefDeallocate` closes; it is only
/// borrowed here.
struct Fd(RawFd);

impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// `<dns_sd.h>`, in libSystem.
unsafe extern "C" {
    fn DNSServiceQueryRecord(
        sdRef: *mut DNSServiceRef,
        flags: u32,
        interfaceIndex: u32,
        fullname: *const c_char,
        rrtype: u16,
        rrclass: u16,
        callBack: unsafe extern "C" fn(
            DNSServiceRef,
            u32,
            u32,
            i32,
            *const c_char,
            u16,
            u16,
            u16,
            *const c_void,
            u32,
            *mut c_void,
        ),
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(sdRef: DNSServiceRef) -> c_int;
    fn DNSServiceProcessResult(sdRef: DNSServiceRef) -> i32;
    fn DNSServiceRefDeallocate(sdRef: DNSServiceRef);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        FLAGS_ADD, FLAGS_MORE_COMING, NO_ERROR, NO_SUCH_RECORD, Outcome, Replies, query_https,
    };
    use crate::dns::message::{CLASS_IN, TYPE_HTTPS};
    use crate::dns::{Answer, Record};

    /// An HTTPS record in `IN`.
    const HTTPS: (u16, u16) = (TYPE_HTTPS, CLASS_IN);

    #[test]
    fn records_are_answered_once_no_more_are_coming() {
        let mut replies = Replies::default();
        replies.take(
            FLAGS_ADD | FLAGS_MORE_COMING,
            NO_ERROR,
            HTTPS,
            b"\x00\x01\x00",
            300,
        );
        assert_eq!(replies.outcome(), Outcome::Pending);
        replies.take(FLAGS_ADD, NO_ERROR, HTTPS, b"\x00\x02\x00", 60);
        let records = vec![
            Record {
                rdata: b"\x00\x01\x00".to_vec(),
                ttl: 300,
            },
            Record {
                rdata: b"\x00\x02\x00".to_vec(),
                ttl: 60,
            },
        ];
        assert_eq!(
            replies.outcome(),
            Outcome::Answered(Answer::Records(records))
        );
    }

    #[test]
    fn no_such_record_is_no_records_and_other_errors_fail() {
        let mut replies = Replies::default();
        replies.take(0, NO_SUCH_RECORD, HTTPS, &[], 0);
        assert_eq!(
            replies.outcome(),
            Outcome::Answered(Answer::NoRecords { negative_ttl: None })
        );
        let mut replies = Replies::default();
        // `kDNSServiceErr_Timeout`.
        replies.take(0, -65_568, HTTPS, &[], 0);
        assert_eq!(replies.outcome(), Outcome::Failed);
    }

    #[test]
    fn a_removal_is_not_an_answer() {
        let mut replies = Replies::default();
        replies.take(0, NO_ERROR, HTTPS, b"\x00\x01\x00", 300);
        assert_eq!(replies.outcome(), Outcome::Pending);
    }

    #[test]
    fn only_https_records_in_in_are_kept() {
        let mut replies = Replies::default();
        // A CNAME on the way to the records, then one in another class.
        let cname = (5, CLASS_IN);
        replies.take(
            FLAGS_ADD | FLAGS_MORE_COMING,
            NO_ERROR,
            cname,
            b"\x03svc\x07example\x00",
            300,
        );
        replies.take(
            FLAGS_ADD | FLAGS_MORE_COMING,
            NO_ERROR,
            (TYPE_HTTPS, 3),
            b"\x00\x01\x00",
            300,
        );
        replies.take(FLAGS_ADD, NO_ERROR, HTTPS, b"\x00\x02\x00", 60);
        let records = vec![Record {
            rdata: b"\x00\x02\x00".to_vec(),
            ttl: 60,
        }];
        assert_eq!(
            replies.outcome(),
            Outcome::Answered(Answer::Records(records))
        );

        let mut replies = Replies::default();
        replies.take(FLAGS_ADD, NO_ERROR, cname, b"\x03svc\x07example\x00", 300);
        assert_eq!(
            replies.outcome(),
            Outcome::Pending,
            "a CNAME alone is no answer"
        );
    }

    // mDNSResponder answers or times out; either way the query completes.
    // What a public name's records are is not asserted: tests have no
    // internet dependency.
    #[tokio::test]
    async fn a_query_completes() {
        let answer =
            tokio::time::timeout(Duration::from_secs(30), query_https("euripos.invalid")).await;
        assert!(answer.is_ok(), "mDNSResponder bounds its own query");
    }

    #[tokio::test]
    async fn a_name_with_a_nul_is_not_queried() {
        assert_eq!(query_https("exa\0mple.com").await, None);
    }

    #[tokio::test]
    async fn a_dropped_query_is_deallocated() {
        let query = query_https("euripos.invalid");
        // Polled once so the query is started and registered, then dropped.
        let _ = tokio::time::timeout(Duration::ZERO, query).await;
    }
}
