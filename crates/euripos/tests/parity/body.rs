//! Streaming request bodies through `Body::wrap`, and reading response
//! bodies with `chunk`, `bytes` and `text` (RFC 9112 §6, RFC 9110 §8.6).

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use euripos::http::{Request, header};
use euripos::{Body, ErrorKind};
use http_body::{Frame, SizeHint};
use tokio::sync::mpsc;

use crate::server::{client, respond, serve, url};

/// A request body fed through a channel, of unknown length unless `exact`.
struct Channel {
    receiver: mpsc::Receiver<Result<Bytes, io::Error>>,
    exact: Option<u64>,
}

impl http_body::Body for Channel {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.get_mut()
            .receiver
            .poll_recv(cx)
            .map(|item| item.map(|item| item.map(Frame::data)))
    }

    fn size_hint(&self) -> SizeHint {
        self.exact.map_or_else(SizeHint::new, SizeHint::with_exact)
    }
}

/// A body that yields `pieces` and ends, its length `exact` when given.
fn channel(pieces: Vec<Result<Bytes, io::Error>>, exact: Option<u64>) -> Body {
    let (sender, receiver) = mpsc::channel(pieces.len().max(1));
    for piece in pieces {
        sender.try_send(piece).expect("room for every piece");
    }
    Body::wrap(Channel { receiver, exact })
}

/// Echoes the request body, and its framing in `x-framing`.
async fn echo(request: crate::server::Received) -> euripos::http::Response<Body> {
    let framing = if request.headers().contains_key(header::TRANSFER_ENCODING) {
        "chunked".to_owned()
    } else {
        request
            .headers()
            .get(header::CONTENT_LENGTH)
            .map_or("none".to_owned(), |length| {
                format!("length {}", length.to_str().expect("ascii"))
            })
    };
    let mut response = respond(200, request.into_body());
    response
        .headers_mut()
        .insert("x-framing", framing.parse().expect("a header value"));
    response
}

#[tokio::test]
async fn a_wrapped_body_of_unknown_length_is_sent_chunked_as_it_is_produced() {
    let address = serve(echo).await;
    let body = channel(
        vec![Ok(Bytes::from("hello, ")), Ok(Bytes::from("world"))],
        None,
    );
    let request = Request::post(url(address, "/"))
        .body(body)
        .expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    assert_eq!(response.headers()["x-framing"], "chunked");
    let text = response.into_body().text().await.expect("the echo");
    assert_eq!(text, "hello, world");
}

#[tokio::test]
async fn a_wrapped_body_of_exact_length_is_sent_with_content_length() {
    let address = serve(echo).await;
    let body = channel(vec![Ok(Bytes::from("12345"))], Some(5));
    let request = Request::put(url(address, "/")).body(body).expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    assert_eq!(response.headers()["x-framing"], "length 5");
    assert_eq!(
        response.into_body().bytes().await.expect("the echo"),
        "12345"
    );
}

#[tokio::test]
async fn an_error_from_a_wrapped_body_fails_the_request_as_a_body_error() {
    let address = serve(echo).await;
    let body = channel(
        vec![
            Ok(Bytes::from("partial")),
            Err(io::Error::other("the source broke")),
        ],
        None,
    );
    let request = Request::post(url(address, "/"))
        .body(body)
        .expect("request");
    let error = client(|builder| builder)
        .send(request)
        .await
        .expect_err("the body failed");
    assert_eq!(error.kind(), ErrorKind::Body, "{error:?}");
    let mut cause: Option<&dyn std::error::Error> = Some(&error);
    let mut found = false;
    while let Some(current) = cause {
        found |= current.to_string() == "the source broke";
        cause = current.source();
    }
    assert!(found, "the source's error is in the chain: {error:?}");
}

#[tokio::test]
async fn chunk_yields_the_response_body_piece_by_piece_then_none() {
    let address = serve(|_| async {
        let pieces = vec![Ok(Bytes::from("one")), Ok(Bytes::from("two"))];
        respond(200, channel(pieces, None))
    })
    .await;
    let request = Request::get(url(address, "/")).body(()).expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    let mut body = response.into_body();
    let mut all = Vec::new();
    while let Some(chunk) = body.chunk().await.expect("a piece") {
        assert!(!chunk.is_empty());
        all.extend_from_slice(&chunk);
    }
    assert_eq!(all, b"onetwo");
    assert_eq!(body.chunk().await.expect("still at its end"), None);
}

#[tokio::test]
async fn text_fails_as_decode_when_the_body_is_not_utf8() {
    let address = serve(|_| async { respond(200, &b"\xff\xfe"[..]) }).await;
    let request = Request::get(url(address, "/")).body(()).expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    let error = response.into_body().text().await.expect_err("not UTF-8");
    assert_eq!(error.kind(), ErrorKind::Decode);
}

#[tokio::test]
async fn bytes_of_an_empty_response_is_empty() {
    let address = serve(|_| async { respond(204, ()) }).await;
    let request = Request::get(url(address, "/")).body(()).expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    assert!(
        response
            .into_body()
            .bytes()
            .await
            .expect("no body")
            .is_empty()
    );
}
