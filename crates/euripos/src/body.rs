//! The body type requests are sent with and responses are read through.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};

use crate::Error;

/// A request or response body: a stream of [`Bytes`] frames that implements
/// [`http_body::Body`].
///
/// Build one from bytes or a string with [`From`], or an empty one with
/// [`Body::empty`].
pub struct Body {
    inner: Inner,
}

enum Inner {
    /// The bytes not yet yielded; `None` once yielded or when empty.
    Full(Option<Bytes>),
}

impl Body {
    /// A body with no bytes.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inner: Inner::Full(None),
        }
    }
}

impl Default for Body {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Inner::Full(bytes) = &self.inner;
        f.debug_struct("Body")
            .field("remaining", &bytes.as_ref().map_or(0, Bytes::len))
            .finish()
    }
}

impl From<Bytes> for Body {
    fn from(bytes: Bytes) -> Self {
        Self {
            inner: Inner::Full((!bytes.is_empty()).then_some(bytes)),
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        Bytes::from(bytes).into()
    }
}

impl From<&'static [u8]> for Body {
    fn from(bytes: &'static [u8]) -> Self {
        Bytes::from_static(bytes).into()
    }
}

impl From<String> for Body {
    fn from(text: String) -> Self {
        Bytes::from(text).into()
    }
}

impl From<&'static str> for Body {
    fn from(text: &'static str) -> Self {
        Bytes::from_static(text.as_bytes()).into()
    }
}

impl From<()> for Body {
    fn from((): ()) -> Self {
        Self::empty()
    }
}

impl http_body::Body for Body {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let Inner::Full(bytes) = &mut self.get_mut().inner;
        Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes))))
    }

    fn is_end_stream(&self) -> bool {
        let Inner::Full(bytes) = &self.inner;
        bytes.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        let Inner::Full(bytes) = &self.inner;
        SizeHint::with_exact(bytes.as_ref().map_or(0, |bytes| bytes.len() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::Body;
    use bytes::Bytes;
    use http_body::Body as _;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    fn frames(mut body: Body) -> Vec<Bytes> {
        let mut cx = Context::from_waker(Waker::noop());
        let mut out = Vec::new();
        while let Poll::Ready(Some(frame)) = Pin::new(&mut body).poll_frame(&mut cx) {
            out.push(
                frame
                    .expect("a full body never fails")
                    .into_data()
                    .expect("data"),
            );
        }
        out
    }

    #[test]
    fn full_body_yields_its_bytes_once_with_an_exact_size() {
        let body = Body::from("hello");
        assert_eq!(body.size_hint().exact(), Some(5));
        assert!(!body.is_end_stream());
        assert_eq!(frames(body), [Bytes::from_static(b"hello")]);
    }

    #[test]
    fn empty_body_is_at_its_end_and_yields_nothing() {
        for body in [Body::empty(), Body::from(()), Body::from(Vec::new())] {
            assert!(body.is_end_stream());
            assert_eq!(body.size_hint().exact(), Some(0));
            assert_eq!(frames(body), Vec::<Bytes>::new());
        }
    }

    #[test]
    fn body_is_send_sync_and_unpin() {
        fn assert_bounds<T: Send + Sync + Unpin + 'static>() {}
        assert_bounds::<Body>();
    }
}
