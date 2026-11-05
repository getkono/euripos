//! `multipart/form-data` request bodies (RFC 7578).
//!
//! See docs/design/parity.md, "Multipart forms".

use std::fmt::Write as _;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use http::HeaderValue;

use crate::Body;

/// A `multipart/form-data` body (RFC 7578): named fields, each a text value
/// or a [`Part`], sent in order.
///
/// Convert it into a [`Body`] to send it; the request then carries
/// `Content-Type: multipart/form-data; boundary=...` unless it sets its
/// own. Each part streams as it is sent, so a part wrapping a large source
/// is never held in memory whole.
///
/// ```
/// use euripos::http::{HeaderValue, Request};
/// use euripos::{Body, Form, Part};
///
/// let form = Form::new().text("title", "holiday").part(
///     "photo",
///     Part::new(vec![0xff, 0xd8])
///         .file_name("beach.jpg")
///         .content_type(HeaderValue::from_static("image/jpeg")),
/// );
/// let request = Request::post("https://example.com/upload").body(Body::from(form))?;
/// # Ok::<(), euripos::http::Error>(())
/// ```
#[derive(Debug)]
#[must_use]
pub struct Form {
    boundary: String,
    fields: Vec<(String, Part)>,
}

impl Form {
    /// An empty form with a fresh random boundary.
    pub fn new() -> Self {
        Self {
            boundary: boundary(),
            fields: Vec::new(),
        }
    }

    /// The form with a text field `name` holding `value`, sent with no
    /// `Content-Type`, which RFC 7578 §4.4 reads as `text/plain`.
    pub fn text(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.part(name, Part::new(value.into()))
    }

    /// The form with a field `name` holding `part`.
    pub fn part(mut self, name: impl Into<String>, part: Part) -> Self {
        self.fields.push((name.into(), part));
        self
    }

    /// The boundary that separates the parts: random, and unlikely to
    /// appear in any part's content (RFC 2046 §5.1.1).
    #[must_use]
    pub fn boundary(&self) -> &str {
        &self.boundary
    }
}

impl Default for Form {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Form> for Body {
    fn from(form: Form) -> Self {
        let mut bodies = Vec::with_capacity(form.fields.len() * 2 + 1);
        for (name, part) in form.fields {
            let mut head = format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{}\"",
                form.boundary,
                escape(&name)
            );
            if let Some(file_name) = &part.file_name {
                let _ = write!(head, "; filename=\"{}\"", escape(file_name));
            }
            let mut head = head.into_bytes();
            if let Some(content_type) = &part.content_type {
                head.extend_from_slice(b"\r\nContent-Type: ");
                head.extend_from_slice(content_type.as_bytes());
            }
            head.extend_from_slice(b"\r\n\r\n");
            bodies.push(Body::from(head));
            bodies.push(part.body);
            bodies.push(Body::from(Bytes::from_static(b"\r\n")));
        }
        bodies.push(Body::from(format!("--{}--\r\n", form.boundary)));
        let content_type =
            HeaderValue::try_from(format!("multipart/form-data; boundary={}", form.boundary))
                // A boundary is ASCII letters, digits and `-`, always a valid value.
                .expect("a boundary is a valid header value");
        Body::chain(bodies).with_content_type(content_type)
    }
}

/// One field's content in a [`Form`]: a body, and optionally the file name
/// and media type it is sent with.
#[derive(Debug)]
#[must_use]
pub struct Part {
    body: Body,
    file_name: Option<String>,
    content_type: Option<HeaderValue>,
}

impl Part {
    /// A part holding `body`; any [`Body`] works, including a
    /// [`Body::wrap`] that streams from a file.
    pub fn new(body: impl Into<Body>) -> Self {
        Self {
            body: body.into(),
            file_name: None,
            content_type: None,
        }
    }

    /// The part, sent as the file `name` (the `filename` parameter of
    /// RFC 7578 §4.2).
    pub fn file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = Some(name.into());
        self
    }

    /// The part, sent with `Content-Type: value` (RFC 7578 §4.4).
    pub fn content_type(mut self, value: HeaderValue) -> Self {
        self.content_type = Some(value);
        self
    }
}

/// `value` with `"`, CR and LF percent-encoded, so it fits a quoted-string
/// parameter: the encoding RFC 7578 §2 defers to and HTML's form submission
/// uses. Other characters, including non-ASCII ones, are sent as UTF-8
/// (RFC 7578 §5.1.1).
fn escape(value: &str) -> String {
    value
        .replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// A boundary of `euripos-` and 32 random hexadecimal digits, well inside
/// RFC 2046's limit of 70 characters.
fn boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // `RandomState` is keyed from the operating system's randomness, so
    // hashing a counter with two fresh states yields bits nobody can guess.
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let high = RandomState::new().hash_one(count);
    let low = RandomState::new().hash_one(count);
    format!("euripos-{high:016x}{low:016x}")
}

#[cfg(test)]
mod tests {
    use super::{Form, Part, escape};
    use crate::Body;
    use http::HeaderValue;
    use http_body::Body as _;

    #[tokio::test]
    async fn a_form_encodes_each_field_between_boundaries() {
        let form = Form::new().text("a", "1").part(
            "f",
            Part::new("data")
                .file_name("x.txt")
                .content_type(HeaderValue::from_static("text/plain")),
        );
        let boundary = form.boundary().to_owned();
        let mut body = Body::from(form);
        assert_eq!(
            body.take_content_type().expect("a media type"),
            format!("multipart/form-data; boundary={boundary}").as_str()
        );
        let expected = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"f\"; filename=\"x.txt\"\r\n\
             Content-Type: text/plain\r\n\r\ndata\r\n--{boundary}--\r\n"
        );
        assert_eq!(body.size_hint().exact(), Some(expected.len() as u64));
        assert_eq!(body.text().await.expect("a full body"), expected);
    }

    #[tokio::test]
    async fn a_part_takes_its_bodys_media_type_unless_it_sets_its_own() {
        let json = || Body::from_json(&[1]).expect("serializes");
        let form = Form::new().part("j", Part::new(json())).part(
            "k",
            Part::new(json()).content_type(HeaderValue::from_static("text/plain")),
        );
        let boundary = form.boundary().to_owned();
        let expected = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"j\"\r\n\
             Content-Type: application/json\r\n\r\n[1]\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"k\"\r\n\
             Content-Type: text/plain\r\n\r\n[1]\r\n--{boundary}--\r\n"
        );
        assert_eq!(
            Body::from(form).text().await.expect("a full body"),
            expected
        );
    }

    #[test]
    fn quotes_and_line_breaks_in_names_are_percent_encoded() {
        assert_eq!(escape("a\"b\r\nc é"), "a%22b%0D%0Ac é");
    }

    #[test]
    fn each_form_gets_its_own_boundary() {
        let first = Form::new();
        let second = Form::new();
        assert_ne!(first.boundary(), second.boundary());
        assert!(first.boundary().len() <= 70);
        assert!(
            first
                .boundary()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        );
    }

    #[test]
    fn a_streaming_part_leaves_the_size_unknown() {
        let body = Body::from(Form::new().part("s", Part::new(Body::wrap(Streamed))));
        assert_eq!(body.size_hint().exact(), None);
        assert!(body.size_hint().lower() > 0);
    }

    /// A body that never yields, of unknown length.
    struct Streamed;

    impl http_body::Body for Streamed {
        type Data = bytes::Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, Self::Error>>> {
            std::task::Poll::Pending
        }
    }
}
