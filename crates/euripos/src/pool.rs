//! The connection pool: hyper-util's composable `client::pool` layers over a
//! TCP and TLS connector.
//!
//! One pool entry per origin ([`Dst`]), held in a `map`. Each entry is a
//! `negotiate` service: it connects, and ALPN decides whether the connection
//! becomes an HTTP/2 connection shared by every request through [`Shared`], or
//! an HTTP/1.1 connection that serves one request at a time from a `cache`.
//! See docs/baseline.md, "Connection pool".

use std::error::Error as StdError;
use std::fmt;
use std::future::{Future, poll_fn};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use http::uri::{Authority, Scheme};
use http::{HeaderValue, Request, Response, Uri, header};
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::client::pool::{cache, map::Map, negotiate};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio_rustls::client::TlsStream;
use tower_layer::layer_fn;
use tower_service::Service;

use crate::timeout::{Timeouts, deadline, until};
use crate::tls::{Trust, negotiated_h2};
use crate::{Body, Error, ErrorKind, TimeoutKind};

type BoxError = Box<dyn StdError + Send + Sync>;
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// How many times a request moves to another connection when the one it was
/// given turns out to be closed before anything was sent on it.
const STALE_RETRIES: usize = 3;

/// An origin: the pool key, and what the connector connects to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Dst {
    tls: bool,
    /// The host as a name or an IP address, without IPv6 brackets.
    host: String,
    port: u16,
}

impl Dst {
    /// The origin `uri` names.
    pub(crate) fn from_uri(uri: &Uri) -> Result<Self, Error> {
        let tls = match uri.scheme() {
            Some(scheme) if *scheme == Scheme::HTTPS => true,
            Some(scheme) if *scheme == Scheme::HTTP => false,
            Some(_) => return Err(invalid_uri("the request URI's scheme is not http or https")),
            None => return Err(invalid_uri("the request URI has no scheme")),
        };
        let authority = uri
            .authority()
            .ok_or_else(|| invalid_uri("the request URI has no host"))?;
        let host = authority.host();
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        if host.is_empty() {
            return Err(invalid_uri("the request URI has no host"));
        }
        Ok(Self {
            tls,
            host: host.to_ascii_lowercase(),
            port: authority.port_u16().unwrap_or(if tls { 443 } else { 80 }),
        })
    }
}

/// A request that names no origin cannot be connected anywhere.
fn invalid_uri(reason: &'static str) -> Error {
    Error::with_source(ErrorKind::Connect, reason)
}

/// The pool and the requests sent through it.
pub(crate) struct Pool {
    send: Box<SendFn>,
}

/// Sends a request to an origin, resolving to the response head and the
/// connection it holds until its body ends.
type SendFn = dyn Fn(Dst, Request<Body>) -> BoxFuture<Result<(Response<Incoming>, Lease), Error>>
    + Send
    + Sync;

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool").finish_non_exhaustive()
    }
}

impl Pool {
    pub(crate) fn new(timeouts: Timeouts, trust: Trust) -> Self {
        let connector = Connector { timeouts, trust };
        let map = Map::builder()
            .keys(Dst::clone)
            .values(move |_: &Dst| {
                negotiate::builder()
                    .connect(connector.clone())
                    .inspect(|connection: &Connection| connection.h2)
                    .fallback(layer_fn(|connect| {
                        cache::builder().build(Http1 { connect })
                    }))
                    .upgrade(layer_fn(|connect| {
                        shared(Http2 { connect }, |http2: &Http2Sender| {
                            !http2.0.is_closed()
                        })
                    }))
                    .build()
            })
            .build();
        let map = Arc::new(Mutex::new(map));
        let read_stall = timeouts.read_stall;

        let send = move |dst: Dst, request: Request<Body>| -> BoxFuture<_> {
            let map = map.clone();
            Box::pin(async move {
                let mut attempt = 0;
                loop {
                    let mut origin = {
                        let mut map = map.lock().unwrap_or_else(PoisonError::into_inner);
                        let origin = map.service(&dst);
                        // `Shared` replaces a closed HTTP/2 connection itself,
                        // under its lock.
                        origin
                            .fallback_mut()
                            .retain(|http1: &mut Http1Sender| http1.is_reusable());
                        origin.clone()
                    };
                    poll_fn(|cx| origin.poll_ready(cx))
                        .await
                        .map_err(into_error)?;
                    let mut connection = origin.call(dst.clone()).await.map_err(into_error)?;
                    if let Err(error) = poll_fn(|cx| connection.poll_ready(cx)).await {
                        // Nothing has been sent on it: another connection
                        // may serve the request.
                        if attempt < STALE_RETRIES {
                            attempt += 1;
                            continue;
                        }
                        return Err(into_error(error));
                    }
                    let abandoned = connection
                        .fallback_ref()
                        .map(|http1| http1.inner().abandoned.clone());
                    let mut lease = Lease {
                        connection: Box::new(connection),
                        abandoned,
                        clean: false,
                    };
                    // `connection` moved into the lease, which marks an HTTP/1.1
                    // connection unusable if this future is dropped before the
                    // response arrives. The stall deadline starts here, before
                    // the request body is written, as docs/baseline.md
                    // ("Default timeouts") specifies.
                    let response = until(
                        deadline(read_stall),
                        TimeoutKind::ReadStall,
                        lease.call(request),
                    )
                    .await?;
                    return Ok((response, lease));
                }
            })
        };
        Self {
            send: Box::new(send),
        }
    }

    /// Sends `request` to `dst`.
    pub(crate) fn send(
        &self,
        dst: Dst,
        request: Request<Body>,
    ) -> BoxFuture<Result<(Response<Incoming>, Lease), Error>> {
        (self.send)(dst, request)
    }
}

/// The error `error`, a pool layer's boxed error, stands for.
fn into_error(error: BoxError) -> Error {
    let error = match error.downcast::<Error>() {
        Ok(error) => return *error,
        Err(error) => error,
    };
    let mut cause: Option<&(dyn StdError + 'static)> = Some(&*error);
    let mut kind = ErrorKind::Protocol;
    while let Some(current) = cause {
        if let Some(ours) = current.downcast_ref::<Error>() {
            kind = ours.kind();
            break;
        }
        cause = current.source();
    }
    Error::with_source(kind, error)
}

/// A connection checked out of the pool for one exchange.
///
/// It goes back to the pool when dropped. An HTTP/1.1 connection dropped
/// before its response body ended is marked unusable, so it is closed rather
/// than handed to the next request mid-exchange.
pub(crate) struct Lease {
    connection: Box<dyn Exchange>,
    abandoned: Option<Arc<AtomicBool>>,
    clean: bool,
}

impl Lease {
    fn call(&mut self, request: Request<Body>) -> BoxFuture<Result<Response<Incoming>, Error>> {
        let future = self.connection.call(request);
        Box::pin(async move { future.await.map_err(into_error) })
    }

    /// The exchange completed: the connection may serve the next request.
    pub(crate) fn release(mut self) {
        self.clean = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.clean
            && let Some(abandoned) = &self.abandoned
        {
            abandoned.store(true, Ordering::Release);
        }
    }
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("http1", &self.abandoned.is_some())
            .finish_non_exhaustive()
    }
}

/// A negotiated connection, with its pool types erased.
trait Exchange: Send + Sync {
    fn call(&mut self, request: Request<Body>) -> BoxFuture<Result<Response<Incoming>, BoxError>>;
}

impl<S> Exchange for S
where
    S: Service<Request<Body>, Response = Response<Incoming>, Error = BoxError> + Send + Sync,
    S::Future: Send + 'static,
{
    fn call(&mut self, request: Request<Body>) -> BoxFuture<Result<Response<Incoming>, BoxError>> {
        Box::pin(Service::call(self, request))
    }
}

/// Opens connections: TCP, then TLS for `https`, each under its timeout.
#[derive(Clone)]
struct Connector {
    timeouts: Timeouts,
    trust: Trust,
}

impl Service<Dst> for Connector {
    type Response = Connection;
    type Error = Error;
    type Future = BoxFuture<Result<Connection, Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Dst) -> Self::Future {
        let Self { timeouts, trust } = self.clone();
        Box::pin(async move {
            let tcp = until(
                deadline(timeouts.connect),
                TimeoutKind::Connect,
                connect_tcp(&dst),
            )
            .await?;
            if !dst.tls {
                return Ok(Connection {
                    io: Io::Tcp(tcp),
                    h2: false,
                });
            }
            let tls = until(
                deadline(timeouts.tls),
                TimeoutKind::Tls,
                trust.handshake(&dst.host, tcp),
            )
            .await?;
            Ok(Connection {
                h2: negotiated_h2(&tls),
                io: Io::Tls(Box::new(tls)),
            })
        })
    }
}

/// Resolves `dst` and connects to its addresses in the order the resolver
/// returned them, until one accepts.
async fn connect_tcp(dst: &Dst) -> Result<TcpStream, Error> {
    let addresses = tokio::net::lookup_host((dst.host.as_str(), dst.port))
        .await
        .map_err(|error| Error::with_source(ErrorKind::Connect, error))?;
    let mut last = None;
    for address in addresses {
        match TcpStream::connect(address).await {
            Ok(tcp) => return Ok(tcp),
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(error) => Error::with_source(ErrorKind::Connect, error),
        None => Error::with_source(ErrorKind::Connect, "the host resolved to no address"),
    })
}

/// An open connection, and whether ALPN chose HTTP/2 on it.
struct Connection {
    io: Io,
    h2: bool,
}

/// The byte stream under a connection.
enum Io {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_read(cx, buf),
            Self::Tls(io) => Pin::new(io).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_write(cx, buf),
            Self::Tls(io) => Pin::new(io).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_write_vectored(cx, bufs),
            Self::Tls(io) => Pin::new(io).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Tcp(io) => io.is_write_vectored(),
            Self::Tls(io) => io.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_flush(cx),
            Self::Tls(io) => Pin::new(io).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_shutdown(cx),
            Self::Tls(io) => Pin::new(io).poll_shutdown(cx),
        }
    }
}

/// Makes an HTTP/1.1 connection over each connection `connect` opens.
#[derive(Clone)]
struct Http1<C> {
    connect: C,
}

impl<C> Service<Dst> for Http1<C>
where
    C: Service<Dst, Response = Connection, Error = BoxError>,
    C::Future: Send + 'static,
{
    type Response = Http1Sender;
    type Error = BoxError;
    type Future = BoxFuture<Result<Http1Sender, BoxError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.connect.poll_ready(cx)
    }

    fn call(&mut self, dst: Dst) -> Self::Future {
        let connecting = self.connect.call(dst);
        Box::pin(async move {
            let connection = connecting.await?;
            let (sender, driver) = http1::handshake(TokioIo::new(connection.io)).await?;
            tokio::spawn(async move {
                // The connection's error reaches the request it failed.
                let _ = driver.await;
            });
            Ok(Http1Sender {
                sender,
                abandoned: Arc::default(),
            })
        })
    }
}

/// Makes the one HTTP/2 connection [`Shared`] shares, over the
/// connection the negotiation handed it.
#[derive(Clone)]
struct Http2<C> {
    connect: C,
}

impl<C> Service<()> for Http2<C>
where
    C: Service<(), Response = Connection, Error = BoxError>,
    C::Future: Send + 'static,
{
    type Response = Http2Sender;
    type Error = BoxError;
    type Future = BoxFuture<Result<Http2Sender, BoxError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.connect.poll_ready(cx)
    }

    fn call(&mut self, (): ()) -> Self::Future {
        let connecting = self.connect.call(());
        Box::pin(async move {
            let connection = connecting.await?;
            let (sender, driver) =
                http2::handshake(TokioExecutor::new(), TokioIo::new(connection.io)).await?;
            tokio::spawn(async move {
                // The connection's error reaches the requests it failed.
                let _ = driver.await;
            });
            Ok(Http2Sender(sender))
        })
    }
}

/// One HTTP/2 connection per origin, made by `make` and shared by every
/// request to it while `open` holds for it.
fn shared<M>(make: M, open: fn(&M::Response) -> bool) -> Shared<M>
where
    M: Service<()>,
{
    Shared {
        make,
        open,
        made: Arc::new(AsyncMutex::new(None)),
    }
}

/// Shares the one connection `make` makes between every request.
///
/// A request that finds no connection, or one `open` no longer holds for,
/// makes one while holding the lock, so requests arriving meanwhile wait for
/// it and share it. A request dropped mid-make only releases the lock, and the
/// next one tries again. It replaces hyper-util's `singleton`, whose dropped
/// participants can cancel a later attempt and panic (#37).
struct Shared<M: Service<()>> {
    make: M,
    open: fn(&M::Response) -> bool,
    made: Arc<AsyncMutex<Option<M::Response>>>,
}

impl<M: Service<()> + Clone> Clone for Shared<M> {
    fn clone(&self) -> Self {
        Self {
            make: self.make.clone(),
            open: self.open,
            made: self.made.clone(),
        }
    }
}

impl<M> Service<()> for Shared<M>
where
    M: Service<()> + Clone + Send + 'static,
    M::Response: Clone + Send,
    M::Error: Into<BoxError>,
    M::Future: Send,
{
    type Response = M::Response;
    type Error = BoxError;
    type Future = BoxFuture<Result<M::Response, BoxError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        // `call` checks whether a connection is needed before making one.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (): ()) -> Self::Future {
        let mut maker = self.make.clone();
        let open = self.open;
        let slot = self.made.clone();
        Box::pin(async move {
            let mut made = slot.lock().await;
            // Checked under the lock, so a connection that closed is replaced
            // however busy the lock is.
            match &*made {
                Some(service) if open(service) => return Ok(service.clone()),
                Some(_) => *made = None,
                None => {}
            }
            poll_fn(|cx| maker.poll_ready(cx))
                .await
                .map_err(Into::into)?;
            let service = maker.call(()).await.map_err(Into::into)?;
            *made = Some(service.clone());
            Ok(service)
        })
    }
}

/// Sends requests over one HTTP/1.1 connection, one at a time.
struct Http1Sender {
    sender: http1::SendRequest<Body>,
    /// Set when a request was abandoned mid-exchange, after which the
    /// connection is never reused.
    abandoned: Arc<AtomicBool>,
}

impl Http1Sender {
    fn is_reusable(&self) -> bool {
        !self.sender.is_closed() && !self.abandoned.load(Ordering::Acquire)
    }
}

impl Service<Request<Body>> for Http1Sender {
    type Response = Response<Incoming>;
    type Error = BoxError;
    type Future = BoxFuture<Result<Response<Incoming>, BoxError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        if self.abandoned.load(Ordering::Acquire) {
            return Poll::Ready(Err("the connection was abandoned mid-exchange".into()));
        }
        self.sender.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let sending = self.sender.send_request(origin_form(request));
        Box::pin(async move { Ok(sending.await?) })
    }
}

/// Sends requests over one HTTP/2 connection, many at a time.
#[derive(Clone)]
struct Http2Sender(http2::SendRequest<Body>);

impl Service<Request<Body>> for Http2Sender {
    type Response = Response<Incoming>;
    type Error = BoxError;
    type Future = BoxFuture<Result<Response<Incoming>, BoxError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.0.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        // HTTP/2 takes the scheme and authority from the absolute URI.
        let sending = self.0.send_request(request);
        Box::pin(async move { Ok(sending.await?) })
    }
}

/// `request` as HTTP/1.1 sends it to an origin server: the path and query as
/// the target, and the authority in `Host` unless the caller set one.
fn origin_form(mut request: Request<Body>) -> Request<Body> {
    let uri = request.uri();
    if let Some(authority) = uri.authority()
        && !request.headers().contains_key(header::HOST)
        && let Ok(host) = HeaderValue::from_str(host_header(authority, uri.scheme()).as_str())
    {
        request.headers_mut().insert(header::HOST, host);
    }
    let target = request
        .uri()
        .path_and_query()
        .map_or_else(|| Uri::from_static("/"), |path| Uri::from(path.clone()));
    *request.uri_mut() = target;
    request
}

/// The `Host` header for `authority`: without user information, and without
/// the port when it is the scheme's default.
fn host_header(authority: &Authority, scheme: Option<&Scheme>) -> String {
    let default = match scheme {
        Some(scheme) if *scheme == Scheme::HTTPS => Some(443),
        Some(scheme) if *scheme == Scheme::HTTP => Some(80),
        _ => None,
    };
    match authority.port_u16() {
        Some(port) if Some(port) != default => format!("{}:{port}", authority.host()),
        _ => authority.host().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{BoxError, Dst, origin_form, shared};
    use crate::{Body, ErrorKind};
    use http::{Request, Uri, header};
    use std::future::{Ready, ready};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use tower_service::Service;

    /// Makes a connection, failing the first time it is asked.
    #[derive(Clone, Default)]
    struct FailsFirst {
        calls: Arc<AtomicUsize>,
    }

    impl Service<()> for FailsFirst {
        type Response = u8;
        type Error = BoxError;
        type Future = Ready<Result<u8, BoxError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, (): ()) -> Self::Future {
            ready(match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err("the first connection fails".into()),
                _ => Ok(1),
            })
        }
    }

    /// A request whose attempt to make the shared connection already ended
    /// can be dropped while the next request makes one: the next request
    /// still gets its connection.
    #[tokio::test]
    async fn a_finished_wait_for_the_h2_connection_dropped_late_spares_the_next() {
        let mut pool = shared(FailsFirst::default(), |_| true);
        let mut first = pool.call(());
        assert!((&mut first).await.is_err());
        let second = pool.call(());
        drop(first);
        assert!(second.await.is_ok());
    }

    /// A connection that can be closed, numbered by the order it was made.
    #[derive(Clone)]
    struct Closable {
        number: usize,
        closed: Arc<AtomicBool>,
    }

    /// Makes a [`Closable`], counting how many it made.
    #[derive(Clone, Default)]
    struct MakesClosable {
        made: Arc<AtomicUsize>,
    }

    impl Service<()> for MakesClosable {
        type Response = Closable;
        type Error = BoxError;
        type Future = Ready<Result<Closable, BoxError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, (): ()) -> Self::Future {
            ready(Ok(Closable {
                number: self.made.fetch_add(1, Ordering::SeqCst),
                closed: Arc::default(),
            }))
        }
    }

    /// A shared HTTP/2 connection that closed is replaced at the next
    /// checkout, even when another request held the lock while the request
    /// checking it out arrived.
    #[tokio::test]
    async fn a_closed_h2_connection_is_replaced_even_when_the_lock_was_held() {
        let mut pool = shared(MakesClosable::default(), |connection| {
            !connection.closed.load(Ordering::SeqCst)
        });
        let first = pool.call(()).await.expect("first connection");
        first.closed.store(true, Ordering::SeqCst);
        let held = pool.made.clone().lock_owned().await;
        let checkout = tokio::spawn(pool.call(()));
        tokio::task::yield_now().await;
        drop(held);
        let second = checkout
            .await
            .expect("the checkout task")
            .expect("second connection");
        assert!(!second.closed.load(Ordering::SeqCst));
        assert_eq!(second.number, 1);
    }

    #[test]
    fn origin_is_scheme_host_and_port_with_defaults_filled() {
        let dst = Dst::from_uri(&Uri::from_static("https://Example.com/a")).expect("origin");
        assert_eq!(
            dst,
            Dst {
                tls: true,
                host: "example.com".into(),
                port: 443
            }
        );
        let dst = Dst::from_uri(&Uri::from_static("http://[::1]:8080/")).expect("origin");
        assert_eq!(
            dst,
            Dst {
                tls: false,
                host: "::1".into(),
                port: 8080
            }
        );
    }

    #[test]
    fn a_uri_without_an_http_origin_cannot_connect() {
        for uri in ["/relative", "ftp://example.com/"] {
            let error = Dst::from_uri(&uri.parse().expect("uri")).expect_err(uri);
            assert_eq!(error.kind(), ErrorKind::Connect);
        }
    }

    #[test]
    fn http1_sends_the_path_and_the_host_without_a_default_port() {
        let request = Request::get("http://example.com:80/a?b=c")
            .body(Body::empty())
            .expect("request");
        let request = origin_form(request);
        assert_eq!(request.uri(), "/a?b=c");
        assert_eq!(request.headers()[header::HOST], "example.com");

        let request = Request::get("https://example.com:8443")
            .header(header::HOST, "other")
            .body(Body::empty())
            .expect("request");
        let request = origin_form(request);
        assert_eq!(request.uri(), "/");
        assert_eq!(request.headers()[header::HOST], "other");

        let request = origin_form(
            Request::get("https://[::1]:8443/")
                .body(Body::empty())
                .expect("request"),
        );
        assert_eq!(request.headers()[header::HOST], "[::1]:8443");
    }
}
