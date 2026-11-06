//! The system proxy, through `CFNetwork`: the settings
//! `CFNetworkCopySystemProxySettings` reports, applied to a request's origin
//! by `CFNetworkCopyProxiesForURL`, and a PAC script run by
//! `CFNetworkExecuteProxyAutoConfigurationURL` where they name one.

use std::cell::Cell;
use std::ffi::{CStr, c_void};
use std::time::{Duration, Instant};

use super::ffi;
use crate::Error;
use crate::proxy::Hop;

/// How long a PAC script may take to be fetched and run before the request
/// goes direct. The request itself waits no longer than its total timeout,
/// which bounds this whole lookup; this bounds the thread it runs on.
const PAC_WAIT: Duration = crate::timeout::DEFAULT_TOTAL;

/// The routes the system proxy settings give a request to `host` on `port`,
/// over TLS when `tls`, in the order to try them. Blocks while a PAC script
/// is fetched or run: call it off the runtime's worker threads.
///
/// The settings' exceptions, and the choice between the `http` and `https`
/// proxy, are `CFNetwork`'s. A PAC script is run for the request's origin and
/// gives its whole list; one that cannot be fetched or run gives none, and
/// the request goes direct, as Safari's does. Settings that cannot be read
/// leave the request direct.
pub(crate) fn routes(tls: bool, host: &str, port: u16) -> Result<Vec<Option<Hop>>, Error> {
    let target = format!(
        "{}://{}/",
        if tls { "https" } else { "http" },
        Hop::authority(host, port)
    );
    let Some(target) = Cf::url(&target) else {
        return Ok(vec![None]);
    };
    // SAFETY: returns a new dictionary, owned from here on, or null.
    let Some(settings) = Cf::new(unsafe { ffi::CFNetworkCopySystemProxySettings() }) else {
        return Ok(vec![None]);
    };
    // SAFETY: both are live; returns a new array, owned from here on.
    let proxies = Cf::new(unsafe { ffi::CFNetworkCopyProxiesForURL(target.0, settings.0) });
    let entries = proxies.map_or_else(Vec::new, |proxies| entries(&proxies, &target));
    crate::proxy::platform_routes(entries.iter().map(Option::as_deref))
}

/// The entries of `proxies`, a `CFNetwork`proxy list for `target`: `None`
/// for the origin directly, or the proxy's `host:port`, with a scheme where
/// it is no `http` proxy so the proxy path can tell it apart. A PAC entry
/// is replaced by the list its script gives. An entry of another kind (FTP)
/// is left out.
fn entries(proxies: &Cf, target: &Cf) -> Vec<Option<String>> {
    let mut entries = Vec::new();
    for proxy in array(proxies.0) {
        match entry(proxy) {
            Entry::Route(route) => entries.push(route),
            Entry::Pac(pac) => {
                if let Some(list) = run(pac, target) {
                    entries.extend(array(list.0).filter_map(|proxy| match entry(proxy) {
                        Entry::Route(route) => Some(route),
                        // A script names no further script.
                        Entry::Pac(_) | Entry::Other => None,
                    }));
                }
            }
            Entry::Other => {}
        }
    }
    entries
}

/// What one proxy dictionary in a `CFNetwork`list says.
enum Entry {
    Route(Option<String>),
    /// A PAC script, borrowed from the dictionary.
    Pac(Pac),
    Other,
}

/// Where a PAC script comes from.
#[derive(Clone, Copy)]
enum Pac {
    /// A `CFURL` to fetch it from.
    Url(ffi::CFURLRef),
    /// The script itself, a `CFString`.
    Script(ffi::CFStringRef),
}

/// The entry `proxy`, a dictionary of a `CFNetwork`list, describes.
fn entry(proxy: ffi::CFTypeRef) -> Entry {
    // SAFETY: the keys and types are `CFNetwork`'s constants; `proxy` is a
    // live member of a live array, checked to be a dictionary.
    unsafe {
        if !has_type(proxy, ffi::CFDictionaryGetTypeID()) {
            return Entry::Other;
        }
        let kind = ffi::CFDictionaryGetValue(proxy, ffi::kCFProxyTypeKey);
        let is = |constant: ffi::CFStringRef| !kind.is_null() && ffi::CFEqual(kind, constant) != 0;
        if is(ffi::kCFProxyTypeNone) {
            return Entry::Route(None);
        }
        let scheme = if is(ffi::kCFProxyTypeHTTP) || is(ffi::kCFProxyTypeHTTPS) {
            // `HTTPS` is the `http` proxy for `https` requests.
            ""
        } else if is(ffi::kCFProxyTypeSOCKS) {
            "socks://"
        } else if is(ffi::kCFProxyTypeAutoConfigurationURL) {
            let url = ffi::CFDictionaryGetValue(proxy, ffi::kCFProxyAutoConfigurationURLKey);
            return if has_type(url, ffi::CFURLGetTypeID()) {
                Entry::Pac(Pac::Url(url))
            } else {
                Entry::Other
            };
        } else if is(ffi::kCFProxyTypeAutoConfigurationJavaScript) {
            let script =
                ffi::CFDictionaryGetValue(proxy, ffi::kCFProxyAutoConfigurationJavaScriptKey);
            return if has_type(script, ffi::CFStringGetTypeID()) {
                Entry::Pac(Pac::Script(script))
            } else {
                Entry::Other
            };
        } else {
            return Entry::Other;
        };
        let Some(host) = string(ffi::CFDictionaryGetValue(proxy, ffi::kCFProxyHostNameKey)) else {
            return Entry::Other;
        };
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(&host);
        // A proxy with no port is on the `http` default, as a URI's is.
        let port = number(ffi::CFDictionaryGetValue(proxy, ffi::kCFProxyPortNumberKey))
            .and_then(|port| u16::try_from(port).ok())
            .filter(|&port| port != 0)
            .unwrap_or(80);
        Entry::Route(Some(format!("{scheme}{}", Hop::authority(host, port))))
    }
}

/// What a PAC script's run gave, written by [`on_pac`].
#[derive(Default)]
struct PacResult {
    list: Cell<Option<Cf>>,
    done: Cell<bool>,
}

/// Run the PAC script `pac` for `target` on this thread's run loop, waiting
/// up to [`PAC_WAIT`]. `None` where it cannot be fetched or run, or did not
/// answer in time.
fn run(pac: Pac, target: &Cf) -> Option<Cf> {
    let result = PacResult::default();
    let mut context = ffi::CFStreamClientContext {
        version: 0,
        info: (&raw const result).cast_mut().cast::<c_void>(),
        retain: None,
        release: None,
        copy_description: None,
    };
    // SAFETY: `target` and the script are live; `context` is copied by the
    // call, and the `info` it carries outlives the source, which is
    // invalidated below before `result` goes out of scope.
    let source = Cf::new(unsafe {
        match pac {
            Pac::Url(url) => ffi::CFNetworkExecuteProxyAutoConfigurationURL(
                url,
                target.0,
                on_pac,
                &raw mut context,
            ),
            Pac::Script(script) => ffi::CFNetworkExecuteProxyAutoConfigurationScript(
                script,
                target.0,
                on_pac,
                &raw mut context,
            ),
        }
    })?;
    // SAFETY: the run loop is this thread's, and the source is live; it is
    // removed and invalidated before it is released.
    unsafe {
        let run_loop = ffi::CFRunLoopGetCurrent();
        ffi::CFRunLoopAddSource(run_loop, source.0, ffi::kCFRunLoopDefaultMode);
        let deadline = Instant::now() + PAC_WAIT;
        while !result.done.get() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            // Finished: the mode has no source left to wait on.
            if ffi::CFRunLoopRunInMode(ffi::kCFRunLoopDefaultMode, left.as_secs_f64(), 1)
                == ffi::K_CF_RUN_LOOP_RUN_FINISHED
            {
                break;
            }
        }
        ffi::CFRunLoopRemoveSource(run_loop, source.0, ffi::kCFRunLoopDefaultMode);
        ffi::CFRunLoopSourceInvalidate(source.0);
    }
    result.list.take()
}

/// `CFProxyAutoConfigurationResultCallback`: keep the list, or note the
/// error, and stop the run loop [`run`] is waiting in.
unsafe extern "C" fn on_pac(client: *mut c_void, list: ffi::CFArrayRef, error: ffi::CFErrorRef) {
    // SAFETY: `client` is the `PacResult` `run` registered, alive while its
    // source is; the list is borrowed for the call, so it is retained to be
    // kept.
    unsafe {
        let result = &*client.cast_const().cast::<PacResult>();
        if error.is_null() && has_type(list, ffi::CFArrayGetTypeID()) {
            result.list.set(Cf::new(ffi::CFRetain(list)));
        }
        result.done.set(true);
        ffi::CFRunLoopStop(ffi::CFRunLoopGetCurrent());
    }
}

/// The members of `array`, a `CFArray`, borrowed from it.
fn array(array: ffi::CFArrayRef) -> impl Iterator<Item = ffi::CFTypeRef> {
    // SAFETY: `array` is live for as long as its caller holds it, and is
    // checked to be an array before it is read.
    let count = unsafe {
        if has_type(array, ffi::CFArrayGetTypeID()) {
            ffi::CFArrayGetCount(array)
        } else {
            0
        }
    };
    // SAFETY: each index is below the count.
    (0..count).map(move |index| unsafe { ffi::CFArrayGetValueAtIndex(array, index) })
}

/// Whether `object` is non-null and of the type `id`.
///
/// # Safety
///
/// `object` is null or a live Core Foundation object.
unsafe fn has_type(object: ffi::CFTypeRef, id: ffi::CFTypeID) -> bool {
    // SAFETY: the caller's contract.
    !object.is_null() && unsafe { ffi::CFGetTypeID(object) } == id
}

/// The text of `string`, a `CFString`; `None` where it is not one.
///
/// # Safety
///
/// `string` is null or a live Core Foundation object.
unsafe fn string(string: ffi::CFTypeRef) -> Option<String> {
    // SAFETY: the caller's contract; the buffer holds the longest UTF-8
    // form of the string and its NUL.
    unsafe {
        if !has_type(string, ffi::CFStringGetTypeID()) {
            return None;
        }
        let size = ffi::CFStringGetMaximumSizeForEncoding(
            ffi::CFStringGetLength(string),
            ffi::K_CF_STRING_ENCODING_UTF8,
        )
        .checked_add(1)?;
        let mut buffer = vec![0_u8; usize::try_from(size).ok()?];
        if ffi::CFStringGetCString(
            string,
            buffer.as_mut_ptr().cast(),
            size,
            ffi::K_CF_STRING_ENCODING_UTF8,
        ) == 0
        {
            return None;
        }
        CStr::from_bytes_until_nul(&buffer)
            .ok()?
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

/// The value of `number`, a `CFNumber`; `None` where it is not one.
///
/// # Safety
///
/// `number` is null or a live Core Foundation object.
unsafe fn number(number: ffi::CFTypeRef) -> Option<i64> {
    let mut value = 0_i64;
    // SAFETY: the caller's contract; `value` is written as a 64-bit integer.
    unsafe {
        (has_type(number, ffi::CFNumberGetTypeID())
            && ffi::CFNumberGetValue(
                number,
                ffi::K_CF_NUMBER_SINT64_TYPE,
                (&raw mut value).cast(),
            ) != 0)
            .then_some(value)
    }
}

/// An owned Core Foundation object, released on drop.
struct Cf(ffi::CFTypeRef);

impl Cf {
    /// Takes ownership of `object`, which a `Create` or `Copy` call
    /// returned. `None` where it is null.
    fn new(object: ffi::CFTypeRef) -> Option<Self> {
        (!object.is_null()).then_some(Self(object))
    }

    /// The `CFURL` `url` parses as.
    fn url(url: &str) -> Option<Self> {
        // SAFETY: the bytes are live for the call, which copies them.
        Self::new(unsafe {
            ffi::CFURLCreateWithBytes(
                std::ptr::null(),
                url.as_ptr(),
                isize::try_from(url.len()).ok()?,
                ffi::K_CF_STRING_ENCODING_UTF8,
                std::ptr::null(),
            )
        })
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: `self` holds one reference, released once.
        unsafe { ffi::CFRelease(self.0) }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::{Cf, Pac, entries, ffi, routes, run};

    /// The script `text` as a `CFString`.
    fn script(text: &str) -> Cf {
        // SAFETY: the bytes are live for the call, which copies them.
        Cf::new(unsafe {
            ffi::CFStringCreateWithBytes(
                std::ptr::null(),
                text.as_ptr(),
                isize::try_from(text.len()).unwrap(),
                ffi::K_CF_STRING_ENCODING_UTF8,
                0,
            )
        })
        .unwrap()
    }

    /// The entries `pac` gives a request for `target`.
    fn resolve(pac: Pac, target: &str) -> Vec<Option<String>> {
        let target = Cf::url(target).unwrap();
        let list = run(pac, &target).expect("the script answers");
        entries(&list, &target)
    }

    const SCRIPT: &str = "function FindProxyForURL(url, host) {\n\
        if (host == \"direct.example\") return \"DIRECT\";\n\
        if (host == \"socks.example\") return \"SOCKS 127.0.0.1:1080\";\n\
        return \"PROXY 127.0.0.1:3128; PROXY 127.0.0.2:8080; DIRECT\";\n}\n";

    // `CFNetwork` runs the script and returns its list as proxy
    // dictionaries, which read back in the script's order.
    #[test]
    fn a_pac_script_picks_the_routes_per_host_in_order() {
        let pac = script(SCRIPT);
        assert_eq!(
            resolve(Pac::Script(pac.0), "https://other.example/"),
            [
                Some("127.0.0.1:3128".to_owned()),
                Some("127.0.0.2:8080".to_owned()),
                None
            ]
        );
        assert_eq!(
            resolve(Pac::Script(pac.0), "http://direct.example/"),
            [None]
        );
        assert_eq!(
            resolve(Pac::Script(pac.0), "http://socks.example/"),
            [Some("socks://127.0.0.1:1080".to_owned())]
        );
    }

    /// Serve `script` as a PAC file to every request, on loopback, for the
    /// rest of the test process.
    fn serve(script: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                    line.clear();
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ns-proxy-autoconfig\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{script}",
                    script.len()
                );
            }
        });
        format!("http://{address}/proxy.pac")
    }

    // The same script, fetched from a URL as the system settings name it.
    #[test]
    fn a_pac_script_at_a_url_is_fetched_and_run() {
        let url = Cf::url(&serve(SCRIPT)).unwrap();
        assert_eq!(resolve(Pac::Url(url.0), "http://direct.example/"), [None]);
        assert_eq!(
            resolve(Pac::Url(url.0), "http://other.example/")[0],
            Some("127.0.0.1:3128".to_owned())
        );
    }

    #[test]
    fn a_script_that_cannot_be_fetched_gives_nothing() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/proxy.pac", closed.local_addr().unwrap());
        drop(closed);
        let url = Cf::url(&url).unwrap();
        let target = Cf::url("http://example.com/").unwrap();
        assert!(
            run(Pac::Url(url.0), &target).is_none_or(|list| entries(&list, &target).is_empty())
        );
    }

    // A hosted runner's settings, whatever they are, give a route, or fail
    // as a proxy that cannot be used.
    #[test]
    fn the_host_settings_give_a_route() {
        match routes(true, "example.com", 443) {
            Ok(routes) => assert_ne!(routes, []),
            Err(error) => assert_eq!(error.kind(), crate::ErrorKind::Connect),
        }
    }
}
