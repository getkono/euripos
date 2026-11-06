//! HTTPS records through `DnsQueryEx`, answered by the DNS Client service
//! with the system's resolution policy: the NRPT, per-interface servers,
//! Windows' DNS over HTTPS and its cache. See docs/design/https-svcb.md,
//! "Windows", and docs/adr/0002-windows-dns.md.
//!
//! The query is made without `DNS_QUERY_PARSE_ALL_RECORDS`, so `DnsQueryEx`
//! returns type 65 in the flat shape, `DNS_UNKNOWN_DATA`, on every build:
//! its RDATA as received, which the shared parser reads.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::iter;
use std::mem::offset_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::oneshot;
use windows::Win32::Foundation::{
    DNS_ERROR_RCODE_NAME_ERROR, DNS_INFO_NO_RECORDS, DNS_REQUEST_PENDING, NO_ERROR,
};
use windows::Win32::NetworkManagement::Dns::{
    DNS_QUERY_CANCEL, DNS_QUERY_REQUEST, DNS_QUERY_REQUEST_VERSION1, DNS_QUERY_RESULT,
    DNS_QUERY_RESULTS_VERSION1, DNS_QUERY_STANDARD, DNS_RECORDW, DNS_TYPE_HTTPS, DNS_TYPE_SOA,
    DNS_UNKNOWN_DATA, DNSREC_ANSWER, DNSREC_SECTION, DnsCancelQuery, DnsFree, DnsFreeRecordList,
    DnsQueryEx,
};
use windows::core::PCWSTR;

use crate::NetworkId;
use crate::dns::{Answer, Record};

/// The query succeeded.
const SUCCESS: i32 = NO_ERROR.0.cast_signed();
/// The name does not exist (`NXDOMAIN`).
const NAME_ERROR: i32 = DNS_ERROR_RCODE_NAME_ERROR.0.cast_signed();

/// Query `name`'s HTTPS records through the DNS Client service. `None` where
/// the query cannot be started or fails.
///
/// `network` is not read: the service answers with the configuration of the
/// network the system is on, and a query cannot be put to a network by its
/// [`NetworkId`]. During a network change the answer may come from the new
/// network; the cache's bound on that is in docs/design/https-svcb.md,
/// "Cache".
pub(crate) async fn query_https(_network: Option<NetworkId>, name: &str) -> Option<Answer> {
    match start(name) {
        Ok((_pending, receiver)) => receiver.await.ok().flatten(),
        Err(answer) => answer,
    }
}

/// Start the query for `name`: the query in flight with where its answer
/// arrives, or the answer where it completed at once.
fn start(name: &str) -> Result<(Pending, oneshot::Receiver<Option<Answer>>), Option<Answer>> {
    if name.contains('\0') {
        return Err(None);
    }
    let (sender, receiver) = oneshot::channel();
    let query = Arc::new(Query {
        name: name.encode_utf16().chain(iter::once(0)).collect(),
        result: UnsafeCell::new(DNS_QUERY_RESULT {
            Version: DNS_QUERY_RESULTS_VERSION1.0,
            ..DNS_QUERY_RESULT::default()
        }),
        cancel: UnsafeCell::new(DNS_QUERY_CANCEL::default()),
        completed: AtomicBool::new(false),
        sender: Mutex::new(Some(sender)),
    });
    // The callback's reference, released by the callback, or below where
    // the query completes without one.
    let context = Arc::into_raw(Arc::clone(&query));
    let request = DNS_QUERY_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        QueryName: PCWSTR(query.name.as_ptr()),
        QueryType: DNS_TYPE_HTTPS.0,
        QueryOptions: u64::from(DNS_QUERY_STANDARD.0),
        pDnsServerList: ptr::null_mut(),
        InterfaceIndex: 0,
        pQueryCompletionCallback: Some(on_complete),
        pQueryContext: context.cast_mut().cast(),
    };
    // SAFETY: `request` outlives the call; the name, the result and the
    // cancel handle it points to are `query`'s, which `context` keeps alive
    // until the callback has run.
    let status = unsafe {
        DnsQueryEx(
            &raw const request,
            query.result.get(),
            Some(query.cancel.get()),
        )
    };
    if status != DNS_REQUEST_PENDING {
        // Completed without the callback, which will not run: its reference
        // is released here, and the records are in `result`.
        // SAFETY: `context` came from `Arc::into_raw` above and nothing else
        // releases it.
        drop(unsafe { Arc::from_raw(context) });
        // SAFETY: no call writes `result` any more.
        let records = unsafe { (*query.result.get()).pQueryRecords };
        // SAFETY: `records` is the list `DnsQueryEx` returned, or null.
        return Err(unsafe { take(status, records.cast()) });
    }
    Ok((Pending(query), receiver))
}

/// One query's state, shared by the lookup and the completion callback. The
/// memory `DnsQueryEx` writes to lives here, so that it outlives both the
/// callback and any `DnsCancelQuery`.
struct Query {
    /// The name, UTF-16 and NUL-terminated.
    name: Vec<u16>,
    /// Written by `DnsQueryEx` until the query completes.
    result: UnsafeCell<DNS_QUERY_RESULT>,
    /// Written by `DnsQueryEx`; read by `DnsCancelQuery`.
    cancel: UnsafeCell<DNS_QUERY_CANCEL>,
    /// Set as the callback starts; the cancel handle is not used after.
    completed: AtomicBool,
    /// Where the callback sends its answer.
    sender: Mutex<Option<oneshot::Sender<Option<Answer>>>>,
}

// SAFETY: the cells are written by `DnsQueryEx` before it returns or before
// it invokes the callback, and read only after that; `cancel` is read by
// `DnsCancelQuery`, which may run on any thread. The pointers in `result`
// are the record list, which only `take` reads and frees, once.
unsafe impl Send for Query {}
// SAFETY: as above.
unsafe impl Sync for Query {}

/// A query not yet answered, cancelled where the lookup is dropped first.
struct Pending(Arc<Query>);

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.0.completed.load(Ordering::Acquire) {
            // SAFETY: the handle is the one `DnsQueryEx` returned for a
            // query whose callback had not started; `self.0` keeps it alive
            // until this call returns, and the callback's reference until
            // the callback has run, which `DnsCancelQuery` requires.
            unsafe {
                let _ = DnsCancelQuery(self.0.cancel.get());
            }
        }
    }
}

/// `DNS_QUERY_COMPLETION_ROUTINE`: the query completed, was cancelled or
/// failed.
unsafe extern "system" fn on_complete(context: *const c_void, results: *mut DNS_QUERY_RESULT) {
    // SAFETY: `context` is the reference `query_https` made for this
    // callback, which runs once.
    let query = unsafe { Arc::from_raw(context.cast::<Query>()) };
    query.completed.store(true, Ordering::Release);
    let answer = if results.is_null() {
        None
    } else {
        // SAFETY: `results` is the query's completed result, and its list
        // is read and freed here only.
        unsafe { take((*results).QueryStatus, (*results).pQueryRecords.cast()) }
    };
    let sender = query
        .sender
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(sender) = sender {
        // The lookup may have been dropped; the answer is then not wanted.
        let _ = sender.send(answer);
    }
}

/// The answer that `status` and `records` make, with `records` freed.
///
/// # Safety
///
/// `records` is null or a record list `DnsQueryEx` returned, not yet freed.
unsafe fn take(status: i32, records: *const DNS_RECORDW) -> Option<Answer> {
    // SAFETY: the caller's contract.
    let answer = unsafe { answer(status, records) };
    if !records.is_null() {
        // SAFETY: the caller's contract; the list is not read again.
        unsafe { DnsFree(Some(records.cast()), DnsFreeRecordList) };
    }
    answer
}

/// The answer that a query's status and record list make: the HTTPS records
/// of the answer section, as RDATA; no records where the name does not exist
/// or has none, with the negative TTL of an SOA in the list; and no answer
/// for any other status.
///
/// # Safety
///
/// `records` is null or the head of a list of records linked by `pNext`
/// and ending in null, each with `wDataLength` bytes of `Data` laid out as
/// its `wType` says, or as `DNS_UNKNOWN_DATA`.
unsafe fn answer(status: i32, records: *const DNS_RECORDW) -> Option<Answer> {
    if !matches!(status, SUCCESS | NAME_ERROR | DNS_INFO_NO_RECORDS) {
        return None;
    }
    let mut found = Vec::new();
    let mut negative_ttl = None;
    // SAFETY: the caller's contract.
    for record in unsafe { list(records) } {
        // SAFETY: the caller's contract: `record` is a live record.
        if let Some(record) = unsafe { https(record) } {
            found.push(record);
        // SAFETY: as above.
        } else if let Some(ttl) = unsafe { soa(record) } {
            negative_ttl = negative_ttl.or(Some(ttl));
        }
    }
    Some(if status == SUCCESS && !found.is_empty() {
        Answer::Records(found)
    } else {
        Answer::NoRecords { negative_ttl }
    })
}

/// The records of the list `head` starts.
///
/// # Safety
///
/// As [`answer`]'s `records`, for as long as the iterator is used.
unsafe fn list(head: *const DNS_RECORDW) -> impl Iterator<Item = *const DNS_RECORDW> {
    iter::successors((!head.is_null()).then_some(head), |&record| {
        // SAFETY: the caller's contract: `record` is a live record.
        let next = unsafe { (*record).pNext };
        (!next.is_null()).then_some(next.cast_const())
    })
}

/// `record` as an HTTPS record of the answer section, in the flat shape:
/// `dwByteCount` bytes of RDATA, which with the count's own four bytes fit
/// in `wDataLength` (`DNS_NULL_RECORD_LENGTH`). `None` for any other record.
///
/// # Safety
///
/// `record` is a live record with `wDataLength` bytes of `Data`.
unsafe fn https(record: *const DNS_RECORDW) -> Option<Record> {
    // SAFETY: the caller's contract; every bit pattern of `Flags` is a `u32`.
    let (kind, length, section, ttl) = unsafe {
        (
            (*record).wType,
            (*record).wDataLength,
            (*record).Flags.DW & DNSREC_SECTION,
            (*record).dwTtl,
        )
    };
    // Section 0 is allowed: a record served from the cache may not say.
    if kind != DNS_TYPE_HTTPS.0 || section > DNSREC_ANSWER {
        return None;
    }
    // SAFETY: the caller's contract: `Data` has `length` bytes, at least the
    // count's four where the check below passes.
    let data = unsafe { &raw const (*record).Data.Unknown };
    if usize::from(length) < offset_of!(DNS_UNKNOWN_DATA, bData) {
        return None;
    }
    // SAFETY: the count lies within `length`, checked above.
    let count = usize::try_from(unsafe { (*data).dwByteCount }).ok()?;
    if count.checked_add(offset_of!(DNS_UNKNOWN_DATA, bData))? > usize::from(length) {
        return None;
    }
    // SAFETY: `count` bytes from `bData` lie within the record's `length`
    // bytes of `Data`, checked above.
    let rdata =
        unsafe { std::slice::from_raw_parts((&raw const (*data).bData).cast::<u8>(), count) };
    Some(Record {
        rdata: rdata.to_vec(),
        ttl,
    })
}

/// The negative-caching TTL an SOA record gives (RFC 2308, section 5): the
/// lesser of its own TTL and its `MINIMUM` field. `None` for any other
/// record.
///
/// # Safety
///
/// As [`https`].
unsafe fn soa(record: *const DNS_RECORDW) -> Option<u32> {
    // SAFETY: the caller's contract; an SOA record's `Data` is `Soa`.
    unsafe {
        ((*record).wType == DNS_TYPE_SOA.0)
            .then(|| (*record).dwTtl.min((*record).Data.Soa.dwDefaultTtl))
    }
}

#[cfg(test)]
mod tests {
    use std::mem::offset_of;
    use std::ptr;
    use std::time::Duration;

    use windows::Win32::Foundation::{
        DNS_ERROR_RCODE_NAME_ERROR, DNS_ERROR_RCODE_SERVER_FAILURE, DNS_INFO_NO_RECORDS,
        ERROR_CANCELLED, ERROR_TIMEOUT,
    };
    use windows::Win32::NetworkManagement::Dns::{
        DNS_RECORDW, DNS_TYPE_CNAME, DNS_TYPE_HTTPS, DNS_TYPE_SOA, DNS_UNKNOWN_DATA,
        DNSREC_ADDITIONAL, DNSREC_ANSWER, DNSREC_AUTHORITY,
    };

    use super::{SUCCESS, answer, query_https};
    use crate::dns::{Answer, Record};

    /// A record with room for RDATA past the end of `Data`, as `DnsQueryEx`
    /// allocates a flat record.
    #[repr(C)]
    struct Owned {
        record: DNS_RECORDW,
        tail: [u8; 256],
    }

    /// A record list built as `DnsQueryEx` returns one, freed on drop.
    struct List(Vec<*mut Owned>);

    impl List {
        fn new() -> Self {
            Self(Vec::new())
        }

        /// Append a record of `kind` in `section`, and return it.
        fn push(&mut self, kind: u16, section: u32, ttl: u32) -> *mut DNS_RECORDW {
            let owned = Box::into_raw(Box::new(Owned {
                record: DNS_RECORDW::default(),
                tail: [0; 256],
            }));
            let record = owned.cast::<DNS_RECORDW>();
            // SAFETY: `record` is the first field of the live `owned`.
            unsafe {
                (*record).wType = kind;
                (*record).Flags.DW = section;
                (*record).dwTtl = ttl;
                if let Some(&last) = self.0.last() {
                    (*last.cast::<DNS_RECORDW>()).pNext = record;
                }
            }
            self.0.push(owned);
            record
        }

        /// Append a flat HTTPS record of `rdata`, its length as Windows
        /// sets it.
        fn https(&mut self, section: u32, ttl: u32, rdata: &[u8]) -> *mut DNS_RECORDW {
            let length = offset_of!(DNS_UNKNOWN_DATA, bData) + rdata.len();
            self.flat(section, ttl, rdata, u16::try_from(length).unwrap())
        }

        /// Append a flat HTTPS record of `rdata` claiming `length` bytes of
        /// `Data`.
        fn flat(&mut self, section: u32, ttl: u32, rdata: &[u8], length: u16) -> *mut DNS_RECORDW {
            let record = self.push(DNS_TYPE_HTTPS.0, section, ttl);
            // SAFETY: the bytes fit in `Data` and the `tail` after it.
            unsafe {
                (*record).wDataLength = length;
                (*record).Data.Unknown.dwByteCount = u32::try_from(rdata.len()).unwrap();
                let bytes = (&raw mut (*record).Data.Unknown.bData).cast::<u8>();
                ptr::copy_nonoverlapping(rdata.as_ptr(), bytes, rdata.len());
            }
            record
        }

        fn head(&self) -> *const DNS_RECORDW {
            self.0.first().map_or(ptr::null(), |&owned| {
                owned.cast::<DNS_RECORDW>().cast_const()
            })
        }

        fn answer(&self, status: i32) -> Option<Answer> {
            // SAFETY: the list is well formed and alive.
            unsafe { answer(status, self.head()) }
        }
    }

    impl Drop for List {
        fn drop(&mut self) {
            for &owned in &self.0 {
                // SAFETY: each came from `Box::into_raw` and is freed once.
                drop(unsafe { Box::from_raw(owned) });
            }
        }
    }

    const SERVICE: &[u8] = &[0, 1, 0, 0, 1, 0, 3, 2, b'h', b'3'];
    const ALIAS: &[u8] = &[0, 0, 3, b's', b'v', b'c', 0];

    #[test]
    fn https_records_of_the_answer_section_come_back_as_their_rdata() {
        let mut list = List::new();
        list.push(DNS_TYPE_CNAME.0, DNSREC_ANSWER, 600);
        list.https(DNSREC_ANSWER, 300, SERVICE);
        list.https(0, 60, ALIAS);
        list.https(DNSREC_ADDITIONAL, 300, SERVICE);
        assert_eq!(
            list.answer(SUCCESS),
            Some(Answer::Records(vec![
                Record {
                    rdata: SERVICE.to_vec(),
                    ttl: 300,
                },
                Record {
                    rdata: ALIAS.to_vec(),
                    ttl: 60,
                },
            ]))
        );
    }

    // A record whose count runs past its data is not read; nor is one too
    // short to hold the count. A count that falls short of the data still
    // reads the count's bytes.
    #[test]
    fn a_flat_record_is_read_only_within_its_data() {
        let mut list = List::new();
        list.flat(DNSREC_ANSWER, 300, SERVICE, 4 + 9);
        list.flat(DNSREC_ANSWER, 300, &[], 3);
        assert_eq!(
            list.answer(SUCCESS),
            Some(Answer::NoRecords { negative_ttl: None })
        );

        let mut list = List::new();
        list.flat(DNSREC_ANSWER, 300, ALIAS, 64);
        assert_eq!(
            list.answer(SUCCESS),
            Some(Answer::Records(vec![Record {
                rdata: ALIAS.to_vec(),
                ttl: 300,
            }]))
        );
    }

    #[test]
    fn no_records_and_nxdomain_are_no_records_with_the_soa_negative_ttl() {
        let mut list = List::new();
        let soa = list.push(DNS_TYPE_SOA.0, DNSREC_AUTHORITY, 900);
        // SAFETY: `soa` is a live record of the list.
        unsafe { (*soa).Data.Soa.dwDefaultTtl = 120 };
        for status in [
            DNS_INFO_NO_RECORDS,
            DNS_ERROR_RCODE_NAME_ERROR.0.cast_signed(),
            SUCCESS,
        ] {
            assert_eq!(
                list.answer(status),
                Some(Answer::NoRecords {
                    negative_ttl: Some(120)
                }),
                "{status}"
            );
        }
        // SAFETY: as above.
        unsafe { (*soa).dwTtl = 30 };
        assert_eq!(
            list.answer(DNS_INFO_NO_RECORDS),
            Some(Answer::NoRecords {
                negative_ttl: Some(30)
            }),
            "the lesser of the SOA's TTL and its minimum"
        );
        assert_eq!(
            List::new().answer(DNS_INFO_NO_RECORDS),
            Some(Answer::NoRecords { negative_ttl: None })
        );
    }

    #[test]
    fn any_other_status_is_no_answer() {
        let mut list = List::new();
        list.https(DNSREC_ANSWER, 300, SERVICE);
        for status in [
            DNS_ERROR_RCODE_SERVER_FAILURE,
            ERROR_TIMEOUT,
            ERROR_CANCELLED,
        ] {
            assert_eq!(list.answer(status.0.cast_signed()), None, "{status:?}");
        }
    }

    // The runner's resolver answers or fails; either way the query
    // completes and its records are freed. What a public name's records are
    // is not asserted: tests have no internet dependency.
    #[tokio::test]
    async fn a_query_completes() {
        let answer = tokio::time::timeout(
            Duration::from_secs(30),
            query_https(None, "euripos.invalid"),
        )
        .await;
        assert!(
            answer.is_ok(),
            "the DNS Client service bounds its own query"
        );
    }

    #[tokio::test]
    async fn a_name_with_a_nul_is_not_queried() {
        assert_eq!(query_https(None, "exa\0mple.com").await, None);
    }

    #[tokio::test]
    async fn a_dropped_query_is_cancelled() {
        let query = query_https(None, "euripos.invalid");
        // Polled once so the query is sent, then dropped.
        let _ = tokio::time::timeout(Duration::ZERO, query).await;
    }
}
