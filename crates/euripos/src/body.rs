//! The body type requests are sent with and responses are read through.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body as _, Frame, SizeHint};
use hyper::body::Incoming;
use tokio::time::{Instant, Sleep};

use crate::marker::NotUnwindSafe;
use crate::pool::Lease;
use crate::timeout::deadline;
use crate::{Error, ErrorKind, TimeoutKind};

/// A request or response body: a stream of [`Bytes`] frames that implements
/// [`http_body::Body`].
///
/// Build one from bytes or a string with [`From`], or an empty one with
/// [`Body::empty`]. A response body streams from the connection as it is
/// read, under the client's total and read-stall timeouts.
pub struct Body {
    inner: Inner,
    _unwind: NotUnwindSafe,
}

enum Inner {
    /// The bytes not yet yielded; `None` once yielded or when empty.
    Full(Option<Bytes>),
    /// A response body arriving from the connection.
    Streaming(Box<Streaming>),
}

/// A response body as it arrives, with the timeouts that bound it.
struct Streaming {
    incoming: Incoming,
    /// The connection the body arrives on, released when the body ends;
    /// `None` once it has ended or failed.
    lease: Option<Lease>,
    read_stall: Duration,
    /// Fires when no frame has arrived for `read_stall`.
    stall: Pin<Box<Sleep>>,
    /// Fires at the end of the request's total timeout.
    total: Pin<Box<Sleep>>,
    /// Set once the body has ended or yielded an error.
    done: bool,
}

impl Streaming {
    /// The body ended: its connection may serve the next request.
    fn finish(&mut self) {
        self.done = true;
        if let Some(lease) = self.lease.take() {
            lease.release();
        }
    }

    fn poll_frame(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        let polled = match Pin::new(&mut self.incoming).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                self.stall.as_mut().reset(deadline(self.read_stall));
                // The deadline is checked even when a frame is ready, so a
                // body that always has bytes waiting still ends at it.
                if self.total.as_mut().poll(cx).is_ready() {
                    Err(Error::new(ErrorKind::Timeout(TimeoutKind::Total)))
                } else {
                    if self.incoming.is_end_stream() {
                        self.finish();
                    }
                    return Poll::Ready(Some(Ok(frame)));
                }
            }
            Poll::Ready(Some(Err(error))) => Err(Error::with_source(ErrorKind::Protocol, error)),
            Poll::Ready(None) => {
                self.finish();
                return Poll::Ready(None);
            }
            Poll::Pending => {
                if self.total.as_mut().poll(cx).is_ready() {
                    Err(Error::new(ErrorKind::Timeout(TimeoutKind::Total)))
                } else if self.stall.as_mut().poll(cx).is_ready() {
                    Err(Error::new(ErrorKind::Timeout(TimeoutKind::ReadStall)))
                } else {
                    return Poll::Pending;
                }
            }
        };
        // An error ends the body, and the connection is not reused.
        self.done = true;
        self.lease = None;
        Poll::Ready(Some(polled))
    }
}

impl Body {
    /// The body of a response arriving on `lease`, bounded by `read_stall`
    /// between frames and by `total`.
    pub(crate) fn streaming(
        incoming: Incoming,
        lease: Lease,
        read_stall: Duration,
        total: Instant,
    ) -> Self {
        Self {
            inner: Inner::Streaming(Box::new(Streaming {
                incoming,
                lease: Some(lease),
                read_stall,
                stall: Box::pin(tokio::time::sleep_until(deadline(read_stall))),
                total: Box::pin(tokio::time::sleep_until(total)),
                done: false,
            })),
            _unwind: NotUnwindSafe::default(),
        }
    }

    /// A body with no bytes.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inner: Inner::Full(None),
            _unwind: NotUnwindSafe::default(),
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
        let mut f = f.debug_struct("Body");
        match &self.inner {
            Inner::Full(bytes) => f.field("remaining", &bytes.as_ref().map_or(0, Bytes::len)),
            Inner::Streaming(streaming) => f.field("streaming", &!streaming.done),
        };
        f.finish_non_exhaustive()
    }
}

impl From<Bytes> for Body {
    fn from(bytes: Bytes) -> Self {
        Self {
            inner: Inner::Full((!bytes.is_empty()).then_some(bytes)),
            _unwind: NotUnwindSafe::default(),
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
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match &mut self.get_mut().inner {
            Inner::Full(bytes) => Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes)))),
            Inner::Streaming(streaming) => streaming.poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.inner {
            Inner::Full(bytes) => bytes.is_none(),
            Inner::Streaming(streaming) => streaming.done,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.inner {
            Inner::Full(bytes) => {
                SizeHint::with_exact(bytes.as_ref().map_or(0, |bytes| bytes.len() as u64))
            }
            Inner::Streaming(streaming) if streaming.done => SizeHint::with_exact(0),
            Inner::Streaming(streaming) => streaming.incoming.size_hint(),
        }
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
