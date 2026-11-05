//! Following redirects (RFC 9110 §15.4).

use std::net::SocketAddr;

use bytes::Bytes;
use euripos::http::{Method, Request, Response, header};
use euripos::{Body, ErrorKind};

use crate::server::{Received, client, respond, serve, url};

/// A redirect with `status` to `location`.
fn redirect(status: u16, location: &str) -> Response<Body> {
    let mut response = respond(status, ());
    response
        .headers_mut()
        .insert(header::LOCATION, location.parse().expect("a header value"));
    response
}

/// What the final hop saw: its method, path, body and the credentials
/// that reached it.
fn describe(request: &Received) -> String {
    let header = |name| {
        request
            .headers()
            .get(name)
            .map_or("-", |value| value.to_str().expect("ascii"))
    };
    format!(
        "{} {} body={:?} authorization={} cookie={} content-type={}",
        request.method(),
        request.uri().path(),
        String::from_utf8_lossy(request.body()),
        header(header::AUTHORIZATION),
        header(header::COOKIE),
        header(header::CONTENT_TYPE),
    )
}

/// A server that redirects `/start` with `status` to `/end` and describes
/// the request that reaches `/end`.
async fn redirecting(status: u16) -> SocketAddr {
    serve(move |request: Received| async move {
        match request.uri().path() {
            "/start" => redirect(status, "end"),
            _ => respond(200, describe(&request)),
        }
    })
    .await
}

async fn send(request: Request<Body>) -> String {
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    assert_eq!(response.status(), 200);
    response.into_body().text().await.expect("text")
}

fn post(address: SocketAddr, method: Method) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(url(address, "/start"))
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::AUTHORIZATION, "Bearer secret")
        .body(Body::from("payload"))
        .expect("request")
}

#[tokio::test]
async fn a_303_is_followed_with_a_get_and_no_body() {
    let address = redirecting(303).await;
    assert_eq!(
        send(post(address, Method::PUT)).await,
        "GET /end body=\"\" authorization=Bearer secret cookie=- content-type=-"
    );
}

#[tokio::test]
async fn a_302_turns_a_post_into_a_get() {
    let address = redirecting(302).await;
    assert_eq!(
        send(post(address, Method::POST)).await,
        "GET /end body=\"\" authorization=Bearer secret cookie=- content-type=-"
    );
}

#[tokio::test]
async fn a_307_and_a_308_resend_the_method_and_body() {
    for status in [307, 308] {
        let address = redirecting(status).await;
        assert_eq!(
            send(post(address, Method::POST)).await,
            "POST /end body=\"payload\" authorization=Bearer secret cookie=- content-type=text/plain",
            "{status}"
        );
    }
}

#[tokio::test]
async fn a_307_with_a_streaming_body_is_returned_rather_than_followed() {
    let address = redirecting(307).await;
    let stream = Body::wrap(http_body_full("payload"));
    let request = Request::post(url(address, "/start"))
        .body(stream)
        .expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    assert_eq!(response.status(), 307);
}

/// [`describe`], then the `Host` and `Proxy-Authorization` that reached
/// the final hop.
fn describe_origin(request: &Received) -> String {
    let header = |name| {
        request
            .headers()
            .get(name)
            .map_or("-", |value| value.to_str().expect("ascii"))
    };
    format!(
        "{} host={} proxy-authorization={}",
        describe(request),
        header(header::HOST),
        header(header::PROXY_AUTHORIZATION),
    )
}

/// A GET of `/start` on `address` carrying every header a redirect to
/// another origin drops, with `Host: origin.test`.
fn with_credentials(address: SocketAddr) -> Request<Body> {
    Request::get(url(address, "/start"))
        .header(header::AUTHORIZATION, "Bearer secret")
        .header(header::PROXY_AUTHORIZATION, "Basic cHJveHk=")
        .header(header::COOKIE, "session=1")
        .header(header::HOST, "origin.test")
        .body(Body::empty())
        .expect("request")
}

#[tokio::test]
async fn credentials_and_host_are_not_sent_to_another_origin() {
    let other =
        serve(|request: Received| async move { respond(200, describe_origin(&request)) }).await;
    let target = url(other, "/end");
    let address = serve(move |_| {
        let target = target.clone();
        async move { redirect(301, &target) }
    })
    .await;
    assert_eq!(
        send(with_credentials(address)).await,
        format!(
            "GET /end body=\"\" authorization=- cookie=- content-type=- \
             host={other} proxy-authorization=-"
        )
    );
}

#[tokio::test]
async fn credentials_and_host_follow_a_redirect_within_the_origin() {
    let address = serve(|request: Received| async move {
        match request.uri().path() {
            "/start" => redirect(301, "end"),
            _ => respond(200, describe_origin(&request)),
        }
    })
    .await;
    assert_eq!(
        send(with_credentials(address)).await,
        "GET /end body=\"\" authorization=Bearer secret cookie=session=1 content-type=- \
         host=origin.test proxy-authorization=Basic cHJveHk="
    );
}

#[tokio::test]
async fn a_chain_longer_than_the_limit_fails_as_a_redirect_error() {
    // Every path redirects to the next one: /0 -> /1 -> /2 -> ...
    let address = serve(|request: Received| async move {
        let hop: u32 = request.uri().path()[1..].parse().expect("a hop");
        redirect(302, &format!("/{}", hop + 1))
    })
    .await;
    let limited = client(|builder| builder.max_redirects(3));
    let error = limited
        .send(Request::get(url(address, "/0")).body(()).expect("request"))
        .await
        .expect_err("too many redirects");
    assert_eq!(error.kind(), ErrorKind::Redirect);
}

#[tokio::test]
async fn with_no_redirects_allowed_the_redirect_response_is_returned() {
    let address = redirecting(302).await;
    let response = client(|builder| builder.max_redirects(0))
        .send(
            Request::get(url(address, "/start"))
                .body(())
                .expect("request"),
        )
        .await
        .expect("a response");
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()[header::LOCATION], "end");
}

/// A body with known content that `Body::wrap` streams, so it cannot be
/// replayed.
fn http_body_full(
    text: &'static str,
) -> impl http_body::Body<Data = Bytes, Error = std::convert::Infallible> + Send + Sync + 'static {
    Once(Some(Bytes::from_static(text.as_bytes())))
}

struct Once(Option<Bytes>);

impl http_body::Body for Once {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        std::task::Poll::Ready(
            self.get_mut()
                .0
                .take()
                .map(|data| Ok(http_body::Frame::data(data))),
        )
    }
}
