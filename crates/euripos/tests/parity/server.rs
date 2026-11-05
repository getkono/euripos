//! A loopback HTTP/1.1 server that answers each request with a handler, and
//! what the tests read requests and responses with.

use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use euripos::http::{Request, Response};
use euripos::{Body, Client};
use http_body::Body as _;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// The most any request in these tests may take.
pub(crate) const BOUND: Duration = Duration::from_secs(10);

/// A request as the server received it, its body read in full.
pub(crate) type Received = Request<Bytes>;

/// Starts a server on loopback whose every response is `handler`'s answer
/// to the request, and returns its address.
pub(crate) async fn serve<F, Fut>(handler: F) -> SocketAddr
where
    F: Fn(Received) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<Body>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let handler = handler.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = read_incoming(body).await;
                        Ok::<_, Infallible>(handler(Request::from_parts(parts, body)).await)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    address
}

/// The whole of a request body as the server receives it.
async fn read_incoming(mut body: Incoming) -> Bytes {
    let mut out = Vec::new();
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        if let Ok(data) = frame.expect("the request body arrives").into_data() {
            out.extend_from_slice(&data);
        }
    }
    out.into()
}

/// A client that bounds each request by [`BOUND`].
pub(crate) fn client(
    configure: impl FnOnce(euripos::ClientBuilder) -> euripos::ClientBuilder,
) -> Client {
    configure(Client::builder().total_timeout(BOUND)).build()
}

/// `http://ADDRESS/PATH`.
pub(crate) fn url(address: SocketAddr, path: &str) -> String {
    format!("http://{address}{path}")
}

/// A response with `status` and `body`.
pub(crate) fn respond(status: u16, body: impl Into<Body>) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(body.into())
        .expect("a valid response")
}
