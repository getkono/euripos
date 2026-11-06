//! Each of the client's four timeouts fires against a loopback server that
//! stalls at the point that timeout bounds.
//!
//! Every test sets its timeouts far below the defaults and bounds its own
//! runtime, so none waits for a default to elapse.

use std::future::{Future, poll_fn};
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::{Duration, Instant};

use euripos::http::Request;
use euripos::{Body, Client, Error, ErrorKind, TimeoutKind};
use http_body::Body as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The timeout under test.
const SHORT: Duration = Duration::from_millis(300);

/// Every other timeout: long enough never to fire first.
const LONG: Duration = Duration::from_secs(20);

/// The most any test may take; a timeout that fails to fire fails here.
const BOUND: Duration = Duration::from_secs(10);

/// A client whose timeouts are all [`LONG`] except those `configure` sets.
/// It ignores the platform's proxy, so its loopback requests reach the test
/// server whatever proxy the environment names.
fn client(configure: impl FnOnce(euripos::ClientBuilder) -> euripos::ClientBuilder) -> Client {
    configure(
        Client::builder()
            .no_proxy()
            .connect_timeout(LONG)
            .tls_timeout(LONG)
            .total_timeout(LONG)
            .read_stall_timeout(LONG),
    )
    .build()
}

/// Runs `future`, failing the test if it outlives [`BOUND`], and returns its
/// output with how long it took.
async fn bounded<T>(future: impl Future<Output = T>) -> (T, Duration) {
    let start = Instant::now();
    let output = tokio::time::timeout(BOUND, future)
        .await
        .expect("the timeout under test fired within the test's bound");
    (output, start.elapsed())
}

/// Asserts `result` failed with the `expected` timeout no sooner than
/// [`SHORT`].
fn assert_timed_out<T>(result: Result<T, Error>, elapsed: Duration, expected: TimeoutKind) {
    let error = result.err().expect("the request timed out");
    assert_eq!(error.kind(), ErrorKind::Timeout(expected), "{error:?}");
    assert!(
        elapsed >= SHORT,
        "fired after {elapsed:?}, before its timeout of {SHORT:?}"
    );
}

async fn get(client: &Client, uri: String) -> Result<euripos::http::Response<Body>, Error> {
    client
        .send(Request::get(uri).body(()).expect("request"))
        .await
}

/// Reads `body` to its end, or to its first error.
async fn read_to_end(mut body: Body) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        if let Ok(data) = frame?.into_data() {
            out.extend_from_slice(&data);
        }
    }
    Ok(out)
}

/// A loopback server that runs `serve` on every connection it accepts.
async fn server<F, Fut>(serve: F) -> SocketAddr
where
    F: Fn(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(serve(tcp));
        }
    });
    address
}

/// Reads one request head from `tcp`.
async fn read_head(tcp: &mut TcpStream) {
    let mut head = Vec::new();
    let mut buf = [0; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = tcp.read(&mut buf).await.expect("read the request");
        assert_ne!(read, 0, "the client closed before sending its request");
        head.extend_from_slice(&buf[..read]);
    }
}

/// Holds `tcp` open without writing to it until the client goes away.
async fn hold(mut tcp: TcpStream) {
    let mut buf = [0; 1024];
    while matches!(tcp.read(&mut buf).await, Ok(read) if read > 0) {}
}

/// A loopback address whose listener's accept queue is full, so a new
/// connection to it does not complete, and the connections that fill it.
///
/// The listener's backlog is one and nothing accepts. Connections are opened
/// until one fails to complete within a short wait: the queue is then full,
/// and the kernel drops (or refuses after a delay) the next handshake.
async fn unconnectable() -> (SocketAddr, socket2::Socket, Vec<TcpStream>) {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .expect("socket");
    let any: SocketAddr = "127.0.0.1:0".parse().expect("address");
    socket.bind(&any.into()).expect("bind");
    socket.listen(1).expect("listen");
    let address = socket
        .local_addr()
        .expect("address")
        .as_socket()
        .expect("an IP address");
    let mut filling = Vec::new();
    for _ in 0..64 {
        match tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(address)).await {
            Ok(Ok(tcp)) => filling.push(tcp),
            Ok(Err(_)) | Err(_) => return (address, socket, filling),
        }
    }
    panic!("the accept queue never filled after 64 connections");
}

#[tokio::test]
async fn connect_timeout_fires_while_the_connection_does_not_open() {
    let (address, _listener, _filling) = unconnectable().await;
    let client = client(|builder| builder.connect_timeout(SHORT));
    let (result, elapsed) = bounded(get(&client, format!("http://{address}/"))).await;
    assert_timed_out(result, elapsed, TimeoutKind::Connect);
}

// The client's own provider, so nothing is installed for the process: without
// `crypto-ring` the request would fail before its handshake starts.
#[cfg(feature = "crypto-ring")]
#[tokio::test]
async fn tls_timeout_fires_while_the_server_never_answers_the_client_hello() {
    let address = server(hold).await;
    let client = client(|builder| builder.tls_timeout(SHORT));
    let (result, elapsed) = bounded(get(&client, format!("https://{address}/"))).await;
    assert_timed_out(result, elapsed, TimeoutKind::Tls);
}

#[tokio::test]
async fn read_stall_timeout_fires_while_the_response_head_does_not_arrive() {
    let address = server(|mut tcp| async move {
        read_head(&mut tcp).await;
        hold(tcp).await;
    })
    .await;
    let client = client(|builder| builder.read_stall_timeout(SHORT));
    let (result, elapsed) = bounded(get(&client, format!("http://{address}/"))).await;
    assert_timed_out(result, elapsed, TimeoutKind::ReadStall);
}

#[tokio::test]
async fn read_stall_timeout_fires_while_the_body_stops_arriving() {
    let address = server(|mut tcp| async move {
        read_head(&mut tcp).await;
        let partial = b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nten bytes!";
        tcp.write_all(partial).await.expect("write");
        hold(tcp).await;
    })
    .await;
    let client = client(|builder| builder.read_stall_timeout(SHORT));
    let (result, elapsed) = bounded(async {
        let response = get(&client, format!("http://{address}/"))
            .await
            .expect("the head arrives");
        read_to_end(response.into_body()).await
    })
    .await;
    assert_timed_out(result, elapsed, TimeoutKind::ReadStall);
}

#[tokio::test]
async fn a_connection_whose_request_timed_out_before_its_head_is_not_reused() {
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    let address = server(move |mut tcp| {
        // The first connection never answers; later ones answer in full.
        let first = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        async move {
            read_head(&mut tcp).await;
            if !first {
                let full = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
                tcp.write_all(full).await.expect("write");
            }
            hold(tcp).await;
        }
    })
    .await;
    let client = client(|builder| builder.total_timeout(SHORT));
    let (timed_out, _) = bounded(get(&client, format!("http://{address}/"))).await;
    assert_eq!(
        timed_out.err().map(|error| error.kind()),
        Some(ErrorKind::Timeout(TimeoutKind::Total))
    );
    let (body, _) = bounded(async {
        let response = get(&client, format!("http://{address}/"))
            .await
            .expect("a response on a new connection");
        read_to_end(response.into_body()).await
    })
    .await;
    assert_eq!(body.expect("the body"), b"hello");
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_connection_whose_body_stalled_is_not_reused() {
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    let address = server(move |mut tcp| {
        // The first connection stalls mid-body; later ones answer in full.
        let first = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        async move {
            read_head(&mut tcp).await;
            if first {
                let partial = b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nten bytes!";
                tcp.write_all(partial).await.expect("write");
                hold(tcp).await;
            } else {
                let full = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
                tcp.write_all(full).await.expect("write");
                hold(tcp).await;
            }
        }
    })
    .await;
    let client = client(|builder| builder.read_stall_timeout(SHORT));
    let (stalled, _) = bounded(async {
        let response = get(&client, format!("http://{address}/"))
            .await
            .expect("the head arrives");
        read_to_end(response.into_body()).await
    })
    .await;
    assert_eq!(
        stalled.err().map(|error| error.kind()),
        Some(ErrorKind::Timeout(TimeoutKind::ReadStall))
    );
    let (body, _) = bounded(async {
        let response = get(&client, format!("http://{address}/"))
            .await
            .expect("a response on a new connection");
        read_to_end(response.into_body()).await
    })
    .await;
    assert_eq!(body.expect("the body"), b"hello");
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn total_timeout_fires_while_the_response_head_does_not_arrive() {
    let address = server(|mut tcp| async move {
        read_head(&mut tcp).await;
        hold(tcp).await;
    })
    .await;
    let client = client(|builder| builder.total_timeout(SHORT));
    let (result, elapsed) = bounded(get(&client, format!("http://{address}/"))).await;
    assert_timed_out(result, elapsed, TimeoutKind::Total);
}

#[tokio::test]
async fn total_timeout_fires_on_a_body_that_never_stalls_but_never_ends() {
    let address = server(|mut tcp| async move {
        read_head(&mut tcp).await;
        let head = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";
        if tcp.write_all(head).await.is_err() {
            return;
        }
        // One byte every 20 ms: never a stall, never an end.
        while tcp.write_all(b"1\r\nx\r\n").await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let client = client(|builder| builder.total_timeout(SHORT).read_stall_timeout(SHORT * 10));
    let (result, elapsed) = bounded(async {
        let response = get(&client, format!("http://{address}/"))
            .await
            .expect("the head arrives");
        read_to_end(response.into_body()).await
    })
    .await;
    assert_timed_out(result, elapsed, TimeoutKind::Total);
}

#[tokio::test]
async fn a_request_within_every_timeout_completes_and_reuses_its_connection() {
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    let address = server(move |mut tcp| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            loop {
                let mut head = Vec::new();
                let mut buf = [0; 1024];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match tcp.read(&mut buf).await {
                        Ok(read) if read > 0 => head.extend_from_slice(&buf[..read]),
                        _ => return,
                    }
                }
                let response = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
                if tcp.write_all(response).await.is_err() {
                    return;
                }
            }
        }
    })
    .await;
    let client = client(|builder| {
        builder
            .connect_timeout(SHORT)
            .read_stall_timeout(SHORT)
            .total_timeout(SHORT * 3)
    });
    for _ in 0..3 {
        let (body, _) = bounded(async {
            let response = get(&client, format!("http://{address}/"))
                .await
                .expect("a response");
            read_to_end(response.into_body()).await
        })
        .await;
        assert_eq!(body.expect("the body"), b"hello");
    }
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
}
