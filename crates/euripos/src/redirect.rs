//! Following redirects (RFC 9110 §15.4): which responses are followed, the
//! request each one turns into, and resolving `Location` against the
//! request's URI (RFC 3986 §5).
//!
//! See docs/design/parity.md, "Redirects".

use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};

use crate::pool::Dst;

/// How many redirects a request follows by default before failing.
pub(crate) const DEFAULT_MAX_REDIRECTS: u32 = 10;

/// The request a redirect turns into.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Redirect {
    pub(crate) method: Method,
    pub(crate) uri: Uri,
    /// Whether the request body is sent again: `307` and `308` keep it, and
    /// `301` and `302` keep it unless they turn a `POST` into a `GET`.
    pub(crate) keep_body: bool,
}

/// The redirect a response with `status` and `location` to a request of
/// `method` for `uri` asks for, or `None` when the response is the answer:
/// it is not a redirect the client follows, or its `Location` is missing
/// or not an `http` or `https` URI.
pub(crate) fn follow(
    status: StatusCode,
    location: Option<&HeaderValue>,
    method: &Method,
    uri: &Uri,
) -> Option<Redirect> {
    let (method, keep_body) = match status {
        // RFC 9110 §15.4.2 and §15.4.3: a user agent may change POST to GET,
        // as every browser does.
        StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND if method == Method::POST => {
            (Method::GET, false)
        }
        StatusCode::MOVED_PERMANENTLY
        | StatusCode::FOUND
        | StatusCode::TEMPORARY_REDIRECT
        | StatusCode::PERMANENT_REDIRECT => (method.clone(), true),
        // §15.4.4: the target is fetched with GET (HEAD stays HEAD).
        StatusCode::SEE_OTHER if method == Method::HEAD => (Method::HEAD, false),
        StatusCode::SEE_OTHER => (Method::GET, false),
        // 300 needs a choice, 304 answers a conditional request, 305 and
        // 306 are deprecated (§15.4.6, §15.4.7).
        _ => return None,
    };
    let uri = resolve(uri, location?.to_str().ok()?)?;
    matches!(uri.scheme_str(), Some("http" | "https")).then_some(Redirect {
        method,
        uri,
        keep_body,
    })
}

/// Removes from `headers`, the caller's headers for a request redirected
/// from `from` to `to`, those that must not follow it (RFC 9110 §15.4):
/// credentials and `Host` when the origin changes, and the body's headers
/// when the body is not sent again.
pub(crate) fn strip(headers: &mut HeaderMap, from: &Uri, to: &Uri, keep_body: bool) {
    let same_origin = matches!(
        (Dst::from_uri(from), Dst::from_uri(to)),
        (Ok(from), Ok(to)) if from == to
    );
    if !same_origin {
        for name in [
            header::AUTHORIZATION,
            header::PROXY_AUTHORIZATION,
            header::COOKIE,
            header::HOST,
        ] {
            headers.remove(name);
        }
    }
    if !keep_body {
        for name in [
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
            header::CONTENT_ENCODING,
            header::CONTENT_LANGUAGE,
            header::CONTENT_LOCATION,
            header::TRANSFER_ENCODING,
        ] {
            headers.remove(name);
        }
    }
}

/// `reference` resolved against `base` (RFC 3986 §5.2), without its
/// fragment, or `None` when the result is not a valid URI.
pub(crate) fn resolve(base: &Uri, reference: &str) -> Option<Uri> {
    let reference = reference
        .split_once('#')
        .map_or(reference, |(head, _)| head);
    let parts = Reference::parse(reference);
    let base_scheme = base.scheme_str()?;
    let base_authority = base.authority()?.as_str();
    let (scheme, authority, path, query) = if let Some(scheme) = parts.scheme {
        (
            scheme,
            parts.authority.unwrap_or_default(),
            remove_dot_segments(parts.path),
            parts.query,
        )
    } else if let Some(authority) = parts.authority {
        (
            base_scheme,
            authority,
            remove_dot_segments(parts.path),
            parts.query,
        )
    } else if parts.path.is_empty() {
        (
            base_scheme,
            base_authority,
            base.path().to_owned(),
            parts.query.or(base.query()),
        )
    } else if parts.path.starts_with('/') {
        (
            base_scheme,
            base_authority,
            remove_dot_segments(parts.path),
            parts.query,
        )
    } else {
        // §5.2.3: the base's path up to its last segment, then the reference.
        let directory = base.path().rsplit_once('/').map_or("", |(dir, _)| dir);
        let merged = format!("{directory}/{}", parts.path);
        (
            base_scheme,
            base_authority,
            remove_dot_segments(&merged),
            parts.query,
        )
    };
    let path = if path.is_empty() { "/" } else { &path };
    let query = query.map(|query| format!("?{query}")).unwrap_or_default();
    format!("{scheme}://{authority}{path}{query}").parse().ok()
}

/// A URI reference split into its components (RFC 3986 §3, Appendix B).
struct Reference<'a> {
    scheme: Option<&'a str>,
    authority: Option<&'a str>,
    path: &'a str,
    query: Option<&'a str>,
}

impl<'a> Reference<'a> {
    fn parse(reference: &'a str) -> Self {
        let (rest, query) = match reference.split_once('?') {
            Some((rest, query)) => (rest, Some(query)),
            None => (reference, None),
        };
        // A scheme is a letter, then letters, digits, `+`, `-` and `.`,
        // before the first `:` and before any `/` (§3.1, §4.2).
        let (scheme, rest) = match rest.split_once(':') {
            Some((scheme, after))
                if scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
            {
                (Some(scheme), after)
            }
            _ => (None, rest),
        };
        let (authority, path) = match rest.strip_prefix("//") {
            Some(after) => {
                let end = after.find('/').unwrap_or(after.len());
                (Some(&after[..end]), &after[end..])
            }
            None => (None, rest),
        };
        Self {
            scheme,
            authority,
            path,
            query,
        }
    }
}

/// `path` with its `.` and `..` segments applied (RFC 3986 §5.2.4).
fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output = String::with_capacity(path.len());
    while !input.is_empty() {
        if let Some(rest) = input
            .strip_prefix("../")
            .or_else(|| input.strip_prefix("./"))
        {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") || input == "/.." {
            input = if input == "/.." { "/" } else { &input[3..] };
            let cut = output.rfind('/').unwrap_or(0);
            output.truncate(cut);
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..]
                .find('/')
                .map_or(input.len(), |at| at + start);
            output.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{Redirect, follow, remove_dot_segments, resolve, strip};
    use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};

    /// The examples of RFC 3986 §5.4.1 and §5.4.2 that an HTTP client can
    /// meet, against the RFC's base URI (with `http` for its scheme).
    #[test]
    fn references_resolve_as_rfc_3986_section_5_4_shows() {
        let base: Uri = "http://a/b/c/d;p?q".parse().expect("a URI");
        for (reference, expected) in [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g/"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q"),
            ("g#s", "http://a/b/c/g"),
            (";x", "http://a/b/c/;x"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../g", "http://a/g"),
            ("../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("g;x=1/../y", "http://a/b/c/y"),
            ("https://other:8443/x/../y?z", "https://other:8443/y?z"),
        ] {
            assert_eq!(
                resolve(&base, reference).map(|uri| uri.to_string()),
                Some(expected.to_owned()),
                "{reference}"
            );
        }
    }

    #[test]
    fn dot_segments_are_removed_as_section_5_2_4_shows() {
        assert_eq!(remove_dot_segments("/a/b/c/./../../g"), "/a/g");
        assert_eq!(remove_dot_segments("mid/content=5/../6"), "mid/6");
    }

    fn location(value: &'static str) -> HeaderValue {
        HeaderValue::from_static(value)
    }

    #[test]
    fn each_redirect_status_keeps_or_changes_the_method_as_rfc_9110_says() {
        let uri: Uri = "http://a/x".parse().expect("a URI");
        let to: Uri = "http://a/y".parse().expect("a URI");
        let next = |status: u16, method: Method| {
            follow(
                StatusCode::from_u16(status).expect("a status"),
                Some(&location("/y")),
                &method,
                &uri,
            )
        };
        let redirect = |method: Method, keep_body| {
            Some(Redirect {
                method,
                uri: to.clone(),
                keep_body,
            })
        };
        assert_eq!(next(301, Method::POST), redirect(Method::GET, false));
        assert_eq!(next(302, Method::POST), redirect(Method::GET, false));
        assert_eq!(next(302, Method::PUT), redirect(Method::PUT, true));
        assert_eq!(next(303, Method::PUT), redirect(Method::GET, false));
        assert_eq!(next(303, Method::HEAD), redirect(Method::HEAD, false));
        assert_eq!(next(307, Method::POST), redirect(Method::POST, true));
        assert_eq!(next(308, Method::DELETE), redirect(Method::DELETE, true));
        for status in [300, 304, 305, 306, 200, 404] {
            assert_eq!(next(status, Method::GET), None, "{status}");
        }
    }

    #[test]
    fn a_missing_or_non_http_location_is_not_followed() {
        let uri: Uri = "http://a/x".parse().expect("a URI");
        let found = StatusCode::FOUND;
        assert_eq!(follow(found, None, &Method::GET, &uri), None);
        let ftp = location("ftp://a/file");
        assert_eq!(follow(found, Some(&ftp), &Method::GET, &uri), None);
    }

    #[test]
    fn credentials_are_stripped_only_when_the_origin_changes() {
        let headers = || {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, location("Bearer secret"));
            headers.insert(header::COOKIE, location("a=b"));
            headers.insert(header::CONTENT_TYPE, location("text/plain"));
            headers.insert(header::ACCEPT, location("*/*"));
            headers
        };
        let from: Uri = "https://a/x".parse().expect("a URI");
        for (to, kept) in [("https://a:443/y", true), ("http://a/y", false)] {
            let mut stripped = headers();
            strip(&mut stripped, &from, &to.parse().expect("a URI"), true);
            assert_eq!(stripped.contains_key(header::AUTHORIZATION), kept, "{to}");
            assert_eq!(stripped.contains_key(header::COOKIE), kept, "{to}");
            assert!(stripped.contains_key(header::CONTENT_TYPE));
        }
        let mut stripped = headers();
        strip(&mut stripped, &from, &from, false);
        assert!(!stripped.contains_key(header::CONTENT_TYPE));
        assert!(stripped.contains_key(header::AUTHORIZATION));
        assert!(stripped.contains_key(header::ACCEPT));
    }
}
