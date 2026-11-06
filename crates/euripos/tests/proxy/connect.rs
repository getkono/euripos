//! `https` requests, tunnelled through the proxy with `CONNECT`
//! (RFC 9110 §9.3.6). An origin these tests trust over TLS needs a client
//! built with a test root, which the public API does not offer: the full
//! exchange through a tunnel is tested in `src/client.rs`. Here the tunnel
//! reaches an origin that records the TLS handshake starting.

use std::time::{Duration, Instant};

use euripos::http::Request;
use euripos::{Client, ErrorKind, Proxy, TimeoutKind};

use crate::server::{BOUND, client, proxy, silent_proxy};

// The client's own provider, so nothing is installed for the process: without
// `crypto-ring` the request would fail before its handshake starts.
#[cfg(feature = "crypto-ring")]
mod tunnel {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use euripos::http::{Method, Request, header};
    use euripos::{ErrorKind, Proxy};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    use crate::server::{client, proxy};

    /// An origin on loopback that records whether the first byte it received
    /// opens a TLS handshake (RFC 8446 §5.1), then closes the connection.
    async fn tls_origin() -> (u16, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();
        let handshake = Arc::new(AtomicBool::new(false));
        let seen = handshake.clone();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let mut first = [0; 1];
                if tcp.read_exact(&mut first).await.is_ok() && first[0] == 0x16 {
                    seen.store(true, Ordering::SeqCst);
                }
            }
        });
        (port, handshake)
    }

    #[tokio::test]
    async fn an_https_request_opens_a_tunnel_with_the_credentials_and_handshakes_through_it() {
        let proxy = proxy(|_| 200).await;
        let (port, handshake) = tls_origin().await;
        let client = client(
            Proxy::new(&proxy.uri(None))
                .expect("proxy")
                .basic_auth("Aladdin", "open sesame"),
        );
        let error = client
            .send(
                Request::get(format!("https://127.0.0.1:{port}/secret?q"))
                    .body(())
                    .expect("request"),
            )
            .await
            .expect_err("the origin closes the handshake");
        assert_eq!(error.kind(), ErrorKind::Tls, "{error:?}");
        assert!(
            handshake.load(Ordering::SeqCst),
            "the handshake reached the origin"
        );
        let seen = proxy.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, Method::CONNECT);
        // The proxy sees the origin's authority, never the path.
        assert_eq!(seen[0].target, format!("127.0.0.1:{port}"));
        assert_eq!(seen[0].headers[header::HOST], format!("127.0.0.1:{port}"));
        assert_eq!(
            seen[0].headers[header::PROXY_AUTHORIZATION],
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }
}

#[tokio::test]
async fn a_407_to_the_connect_fails_as_proxy_auth_required() {
    let proxy = proxy(|_| 407).await;
    let client = client(Proxy::new(&proxy.uri(None)).expect("proxy"));
    let error = client
        .send(
            Request::get("https://origin.invalid/")
                .body(())
                .expect("request"),
        )
        .await
        .expect_err("the proxy asks for credentials");
    assert_eq!(error.kind(), ErrorKind::ProxyAuthRequired);
    assert_eq!(proxy.seen().len(), 1);
}

/// The connect timeout covers the `CONNECT` exchange (docs/design/proxy.md):
/// a proxy that accepts the TCP connection and never answers fails the
/// request with a connect timeout, not the total timeout.
#[tokio::test]
async fn a_proxy_that_never_answers_the_connect_times_out_as_connect() {
    const SHORT: Duration = Duration::from_millis(300);
    let (address, received) = silent_proxy().await;
    let client = Client::builder()
        .connect_timeout(SHORT)
        .tls_timeout(BOUND)
        .total_timeout(BOUND)
        .read_stall_timeout(BOUND)
        .proxy(Proxy::new(&format!("http://{address}")).expect("proxy"))
        .build();
    let start = Instant::now();
    let error = tokio::time::timeout(
        BOUND,
        client.send(
            Request::get("https://origin.invalid/")
                .body(())
                .expect("request"),
        ),
    )
    .await
    .expect("the connect timeout fired within the test's bound")
    .expect_err("the proxy never answers the CONNECT");
    let elapsed = start.elapsed();
    assert_eq!(
        error.kind(),
        ErrorKind::Timeout(TimeoutKind::Connect),
        "{error:?}"
    );
    assert!(
        elapsed >= SHORT,
        "fired after {elapsed:?}, before its timeout of {SHORT:?}"
    );
    // The stall is in the CONNECT exchange: the TCP connection was made and
    // the request sent.
    let received = received.lock().expect("the record").clone();
    assert!(
        received.starts_with(b"CONNECT origin.invalid:443 "),
        "{:?}",
        String::from_utf8_lossy(&received)
    );
}

#[tokio::test]
async fn a_tunnel_the_proxy_refuses_fails_as_connect() {
    let proxy = proxy(|_| 403).await;
    let client = client(Proxy::new(&proxy.uri(None)).expect("proxy"));
    let error = client
        .send(
            Request::get("https://origin.invalid/")
                .body(())
                .expect("request"),
        )
        .await
        .expect_err("the proxy refuses the tunnel");
    assert_eq!(error.kind(), ErrorKind::Connect);
}
