//! TCP connections through Network.framework's `nw_connection`, as the byte
//! stream hyper runs HTTP over (docs/adr/0003-apple-network-framework.md).
//!
//! The connection is made by name: Network.framework resolves the host,
//! races its addresses, and brings up VPN On Demand where the system asks
//! for it. TLS runs over the stream in `crate::tls`, as on every platform.

use std::ffi::{CString, c_void};
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use block2::RcBlock;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::{NwError, Owned, ffi, queue};
use crate::{Error, ErrorKind};

/// The most one receive asks for.
const RECEIVE_MAX: u32 = 64 * 1024;

/// How many bytes may be handed to Network.framework and not yet reported
/// sent before a write waits. Network.framework buffers whatever it is given,
/// so this is the stream's back-pressure.
const SEND_WINDOW: usize = 256 * 1024;

/// Connect to `host` (a name or an IP address) on `port`, and wait until the
/// connection is ready.
///
/// A connection Network.framework cannot make now (it would wait for a
/// better network, or failed) fails as [`ErrorKind::Connect`] with the
/// reason it gave. Dropping the future cancels the connection.
pub(crate) async fn connect(host: &str, port: u16) -> Result<Stream, Error> {
    let stream =
        Stream::open(host, port).map_err(|error| Error::with_source(ErrorKind::Connect, error))?;
    poll_fn(|cx| stream.poll_ready(cx))
        .await
        .map_err(|error| Error::with_source(ErrorKind::Connect, error))?;
    Ok(stream)
}

/// A TCP connection made by Network.framework, read and written through
/// tokio's I/O traits. Dropping it cancels the connection.
pub(crate) struct Stream {
    connection: Owned,
    shared: Arc<Shared>,
}

/// What the connection's callbacks share with the stream.
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// `Some` once the connection is ready, or failed before it was.
    ready: Option<Result<(), NwError>>,
    /// Woken when `ready` is set.
    ready_waker: Option<Waker>,

    /// Bytes received and not yet read, from `read_at` on.
    received: Vec<u8>,
    read_at: usize,
    /// A receive is outstanding.
    receiving: bool,
    /// The peer closed its side.
    eof: bool,
    read_error: Option<NwError>,
    read_waker: Option<Waker>,

    /// Bytes handed to Network.framework whose send has not completed.
    unsent: usize,
    write_error: Option<NwError>,
    /// The final send (FIN) was made, and whether it completed.
    shutdown: Option<bool>,
    write_waker: Option<Waker>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Wake every task waiting on the connection.
    fn wake_all(&mut self) {
        for waker in [
            self.ready_waker.take(),
            self.read_waker.take(),
            self.write_waker.take(),
        ]
        .into_iter()
        .flatten()
        {
            waker.wake();
        }
    }
}

impl Stream {
    /// Create and start a connection to `host` and `port`, over TCP with
    /// Network.framework's defaults, without its TLS, and preferring no
    /// system proxy.
    fn open(host: &str, port: u16) -> io::Result<Self> {
        let host = CString::new(host)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the host holds a NUL"))?;
        let port = CString::new(port.to_string())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the port holds a NUL"))?;
        // SAFETY: both strings are NUL-terminated and outlive the call; the
        // two configuration blocks are Network.framework's own constants.
        let (endpoint, parameters) = unsafe {
            (
                Owned::new(ffi::nw_endpoint_create_host(host.as_ptr(), port.as_ptr())),
                Owned::new(ffi::nw_parameters_create_secure_tcp(
                    ffi::_nw_parameters_configure_protocol_disable,
                    ffi::_nw_parameters_configure_protocol_default_configuration,
                )),
            )
        };
        let (Some(endpoint), Some(parameters)) = (endpoint, parameters) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Network.framework rejected the host or the parameters",
            ));
        };
        // Proxies are the client's own concern (#49): it reads the system
        // proxy itself and connects to the proxy it chose (`proxy::routes`),
        // so Network.framework must not proxy the connection again.
        // SAFETY: the parameters are live.
        unsafe { ffi::nw_parameters_set_prefer_no_proxy(parameters.0, true) };
        // SAFETY: the endpoint and parameters are live; the connection
        // keeps its own references to them.
        let connection = Owned::new(unsafe { ffi::nw_connection_create(endpoint.0, parameters.0) })
            .ok_or_else(|| io::Error::other("Network.framework made no connection"))?;
        let shared = Arc::new(Shared::default());
        let handler = {
            let shared = shared.clone();
            RcBlock::new(move |state: u32, error: ffi::nw_error_t| {
                // SAFETY: the error is null or live for the call.
                let error = unsafe { NwError::from_raw(error) };
                on_state(&shared, state, error);
            })
        };
        // SAFETY: the connection is live; the handler is copied by the call,
        // and the queue lives as long as the process.
        unsafe {
            ffi::nw_connection_set_queue(connection.0, queue());
            ffi::nw_connection_set_state_changed_handler(connection.0, &handler);
            ffi::nw_connection_start(connection.0);
        }
        Ok(Self { connection, shared })
    }

    /// Ready once the connection is, or has failed.
    fn poll_ready(&self, cx: &Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.shared.lock();
        if let Some(result) = state.ready {
            return Poll::Ready(result.map_err(NwError::to_io));
        }
        state.ready_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Ask for the next bytes the peer sends.
    fn receive(&self) {
        let shared = self.shared.clone();
        let completion = RcBlock::new(
            move |content: ffi::dispatch_data_t,
                  _context: ffi::nw_content_context_t,
                  is_complete: ffi::CBool,
                  error: ffi::nw_error_t| {
                // SAFETY: each is null or live for the call.
                let (bytes, error) = unsafe { (copy_data(content), NwError::from_raw(error)) };
                let mut state = shared.lock();
                state.receiving = false;
                state.received.extend_from_slice(&bytes);
                // On a TCP stream the content completes when the peer closes
                // its side; Network.framework may instead report that as
                // `ENODATA`.
                match error {
                    Some(error) if error == NwError::posix(libc::ENODATA) => state.eof = true,
                    Some(error) => state.read_error = Some(error),
                    None if is_complete != 0 => state.eof = true,
                    None => {}
                }
                if let Some(waker) = state.read_waker.take() {
                    waker.wake();
                }
            },
        );
        // SAFETY: the connection is live; the completion is copied by the
        // call and called once.
        unsafe { ffi::nw_connection_receive(self.connection.0, 1, RECEIVE_MAX, &completion) }
    }

    /// Send `content`, or the end of the stream where it is null.
    ///
    /// # Safety
    ///
    /// `content` is null or a live `dispatch_data_t`.
    unsafe fn send(&self, content: ffi::dispatch_data_t, len: usize) {
        let shared = self.shared.clone();
        let fin = content.is_null();
        let completion = RcBlock::new(move |error: ffi::nw_error_t| {
            // SAFETY: the error is null or live for the call.
            let error = unsafe { NwError::from_raw(error) };
            let mut state = shared.lock();
            state.unsent -= len;
            if fin {
                state.shutdown = Some(true);
            }
            if let Some(error) = error {
                state.write_error.get_or_insert(error);
            }
            if let Some(waker) = state.write_waker.take() {
                waker.wake();
            }
        });
        // SAFETY: the connection is live and `content` is null or live, per
        // the contract; the contexts are Network.framework's constants; the
        // completion is copied by the call and called once.
        unsafe {
            let message = if fin {
                ffi::_nw_content_context_final_send
            } else {
                ffi::_nw_content_context_default_stream
            };
            ffi::nw_connection_send(self.connection.0, content, message, fin, &completion);
        }
    }
}

/// Record a state the connection entered.
fn on_state(shared: &Shared, state: u32, error: Option<NwError>) {
    let mut guard = shared.lock();
    match state {
        ffi::NW_CONNECTION_STATE_READY => {
            guard.ready.get_or_insert(Ok(()));
        }
        // Waiting means Network.framework would retry once the network
        // changes; a request fails now instead, and the caller retries.
        ffi::NW_CONNECTION_STATE_WAITING
        | ffi::NW_CONNECTION_STATE_FAILED
        | ffi::NW_CONNECTION_STATE_CANCELLED => {
            // After it was ready, the outstanding sends and receives
            // complete with the error themselves.
            let error = error.unwrap_or(NwError::posix(libc::ECANCELED));
            guard.ready.get_or_insert(Err(error));
        }
        _ => return,
    }
    guard.wake_all();
}

/// The bytes `content` holds, copied out. Empty where it is null.
///
/// # Safety
///
/// `content` is null or a live `dispatch_data_t`.
unsafe fn copy_data(content: ffi::dispatch_data_t) -> Vec<u8> {
    if content.is_null() {
        return Vec::new();
    }
    let mut buffer: *const c_void = std::ptr::null();
    let mut size = 0;
    // SAFETY: `content` is live; the map is contiguous and valid until it
    // is released, after the copy.
    unsafe {
        let map = ffi::dispatch_data_create_map(content, &raw mut buffer, &raw mut size);
        let bytes = if buffer.is_null() || size == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(buffer.cast::<u8>(), size).to_vec()
        };
        if !map.is_null() {
            ffi::dispatch_release(map);
        }
        bytes
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut state = this.shared.lock();
        if state.read_at < state.received.len() {
            let available = &state.received[state.read_at..];
            let n = available.len().min(buf.remaining());
            buf.put_slice(&available[..n]);
            state.read_at += n;
            if state.read_at == state.received.len() {
                state.received.clear();
                state.read_at = 0;
            }
            return Poll::Ready(Ok(()));
        }
        if let Some(error) = state.read_error {
            return Poll::Ready(Err(error.to_io()));
        }
        if state.eof {
            return Poll::Ready(Ok(()));
        }
        state.read_waker = Some(cx.waker().clone());
        if !state.receiving {
            state.receiving = true;
            drop(state);
            this.receive();
        }
        Poll::Pending
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut state = this.shared.lock();
        if let Some(error) = state.write_error {
            return Poll::Ready(Err(error.to_io()));
        }
        if state.shutdown.is_some() {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if state.unsent >= SEND_WINDOW {
            state.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let len = buf.len().min(SEND_WINDOW);
        state.unsent += len;
        drop(state);
        // SAFETY: `buf` is readable for `len` bytes; with the default
        // destructor (null) they are copied before the call returns.
        let content = unsafe {
            ffi::dispatch_data_create(
                buf.as_ptr().cast(),
                len,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if content.is_null() {
            this.shared.lock().unsent -= len;
            return Poll::Ready(Err(io::ErrorKind::OutOfMemory.into()));
        }
        // SAFETY: `content` is live until it is released below; the send
        // keeps its own reference.
        unsafe {
            this.send(content, len);
            ffi::dispatch_release(content);
        }
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.shared.lock();
        if let Some(error) = state.write_error {
            return Poll::Ready(Err(error.to_io()));
        }
        if state.unsent == 0 {
            return Poll::Ready(Ok(()));
        }
        state.write_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut state = this.shared.lock();
        if let Some(error) = state.write_error {
            return Poll::Ready(Err(error.to_io()));
        }
        match state.shutdown {
            Some(true) => return Poll::Ready(Ok(())),
            Some(false) => {}
            // Everything written goes out before the end of the stream.
            None if state.unsent > 0 => {}
            None => {
                state.shutdown = Some(false);
                drop(state);
                // SAFETY: a null content sends the end of the stream.
                unsafe { this.send(std::ptr::null_mut(), 0) };
                state = this.shared.lock();
                if state.shutdown == Some(true) {
                    return Poll::Ready(Ok(()));
                }
            }
        }
        state.write_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the connection is live until `self.connection` drops after
        // this. Cancelling completes every outstanding callback, which hold
        // only `Shared`.
        unsafe { ffi::nw_connection_cancel(self.connection.0) }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::connect;
    use crate::ErrorKind;

    /// Bounds each test: a connection that never becomes ready fails here.
    const BOUND: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn bytes_go_both_ways_and_each_end_sees_the_other_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            // Until the client's end of the stream.
            tcp.read_to_end(&mut request).await.unwrap();
            tcp.write_all(&request).await.unwrap();
            request
        });
        let mut stream = tokio::time::timeout(BOUND, connect("localhost", port))
            .await
            .unwrap()
            .unwrap();
        // More than the send window, so a write waits for sends to complete.
        let sent: Vec<u8> = (0..1_000_000_u32).map(|i| (i % 251) as u8).collect();
        stream.write_all(&sent).await.unwrap();
        stream.flush().await.unwrap();
        stream.shutdown().await.unwrap();
        let mut echoed = Vec::new();
        tokio::time::timeout(BOUND, stream.read_to_end(&mut echoed))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(server.await.unwrap(), sent);
        assert_eq!(echoed, sent);
    }

    #[tokio::test]
    async fn a_reset_after_ready_fails_reads_then_writes_and_flushes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            tcp.write_all(b"hello").await.unwrap();
            // A zero linger makes the close a reset rather than a FIN.
            socket2::SockRef::from(&tcp)
                .set_linger(Some(Duration::ZERO))
                .unwrap();
            let mut byte = [0; 1];
            // Until the client has read the greeting and asks for more.
            tcp.read_exact(&mut byte).await.unwrap();
            drop(tcp);
        });
        let mut stream = tokio::time::timeout(BOUND, connect("127.0.0.1", port))
            .await
            .unwrap()
            .unwrap();
        let mut greeting = [0; 5];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, b"hello");
        stream.write_all(b"!").await.unwrap();
        stream.flush().await.unwrap();
        server.await.unwrap();

        // The receive outstanding when the reset arrives completes with it.
        let mut rest = [0; 1];
        tokio::time::timeout(BOUND, stream.read(&mut rest))
            .await
            .unwrap()
            .unwrap_err();

        // A write after the reset is handed over, and its failed send fails
        // the next write or flush.
        tokio::time::timeout(BOUND, async {
            for _ in 0..100 {
                if stream.write_all(b"after").await.is_err() || stream.flush().await.is_err() {
                    return;
                }
            }
            panic!("no write or flush failed after the reset");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_refused_connection_fails_as_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = tokio::time::timeout(BOUND, connect("127.0.0.1", port))
            .await
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::Connect);
    }

    #[tokio::test]
    async fn a_host_with_a_nul_fails_as_connect() {
        let error = connect("local\0host", 80).await.err().unwrap();
        assert_eq!(error.kind(), ErrorKind::Connect);
    }
}
