//! The proxy, through `WinHTTP`: the configuration the facts report, a watch
//! on the settings it is read from, and per-URL resolution of a PAC script
//! or WPAD discovery with `WinHttpGetProxyForUrl`.

use std::ffi::c_void;
use std::io;
use std::sync::Arc;

use tokio::sync::Notify;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, GlobalFree, HANDLE, HGLOBAL, INVALID_HANDLE_VALUE,
};
use windows::Win32::Networking::WinHttp::{
    ERROR_WINHTTP_AUTODETECTION_FAILED, WINHTTP_ACCESS_TYPE_NAMED_PROXY,
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTO_DETECT_TYPE_DHCP, WINHTTP_AUTO_DETECT_TYPE_DNS_A,
    WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS,
    WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WINHTTP_PROXY_INFO, WINHTTP_RESET_NOTIFY_NETWORK_CHANGED,
    WINHTTP_RESET_OUT_OF_PROC, WinHttpCloseHandle, WinHttpGetDefaultProxyConfiguration,
    WinHttpGetIEProxyConfigForCurrentUser, WinHttpGetProxyForUrl, WinHttpOpen,
    WinHttpResetAutoProxy,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME,
    REG_NOTIFY_THREAD_AGNOSTIC, RegCloseKey, RegNotifyChangeKeyValue, RegOpenKeyExW,
};
use windows::Win32::System::Threading::{
    CreateEventW, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEDEFAULT,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

use super::{change, check};
use crate::{NetworkId, ProxyConfig};

/// Where the current user's proxy settings live; `WinHTTP` reads them from
/// here.
const SETTINGS: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");

/// The proxy configuration now: the current user's, or the machine's `WinHTTP`
/// default where the user has none (a service account). `Unknown` where
/// neither can be read.
pub(super) fn config() -> ProxyConfig {
    match user() {
        Ok(user) => user.config(),
        Err(error) if error.raw_os_error() == Some(code(ERROR_FILE_NOT_FOUND.0)) => machine(),
        Err(_) => ProxyConfig::Unknown,
    }
}

/// The current user's settings, as `WinHTTP` reads them.
struct User {
    auto_detect: bool,
    script: Option<String>,
    proxy: Option<String>,
}

impl User {
    /// A PAC script or WPAD discovery is automatic, and wins over a fixed
    /// proxy, as it does in `WinHTTP` and the browsers.
    fn config(&self) -> ProxyConfig {
        if self.auto_detect || self.script.is_some() {
            ProxyConfig::Automatic
        } else if self.proxy.is_some() {
            ProxyConfig::Manual
        } else {
            ProxyConfig::Direct
        }
    }
}

fn user() -> io::Result<User> {
    let mut raw = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
    // SAFETY: `raw` is written by the call; its strings are `WinHTTP`'s, and
    // each is taken (and freed) once below.
    let result = unsafe { WinHttpGetIEProxyConfigForCurrentUser(&raw mut raw) };
    // SAFETY: as above, on success and failure alike.
    let (script, proxy, _bypass) = unsafe {
        (
            take(raw.lpszAutoConfigUrl),
            take(raw.lpszProxy),
            take(raw.lpszProxyBypass),
        )
    };
    result.map_err(io::Error::from)?;
    Ok(User {
        auto_detect: raw.fAutoDetect.as_bool(),
        script,
        proxy,
    })
}

fn machine() -> ProxyConfig {
    let mut raw = WINHTTP_PROXY_INFO::default();
    // SAFETY: `raw` is written by the call; its strings are taken once.
    let result = unsafe { WinHttpGetDefaultProxyConfiguration(&raw mut raw) };
    // SAFETY: as above.
    let (proxy, _bypass) = unsafe { (take(raw.lpszProxy), take(raw.lpszProxyBypass)) };
    match result {
        Ok(()) if raw.dwAccessType == WINHTTP_ACCESS_TYPE_NAMED_PROXY && proxy.is_some() => {
            ProxyConfig::Manual
        }
        Ok(()) => ProxyConfig::Direct,
        Err(_) => ProxyConfig::Unknown,
    }
}

/// The string `WinHTTP` allocated at `string`, freed; `None` where it is null
/// or empty.
///
/// # Safety
///
/// `string` is null or a string `WinHTTP` allocated with `GlobalAlloc`, not
/// yet freed, and not used after this call.
unsafe fn take(string: PWSTR) -> Option<String> {
    if string.is_null() {
        return None;
    }
    // SAFETY: the caller's contract: a live, NUL-terminated string.
    let text = unsafe { string.to_string() }.ok();
    // SAFETY: the caller's contract; `GlobalFree` reports success as an
    // error in this binding (it returns null), so its result is ignored.
    let _ = unsafe { GlobalFree(Some(HGLOBAL(string.0.cast()))) };
    text.filter(|text| !text.trim().is_empty())
}

#[allow(
    clippy::cast_possible_wrap,
    reason = "a Win32 error code is the OS error's bit pattern"
)]
fn code(error: u32) -> i32 {
    error as i32
}

/// A watch on the current user's proxy settings: every change under
/// [`SETTINGS`] wakes the [`Notify`] it was opened with.
pub(super) struct Watch {
    wait: HANDLE,
    /// The key and event the wait callback re-arms the notification with,
    /// from [`Box::into_raw`]; released on drop, once the callback cannot
    /// run.
    context: *mut WatchContext,
}

struct WatchContext {
    key: HKEY,
    event: HANDLE,
    notify: Arc<Notify>,
}

impl Watch {
    /// Watch [`SETTINGS`], waking `notify` on each change.
    pub(super) fn open(notify: &Arc<Notify>) -> io::Result<Self> {
        Self::open_at(SETTINGS, notify)
    }

    /// Watch `path` under `HKEY_CURRENT_USER`.
    fn open_at(path: PCWSTR, notify: &Arc<Notify>) -> io::Result<Self> {
        let mut key = HKEY::default();
        // SAFETY: `path` is a NUL-terminated literal; `key` is written by
        // the call.
        check(unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path, None, KEY_NOTIFY, &raw mut key) })?;
        // An auto-reset event, so each change wakes the wait once.
        // SAFETY: no attributes and no name.
        let event = match unsafe { CreateEventW(None, false, false, PCWSTR::null()) } {
            Ok(event) => event,
            Err(error) => {
                // SAFETY: `key` was opened above and is not used again.
                let _ = unsafe { RegCloseKey(key) };
                return Err(error.into());
            }
        };
        let context = Box::into_raw(Box::new(WatchContext {
            key,
            event,
            notify: Arc::clone(notify),
        }));
        let mut watch = Self {
            wait: HANDLE::default(),
            context,
        };
        // SAFETY: `context` is live until `drop`, which unregisters the wait
        // and waits for its callback before releasing it.
        unsafe {
            arm(&*context)?;
            RegisterWaitForSingleObject(
                &raw mut watch.wait,
                event,
                Some(on_change),
                Some(context.cast_const().cast::<c_void>()),
                u32::MAX,
                WT_EXECUTEDEFAULT,
            )?;
        }
        Ok(watch)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: `wait` is null or the registration this value made;
        // `UnregisterWaitEx` with `INVALID_HANDLE_VALUE` returns once its
        // callback is not running, so `context` is released after its last
        // use, and its key and event are closed once.
        unsafe {
            if !self.wait.is_invalid() {
                let _ = UnregisterWaitEx(self.wait, Some(INVALID_HANDLE_VALUE));
            }
            let context = Box::from_raw(self.context);
            let _ = RegCloseKey(context.key);
            let _ = CloseHandle(context.event);
        }
    }
}

/// Ask for the next change under the watched key to signal the event.
///
/// # Safety
///
/// `context`'s key and event are open.
unsafe fn arm(context: &WatchContext) -> io::Result<()> {
    let filter = REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_THREAD_AGNOSTIC;
    // SAFETY: the caller's contract. Thread-agnostic, so the request
    // outlives whichever thread made it.
    check(unsafe { RegNotifyChangeKeyValue(context.key, true, filter, Some(context.event), true) })
}

/// `WAITORTIMERCALLBACK`: the settings changed. Re-arm the notification,
/// which fires once per request, then wake the monitor, so a change made
/// before it reads is never missed.
unsafe extern "system" fn on_change(context: *mut c_void, _timed_out: bool) {
    let context = context.cast_const().cast::<WatchContext>();
    // SAFETY: `context` is the `WatchContext` `Watch::open_at` registered,
    // alive while the wait is.
    unsafe {
        let _ = arm(&*context);
        change::wake(Arc::as_ptr(&(*context).notify).cast());
    }
}

/// Where a request should go, one choice of a PAC script's ordered list.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the proxy path, #49")
)]
pub(crate) enum Hop {
    /// Straight to the origin.
    Direct,
    /// Through the proxy at this `host:port`.
    Proxy(String),
}

/// How the proxy for a URL is found automatically: WPAD discovery, a PAC
/// script at a URL, or both, discovery first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the proxy path, #49")
)]
pub(crate) struct Auto {
    pub(crate) detect: bool,
    pub(crate) script: Option<String>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the proxy path, #49")
)]
impl Auto {
    /// The current user's automatic configuration; `None` where the user
    /// configured none, or it cannot be read.
    pub(crate) fn current() -> Option<Self> {
        let user = user().ok()?;
        (user.auto_detect || user.script.is_some()).then_some(Self {
            detect: user.auto_detect,
            script: user.script,
        })
    }
}

/// Resolves the proxy for each URL by WPAD discovery or a PAC script,
/// through a `WinHTTP` session whose cached discovery and script are
/// discarded when the network changes.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the proxy path, #49")
)]
pub(crate) struct Resolver {
    session: *mut c_void,
    network: Option<NetworkId>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the proxy path, #49")
)]
impl Resolver {
    /// Open the `WinHTTP` session the resolution runs in.
    pub(crate) fn open() -> io::Result<Self> {
        // SAFETY: literal strings; the session is closed on drop.
        let session = unsafe {
            WinHttpOpen(
                w!("euripos"),
                WINHTTP_ACCESS_TYPE_NO_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        };
        if session.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            session,
            network: None,
        })
    }

    /// The hops for `url` under `auto`, in the order to try them, on
    /// `network`. Blocks while the script is fetched or run, or WPAD
    /// discovers none: call it off the runtime's worker threads.
    ///
    /// Where discovery finds no script, the answer is [`Hop::Direct`], as
    /// in `WinHTTP` and the browsers. A network other than the last one asked
    /// for discards what the session and the `WinHTTP` service cached, so the
    /// proxy is re-read on each network rather than once.
    pub(crate) fn resolve(
        &mut self,
        url: &str,
        auto: &Auto,
        network: Option<NetworkId>,
    ) -> io::Result<Vec<Hop>> {
        if network != self.network {
            // SAFETY: `session` is open.
            let _ = unsafe {
                WinHttpResetAutoProxy(
                    self.session,
                    WINHTTP_RESET_NOTIFY_NETWORK_CHANGED | WINHTTP_RESET_OUT_OF_PROC,
                )
            };
            self.network = network;
        }
        let script = auto.script.as_deref().map(HSTRING::from);
        let mut options = WINHTTP_AUTOPROXY_OPTIONS {
            fAutoLogonIfChallenged: BOOL::from(true),
            ..Default::default()
        };
        if auto.detect {
            options.dwFlags |= WINHTTP_AUTOPROXY_AUTO_DETECT;
            options.dwAutoDetectFlags =
                WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A;
        }
        if let Some(script) = &script {
            options.dwFlags |= WINHTTP_AUTOPROXY_CONFIG_URL;
            options.lpszAutoConfigUrl = PCWSTR(script.as_ptr());
        }
        if options.dwFlags == 0 {
            return Ok(vec![Hop::Direct]);
        }
        let mut info = WINHTTP_PROXY_INFO::default();
        // SAFETY: `session` is open; `options` and the strings it points to
        // outlive the call; `info` is written by it and its strings taken
        // once below.
        let result = unsafe {
            WinHttpGetProxyForUrl(
                self.session,
                &HSTRING::from(url),
                &raw mut options,
                &raw mut info,
            )
        };
        // SAFETY: as above.
        let (proxy, _bypass) = unsafe { (take(info.lpszProxy), take(info.lpszProxyBypass)) };
        match result {
            Ok(()) if info.dwAccessType == WINHTTP_ACCESS_TYPE_NO_PROXY => Ok(vec![Hop::Direct]),
            Ok(()) => Ok(proxy.as_deref().map_or_else(|| vec![Hop::Direct], hops)),
            Err(error)
                if error.code().0
                    == windows::core::HRESULT::from_win32(ERROR_WINHTTP_AUTODETECTION_FAILED).0 =>
            {
                Ok(vec![Hop::Direct])
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for Resolver {
    fn drop(&mut self) {
        // SAFETY: `session` is open and not used again.
        let _ = unsafe { WinHttpCloseHandle(self.session) };
    }
}

/// The hops in a `WinHTTP` proxy list: entries separated by `;` or
/// whitespace, each `host:port`, `scheme=host:port`, or `DIRECT`.
fn hops(list: &str) -> Vec<Hop> {
    list.split(|c: char| c == ';' || c.is_whitespace())
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            if entry.eq_ignore_ascii_case("DIRECT") {
                Hop::Direct
            } else {
                let address = entry.split_once('=').map_or(entry, |(_, address)| address);
                Hop::Proxy(address.to_owned())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Notify;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, REG_OPTION_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
    };
    use windows::core::{HSTRING, PCWSTR};

    use super::{Auto, Hop, Resolver, User, Watch, config, hops};
    use crate::{NetworkId, ProxyConfig};

    #[test]
    fn user_settings_map_to_the_configuration_they_describe() {
        let user = |auto_detect, script: Option<&str>, proxy: Option<&str>| User {
            auto_detect,
            script: script.map(str::to_owned),
            proxy: proxy.map(str::to_owned),
        };
        assert_eq!(user(false, None, None).config(), ProxyConfig::Direct);
        assert_eq!(
            user(false, None, Some("proxy:3128")).config(),
            ProxyConfig::Manual
        );
        assert_eq!(user(true, None, None).config(), ProxyConfig::Automatic);
        assert_eq!(
            user(false, Some("http://wpad/wpad.dat"), Some("proxy:3128")).config(),
            ProxyConfig::Automatic
        );
    }

    #[test]
    fn proxy_lists_parse_in_order() {
        assert_eq!(hops("proxy:3128"), [Hop::Proxy("proxy:3128".into())]);
        assert_eq!(
            hops("a:1; b:2 DIRECT"),
            [
                Hop::Proxy("a:1".into()),
                Hop::Proxy("b:2".into()),
                Hop::Direct
            ]
        );
        assert_eq!(
            hops("http=a:80;https=b:443"),
            [Hop::Proxy("a:80".into()), Hop::Proxy("b:443".into())]
        );
        assert_eq!(hops(" ; "), []);
    }

    // A hosted runner's settings, whatever they are, read as a known
    // configuration, and an automatic one exactly when the resolver finds
    // one to run.
    #[test]
    fn the_host_configuration_reads() {
        let config = config();
        assert_ne!(config, ProxyConfig::Unknown);
        assert_eq!(Auto::current().is_some(), config == ProxyConfig::Automatic);
    }

    #[test]
    fn no_automatic_configuration_is_direct() {
        let mut resolver = Resolver::open().unwrap();
        let hops = resolver
            .resolve("http://example.com/", &Auto::default(), None)
            .unwrap();
        assert_eq!(hops, [Hop::Direct]);
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

    // The issue's PAC evaluation, end to end: `WinHTTP` fetches the script
    // and runs it per URL.
    #[test]
    fn a_pac_script_picks_the_hops_per_url() {
        let script = serve(
            "function FindProxyForURL(url, host) {\n\
             if (host == \"direct.example\") return \"DIRECT\";\n\
             return \"PROXY 127.0.0.1:3128\";\n}\n",
        );
        let auto = Auto {
            detect: false,
            script: Some(script),
        };
        let network = Some(NetworkId::from_key(&1_u32));
        let mut resolver = Resolver::open().unwrap();
        assert_eq!(
            resolver
                .resolve("http://other.example/", &auto, network)
                .unwrap(),
            [Hop::Proxy("127.0.0.1:3128".into())]
        );
        assert_eq!(
            resolver
                .resolve("http://direct.example/", &auto, network)
                .unwrap(),
            [Hop::Direct]
        );
        // A new network discards the cached script, and it is fetched again.
        let network = Some(NetworkId::from_key(&2_u32));
        assert_eq!(
            resolver
                .resolve("http://other.example/", &auto, network)
                .unwrap(),
            [Hop::Proxy("127.0.0.1:3128".into())]
        );
    }

    #[tokio::test]
    async fn a_change_under_the_watched_key_wakes_the_watch() {
        let path = HSTRING::from(format!(r"Software\euripos-test-{}", std::process::id()));
        let mut key = HKEY::default();
        // SAFETY: `key` is written by the call and closed below.
        let created = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &path,
                None,
                PCWSTR::null(),
                REG_OPTION_VOLATILE,
                KEY_ALL_ACCESS,
                None,
                &raw mut key,
                None,
            )
        };
        super::check(created).unwrap();
        let notify = Arc::new(Notify::new());
        let watch = Watch::open_at(PCWSTR(path.as_ptr()), &notify).unwrap();
        for value in ["one", "two"] {
            let data: Vec<u8> = value
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect();
            // SAFETY: `key` is open; `data` is a NUL-terminated UTF-16 string.
            let set =
                unsafe { RegSetValueExW(key, &HSTRING::from("proxy"), None, REG_SZ, Some(&data)) };
            super::check(set).unwrap();
            // Re-armed after the first change, so the second wakes it too.
            tokio::time::timeout(Duration::from_secs(5), notify.notified())
                .await
                .unwrap();
        }
        drop(watch);
        // SAFETY: `key` is open and not used again.
        unsafe {
            let _ = RegCloseKey(key);
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &path);
        }
    }
}
