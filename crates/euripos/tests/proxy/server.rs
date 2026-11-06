//! A loopback HTTP proxy that records what it is sent and answers as the
//! test says, and a loopback origin.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use euripos::http::{HeaderMap, Method, Request, Response, StatusCode, header};
use euripos::{Body, Client, Proxy};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

/// The most any request in these tests may take.
pub(crate) const BOUND: Duration = Duration::from_secs(10);

/// A request as the proxy received it.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: Method,
    /// The request target as sent: an absolute URI, or `host:port` for a
    /// `CONNECT`.
    pub(crate) target: String,
    pub(crate) headers: HeaderMap,
}

/// A proxy on loopback, and what it has been sent.
pub(crate) struct ProxyServer {
    pub(crate) address: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl ProxyServer {
    /// Every request the proxy received, in order.
    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("the record").clone()
    }

    /// `http://ADDRESS`, with `userinfo@` before the address when given.
    pub(crate) fn uri(&self, userinfo: Option<&str>) -> String {
        match userinfo {
            Some(userinfo) => format!("http://{userinfo}@{}", self.address),
            None => format!("http://{}", self.address),
        }
    }
}

/// Starts a proxy that answers each request with the status `answer` gives
/// it. A `CONNECT` answered `200` becomes a tunnel to the authority it names;
/// any other request answered `200` gets the body `via proxy`.
pub(crate) async fn proxy(answer: fn(&Seen) -> u16) -> ProxyServer {
    serve(answer, None).await
}

/// Starts a proxy that answers every request `302 Found` with
/// `Location: location`.
pub(crate) async fn redirecting_proxy(location: String) -> ProxyServer {
    serve(|_| 302, Some(location)).await
}

/// Starts a proxy that answers with the status `answer` gives, and with
/// `Location` when `location` is given.
async fn serve(answer: fn(&Seen) -> u16, location: Option<String>) -> ProxyServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let record = record.clone();
            let location = location.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let record = record.clone();
                    let location = location.clone();
                    async move {
                        let seen = Seen {
                            method: request.method().clone(),
                            target: request.uri().to_string(),
                            headers: request.headers().clone(),
                        };
                        record.lock().expect("the record").push(seen.clone());
                        let status = answer(&seen);
                        if status == 200 && seen.method == Method::CONNECT {
                            tokio::spawn(async move {
                                let Ok(upgraded) = hyper::upgrade::on(request).await else {
                                    return;
                                };
                                let Ok(mut origin) = TcpStream::connect(&seen.target).await else {
                                    return;
                                };
                                let mut client = TokioIo::new(upgraded);
                                let _ =
                                    tokio::io::copy_bidirectional(&mut client, &mut origin).await;
                            });
                        }
                        let body = if status == 200 {
                            Body::from("via proxy")
                        } else {
                            Body::empty()
                        };
                        let mut response = Response::builder()
                            .status(StatusCode::from_u16(status).expect("a status"));
                        if let Some(location) = location {
                            response = response.header(header::LOCATION, location);
                        }
                        let response = response.body(body).expect("a valid response");
                        Ok::<_, Infallible>(response)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    ProxyServer { address, seen }
}

/// Starts a proxy that accepts every connection, keeps the bytes it is sent,
/// and never answers: a `CONNECT` to it stalls until the client gives up.
/// Returns its address and everything received so far across connections.
pub(crate) async fn silent_proxy() -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let received = Arc::new(Mutex::new(Vec::new()));
    let record = received.clone();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let record = record.clone();
            tokio::spawn(async move {
                let mut buf = [0; 1024];
                while let Ok(read @ 1..) = tcp.read(&mut buf).await {
                    record
                        .lock()
                        .expect("the record")
                        .extend_from_slice(&buf[..read]);
                }
            });
        }
    });
    (address, received)
}

/// Starts an origin that answers every request with `direct`.
pub(crate) async fn origin() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = hyper::service::service_fn(|_request: Request<Incoming>| async {
                    Ok::<_, Infallible>(Response::new(Body::from("direct")))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    address
}

/// A client that sends every request through `proxy`, bounded by
/// [`BOUND`].
pub(crate) fn client(proxy: Proxy) -> Client {
    Client::builder().total_timeout(BOUND).proxy(proxy).build()
}

/// The text of `response`'s body.
pub(crate) async fn text(response: Response<Body>) -> String {
    response.into_body().text().await.expect("the body arrives")
}
