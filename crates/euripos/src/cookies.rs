//! The cookie store (RFC 6265): `Set-Cookie` responses are stored by the
//! storage model of §5.3, and each request carries the `Cookie` header of
//! §5.4.
//!
//! See docs/design/parity.md, "Cookies".

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::{HeaderValue, Uri};

/// The most cookies the store holds for one domain (RFC 6265 §6.1 asks for
/// at least 50); the oldest goes first.
const PER_DOMAIN: usize = 50;

/// The most cookies the store holds in all (RFC 6265 §6.1 asks for at
/// least 3000); the oldest goes first.
const TOTAL: usize = 3000;

/// The longest name and value a stored cookie may have together (RFC 6265
/// §6.1 asks for at least 4096 bytes).
const MAX_SIZE: usize = 4096;

/// The cookies a client has been sent, shared by its clones.
#[derive(Debug, Default)]
pub(crate) struct Jar {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    cookies: Vec<Cookie>,
    /// Orders cookies by when they were first stored (§5.4 step 2).
    created: u64,
}

/// A stored cookie (RFC 6265 §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cookie {
    name: String,
    value: String,
    /// Lowercase; the request host itself when `host_only`.
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    /// `None` for a session cookie, kept until the client is dropped.
    expires: Option<SystemTime>,
    created: u64,
}

impl Jar {
    /// Stores each cookie in `set_cookies`, the `Set-Cookie` headers of a
    /// response to a request for `uri`.
    pub(crate) fn store<'a>(
        &self,
        uri: &Uri,
        set_cookies: impl IntoIterator<Item = &'a HeaderValue>,
    ) {
        self.store_at(uri, set_cookies, SystemTime::now());
    }

    /// The `Cookie` header a request for `uri` carries, or `None` when no
    /// stored cookie applies to it.
    pub(crate) fn header(&self, uri: &Uri) -> Option<HeaderValue> {
        self.header_at(uri, SystemTime::now())
    }

    fn store_at<'a>(
        &self,
        uri: &Uri,
        set_cookies: impl IntoIterator<Item = &'a HeaderValue>,
        now: SystemTime,
    ) {
        let Some(host) = host(uri) else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for set_cookie in set_cookies {
            let Ok(set_cookie) = set_cookie.to_str() else {
                continue;
            };
            let Some(parsed) = parse(set_cookie, now) else {
                continue;
            };
            state.insert(parsed, &host, uri.path(), now);
        }
    }

    fn header_at(&self, uri: &Uri, now: SystemTime) -> Option<HeaderValue> {
        let host = host(uri)?;
        let secure = uri.scheme_str() == Some("https");
        let path = if uri.path().is_empty() {
            "/"
        } else {
            uri.path()
        };
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .cookies
            .retain(|cookie| cookie.expires.is_none_or(|at| at > now));
        // §5.4 step 1: the cookies whose domain, path and secure flag match.
        let mut matching: Vec<&Cookie> = state
            .cookies
            .iter()
            .filter(|cookie| {
                let domain = if cookie.host_only {
                    host == cookie.domain
                } else {
                    domain_matches(&host, &cookie.domain)
                };
                domain && path_matches(path, &cookie.path) && (secure || !cookie.secure)
            })
            .collect();
        if matching.is_empty() {
            return None;
        }
        // §5.4 step 2: longer paths first, then the earlier created.
        matching.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then(a.created.cmp(&b.created))
        });
        let pairs: Vec<String> = matching
            .iter()
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect();
        HeaderValue::from_str(&pairs.join("; ")).ok()
    }
}

impl State {
    /// Stores `parsed`, received from `host` for a request to `path`
    /// (§5.3 steps 4 to 11).
    fn insert(&mut self, parsed: Parsed, host: &str, path: &str, now: SystemTime) {
        let (domain, host_only) = match parsed.domain {
            Some(domain) => {
                // A domain attribute must cover the host that set it, and
                // may not be a single label such as `com`, the one public
                // suffix the store can recognise without a list.
                if !domain_matches(host, &domain) || (!domain.contains('.') && domain != host) {
                    return;
                }
                (domain, false)
            }
            None => (host.to_owned(), true),
        };
        let path = parsed.path.unwrap_or_else(|| default_path(path));
        let existing = self.cookies.iter().position(|cookie| {
            cookie.name == parsed.name && cookie.domain == domain && cookie.path == path
        });
        // A replaced cookie keeps its creation time (§5.3 step 11).
        let created = if let Some(at) = existing {
            self.cookies.remove(at).created
        } else {
            self.created += 1;
            self.created
        };
        if parsed.expires.is_some_and(|at| at <= now) {
            // An expired cookie is evicted at once (§5.3, last paragraph):
            // this is how a server deletes one.
            return;
        }
        self.cookies.push(Cookie {
            name: parsed.name,
            value: parsed.value,
            domain,
            host_only,
            path,
            secure: parsed.secure,
            expires: parsed.expires,
            created,
        });
        self.evict(now);
    }

    /// Drops expired cookies, then the oldest of a domain over its limit
    /// and the oldest over the store's (§5.3, last paragraph).
    fn evict(&mut self, now: SystemTime) {
        self.cookies
            .retain(|cookie| cookie.expires.is_none_or(|at| at > now));
        let Some(newest) = self.cookies.last() else {
            return;
        };
        let domain = newest.domain.clone();
        while self
            .cookies
            .iter()
            .filter(|cookie| cookie.domain == domain)
            .count()
            > PER_DOMAIN
        {
            self.remove_oldest(|cookie| cookie.domain == domain);
        }
        while self.cookies.len() > TOTAL {
            self.remove_oldest(|_| true);
        }
    }

    fn remove_oldest(&mut self, which: impl Fn(&Cookie) -> bool) {
        if let Some(at) = self
            .cookies
            .iter()
            .enumerate()
            .filter(|(_, cookie)| which(cookie))
            .min_by_key(|(_, cookie)| cookie.created)
            .map(|(at, _)| at)
        {
            self.cookies.remove(at);
        }
    }
}

/// A `Set-Cookie` header as §5.2 parses it.
#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    name: String,
    value: String,
    domain: Option<String>,
    path: Option<String>,
    secure: bool,
    expires: Option<SystemTime>,
}

/// `set_cookie` parsed by RFC 6265 §5.2, received at `now`, or `None` when
/// the user agent ignores it.
fn parse(set_cookie: &str, now: SystemTime) -> Option<Parsed> {
    let (pair, attributes) = set_cookie.split_once(';').unwrap_or((set_cookie, ""));
    let (name, value) = pair.split_once('=')?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() || name.len() + value.len() > MAX_SIZE {
        return None;
    }
    let mut parsed = Parsed {
        name: name.to_owned(),
        value: value.to_owned(),
        domain: None,
        path: None,
        secure: false,
        expires: None,
    };
    let mut max_age = None;
    for attribute in attributes.split(';') {
        let (key, value) = attribute.split_once('=').unwrap_or((attribute, ""));
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("expires") {
            if let Some(at) = parse_date(value) {
                parsed.expires = Some(at);
            }
        } else if key.eq_ignore_ascii_case("max-age") {
            // §5.2.2: a digit or `-`, then digits; zero or less expires now.
            let digits = value.strip_prefix('-').unwrap_or(value);
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                // A delta too large to represent is as good as never.
                let never = || now + Duration::from_secs(u64::from(u32::MAX));
                max_age = Some(match value.parse::<u64>() {
                    _ if value.starts_with('-') => UNIX_EPOCH,
                    Ok(0) => UNIX_EPOCH,
                    Ok(seconds) => now
                        .checked_add(Duration::from_secs(seconds))
                        .unwrap_or_else(never),
                    Err(_) => never(),
                });
            }
        } else if key.eq_ignore_ascii_case("domain") {
            // §5.2.3: an empty value is ignored; a leading dot is dropped.
            let domain = value.strip_prefix('.').unwrap_or(value);
            if !domain.is_empty() {
                parsed.domain = Some(domain.to_ascii_lowercase());
            }
        } else if key.eq_ignore_ascii_case("path") {
            // §5.2.4: a value that is not absolute means the default path.
            parsed.path = value.starts_with('/').then(|| value.to_owned());
        } else if key.eq_ignore_ascii_case("secure") {
            parsed.secure = true;
        }
        // HttpOnly and SameSite govern scripts and sites, which a client
        // without either has no use for; other attributes are ignored.
    }
    // §5.3 step 3: Max-Age wins over Expires.
    if max_age.is_some() {
        parsed.expires = max_age;
    }
    Some(parsed)
}

/// The host of `uri`, lowercase and without IPv6 brackets.
fn host(uri: &Uri) -> Option<String> {
    let host = uri.host()?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `host` domain-matches `domain` (RFC 6265 §5.1.3): they are
/// equal, or `host` is a name that ends in `.` and `domain`.
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (host.parse::<std::net::IpAddr>().is_err()
            && host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.')))
}

/// The default path of a cookie set by a request to `path` (RFC 6265
/// §5.1.4): the path up to, not including, its last `/`, or `/`.
fn default_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(at) => path[..at].to_owned(),
    }
}

/// Whether a request for `path` path-matches `cookie_path` (§5.1.4).
fn path_matches(path: &str, cookie_path: &str) -> bool {
    path == cookie_path
        || (path.starts_with(cookie_path)
            && (cookie_path.ends_with('/') || path[cookie_path.len()..].starts_with('/')))
}

/// A cookie date parsed by the algorithm of RFC 6265 §5.1.1, which accepts
/// every format servers send: IMF-fixdate, RFC 850, asctime and Netscape's
/// dashed form.
fn parse_date(value: &str) -> Option<SystemTime> {
    let is_delimiter = |c: char| matches!(c, '\x09' | '\x20'..='\x2f' | '\x3b'..='\x40' | '\x5b'..='\x60' | '\x7b'..='\x7e');
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for token in value.split(is_delimiter).filter(|token| !token.is_empty()) {
        if time.is_none()
            && let Some(parsed) = parse_time(token)
        {
            time = Some(parsed);
        } else if day.is_none()
            && let Some((parsed, _)) = leading_digits(token, 1, 2)
        {
            day = Some(parsed);
        } else if month.is_none()
            && let Some(parsed) = parse_month(token)
        {
            month = Some(parsed);
        } else if year.is_none()
            && let Some((parsed, _)) = leading_digits(token, 2, 4)
        {
            year = Some(parsed);
        }
    }
    let (hour, minute, second) = time?;
    let (day, month, mut year) = (day?, month?, year?);
    if (70..=99).contains(&year) {
        year += 1900;
    } else if year <= 69 {
        year += 2000;
    }
    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(i64::from(year), month, day);
    let seconds = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second);
    match u64::try_from(seconds) {
        Ok(after) => UNIX_EPOCH.checked_add(Duration::from_secs(after)),
        Err(_) => UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs())),
    }
}

/// `hh:mm:ss` at the start of `token`, each one or two digits (§5.1.1's
/// `time`).
fn parse_time(token: &str) -> Option<(u32, u32, u32)> {
    let (hour, rest) = leading_digits(token, 1, 2)?;
    let (minute, rest) = leading_digits(rest.strip_prefix(':')?, 1, 2)?;
    let (second, _) = leading_digits(rest.strip_prefix(':')?, 1, 2)?;
    Some((hour, minute, second))
}

/// The month whose name `token` starts with, 1 to 12 (§5.1.1's `month`).
fn parse_month(token: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let prefix = token.get(..3)?;
    let at = MONTHS
        .iter()
        .position(|month| prefix.eq_ignore_ascii_case(month))?;
    u32::try_from(at + 1).ok()
}

/// Between `min` and `max` digits at the start of `token`, not followed by
/// another digit, and what follows them.
fn leading_digits(token: &str, min: usize, max: usize) -> Option<(u32, &str)> {
    let count = token.bytes().take_while(u8::is_ascii_digit).count();
    if !(min..=max).contains(&count) {
        return None;
    }
    Some((token[..count].parse().ok()?, &token[count..]))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::{Jar, PER_DOMAIN, default_path, domain_matches, parse, parse_date, path_matches};
    use http::{HeaderValue, Uri};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn uri(text: &str) -> Uri {
        text.parse().expect("a URI")
    }

    fn store(jar: &Jar, from: &str, set_cookie: &str, now: SystemTime) {
        jar.store_at(
            &uri(from),
            [&HeaderValue::from_str(set_cookie).expect("a header value")],
            now,
        );
    }

    fn header(jar: &Jar, to: &str, now: SystemTime) -> Option<String> {
        jar.header_at(&uri(to), now)
            .map(|value| value.to_str().expect("ascii").to_owned())
    }

    #[test]
    fn every_date_format_servers_send_parses_to_the_same_instant() {
        // 1994-11-06T08:49:37Z, RFC 9110 §5.6.7's example.
        let expected = Some(at(784_111_777));
        for date in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06-Nov-1994 08:49:37 GMT",
        ] {
            assert_eq!(parse_date(date), expected, "{date}");
        }
    }

    #[test]
    fn invalid_dates_do_not_parse() {
        for date in [
            "Sun, 31 Feb 2021 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1600 08:49:37 GMT",
            "06 Nov 1994",
            "tomorrow",
        ] {
            assert_eq!(parse_date(date), None, "{date}");
        }
    }

    #[test]
    fn max_age_wins_over_expires_and_zero_expires_at_once() {
        let now = at(1_000_000);
        let parsed = parse(
            "a=b; Expires=Sun, 06 Nov 1994 08:49:37 GMT; Max-Age=60",
            now,
        )
        .expect("a cookie");
        assert_eq!(parsed.expires, Some(now + Duration::from_secs(60)));
        let parsed = parse("a=b; max-age=0", now).expect("a cookie");
        assert_eq!(parsed.expires, Some(UNIX_EPOCH));
        let parsed = parse("a=b; max-age=soon", now).expect("a cookie");
        assert_eq!(parsed.expires, None);
    }

    #[test]
    fn a_pair_without_a_name_or_an_equals_sign_is_ignored() {
        let now = at(0);
        assert!(parse("=b", now).is_none());
        assert!(parse("lonely", now).is_none());
        let parsed = parse(" a = b ; Secure; Path=/x; Domain=.Example.COM", now).expect("a cookie");
        assert_eq!((parsed.name.as_str(), parsed.value.as_str()), ("a", "b"));
        assert!(parsed.secure);
        assert_eq!(parsed.path.as_deref(), Some("/x"));
        assert_eq!(parsed.domain.as_deref(), Some("example.com"));
    }

    #[test]
    fn domains_and_paths_match_as_section_5_1_says() {
        assert!(domain_matches("www.example.com", "example.com"));
        assert!(domain_matches("example.com", "example.com"));
        assert!(!domain_matches("badexample.com", "example.com"));
        assert!(!domain_matches("10.0.0.1", "0.0.1"));
        assert!(path_matches("/a/b", "/a"));
        assert!(path_matches("/a/b", "/a/"));
        assert!(path_matches("/a", "/a"));
        assert!(!path_matches("/ab", "/a"));
        assert_eq!(default_path("/a/b/c"), "/a/b");
        assert_eq!(default_path("/a"), "/");
        assert_eq!(default_path(""), "/");
    }

    #[test]
    fn a_host_only_cookie_goes_back_to_its_host_alone() {
        let jar = Jar::default();
        let now = at(0);
        store(&jar, "http://example.com/", "a=1", now);
        assert_eq!(
            header(&jar, "http://example.com/x", now).as_deref(),
            Some("a=1")
        );
        assert_eq!(header(&jar, "http://www.example.com/", now), None);
    }

    #[test]
    fn a_domain_cookie_reaches_subdomains_but_not_other_or_public_domains() {
        let jar = Jar::default();
        let now = at(0);
        store(
            &jar,
            "http://www.example.com/",
            "a=1; Domain=example.com",
            now,
        );
        store(
            &jar,
            "http://www.example.com/",
            "b=2; Domain=other.com",
            now,
        );
        store(&jar, "http://www.example.com/", "c=3; Domain=com", now);
        assert_eq!(
            header(&jar, "http://api.example.com/", now).as_deref(),
            Some("a=1")
        );
        assert_eq!(header(&jar, "http://other.com/", now), None);
        assert_eq!(header(&jar, "http://shop.com/", now), None);
    }

    #[test]
    fn secure_cookies_go_only_over_https_and_longer_paths_come_first() {
        let jar = Jar::default();
        let now = at(0);
        store(&jar, "https://a.test/", "s=1; Secure", now);
        store(&jar, "https://a.test/", "root=2", now);
        store(&jar, "https://a.test/", "deep=3; Path=/docs", now);
        assert_eq!(
            header(&jar, "https://a.test/docs/x", now).as_deref(),
            Some("deep=3; s=1; root=2")
        );
        assert_eq!(
            header(&jar, "http://a.test/", now).as_deref(),
            Some("root=2")
        );
    }

    #[test]
    fn a_cookie_is_replaced_by_name_domain_and_path_and_removed_when_expired() {
        let jar = Jar::default();
        let now = at(100);
        store(&jar, "http://a.test/", "a=1; Max-Age=10", now);
        store(&jar, "http://a.test/", "a=2; Max-Age=10", now);
        assert_eq!(header(&jar, "http://a.test/", now).as_deref(), Some("a=2"));
        assert_eq!(header(&jar, "http://a.test/", at(111)), None);
        store(&jar, "http://a.test/", "b=1", now);
        store(&jar, "http://a.test/", "b=; Max-Age=0", now);
        assert_eq!(header(&jar, "http://a.test/", now), None);
    }

    #[test]
    fn a_domain_keeps_only_its_newest_cookies() {
        let jar = Jar::default();
        let now = at(0);
        for n in 0..=PER_DOMAIN {
            store(&jar, "http://a.test/", &format!("c{n}=v"), now);
        }
        let sent = header(&jar, "http://a.test/", now).expect("cookies");
        assert!(!sent.split("; ").any(|pair| pair == "c0=v"));
        assert_eq!(sent.split("; ").count(), PER_DOMAIN);
    }
}
