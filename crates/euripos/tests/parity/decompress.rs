//! Response decompression (RFC 9110 §8.4, §12.5.3), one Cargo feature per
//! content coding.

use euripos::Body;
use euripos::http::{HeaderValue, Request, Response, header};

use crate::server::{Received, client, respond, serve, url};

/// What every fixture decodes to.
#[cfg(any(feature = "gzip", feature = "brotli", feature = "zstd"))]
fn plain() -> String {
    "hello, world\n".repeat(64)
}

/// Answers with `encoded` as `coding`, after checking the request asked for
/// it, or with the `Accept-Encoding` it got when it did not.
fn encoded(
    coding: &'static str,
    encoded: &'static [u8],
) -> impl Fn(Received) -> std::future::Ready<Response<Body>> + Clone {
    move |request: Received| {
        let accepted = request
            .headers()
            .get(header::ACCEPT_ENCODING)
            .map_or("", |value| value.to_str().expect("ascii"))
            .to_owned();
        let mut response = respond(200, encoded);
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
        response.headers_mut().insert(
            "x-accepted",
            HeaderValue::from_str(&accepted).expect("a header value"),
        );
        std::future::ready(response)
    }
}

/// Sends a GET with `headers` and returns the response.
async fn get(uri: String, headers: &[(header::HeaderName, &'static str)]) -> Response<Body> {
    let mut request = Request::get(uri);
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    client(|builder| builder)
        .send(request.body(()).expect("request"))
        .await
        .expect("a response")
}

/// A response in `coding` arrives decoded, with the encoding's headers
/// gone, when the client asked for it.
#[cfg(any(feature = "gzip", feature = "brotli", feature = "zstd"))]
async fn decodes(coding: &'static str, fixture: &'static [u8]) {
    let address = serve(encoded(coding, fixture)).await;
    let response = get(url(address, "/"), &[]).await;
    let accepted = response.headers()["x-accepted"].to_str().expect("ascii");
    assert!(
        accepted.split(", ").any(|name| name == coding),
        "{accepted}"
    );
    assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    assert_eq!(response.into_body().text().await.expect("decoded"), plain());
}

#[cfg(feature = "gzip")]
#[tokio::test]
async fn a_gzip_response_is_decoded() {
    decodes("gzip", include_bytes!("fixtures/hello.gz")).await;
}

#[cfg(feature = "brotli")]
#[tokio::test]
async fn a_brotli_response_is_decoded() {
    decodes("br", include_bytes!("fixtures/hello.br")).await;
}

#[cfg(feature = "zstd")]
#[tokio::test]
async fn a_zstd_response_is_decoded() {
    decodes("zstd", include_bytes!("fixtures/hello.zst")).await;
}

#[cfg(feature = "gzip")]
#[tokio::test]
async fn a_corrupt_gzip_body_fails_as_decode() {
    let mut corrupt = include_bytes!("fixtures/hello.gz").to_vec();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    let corrupt: &'static [u8] = corrupt.leak();
    let address = serve(encoded("gzip", corrupt)).await;
    let error = get(url(address, "/"), &[])
        .await
        .into_body()
        .bytes()
        .await
        .expect_err("the gzip trailer is wrong");
    assert_eq!(error.kind(), euripos::ErrorKind::Decode);
}

/// A response that has no body (RFC 9110 §6.4.1) is returned as it is,
/// `Content-Encoding` included, rather than failing to decode its empty
/// body.
#[cfg(feature = "gzip")]
#[tokio::test]
async fn a_response_with_no_body_is_returned_undecoded() {
    let address = serve(|request: Received| {
        let status = match request.uri().path() {
            "/204" => 204,
            "/304" => 304,
            _ => 200,
        };
        let mut response = respond(status, ());
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        std::future::ready(response)
    })
    .await;
    for (method, path) in [
        (euripos::http::Method::HEAD, "/"),
        (euripos::http::Method::GET, "/204"),
        (euripos::http::Method::GET, "/304"),
    ] {
        let request = Request::builder()
            .method(method.clone())
            .uri(url(address, path))
            .body(Body::empty())
            .expect("request");
        let response = client(|builder| builder)
            .send(request)
            .await
            .expect("a response");
        assert_eq!(
            response.headers()[header::CONTENT_ENCODING],
            "gzip",
            "{method} {path}"
        );
        assert!(
            response
                .into_body()
                .bytes()
                .await
                .expect("an empty body")
                .is_empty(),
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn a_request_that_sets_accept_encoding_gets_the_body_as_sent() {
    let fixture = include_bytes!("fixtures/hello.gz");
    let address = serve(encoded("gzip", fixture)).await;
    let response = get(url(address, "/"), &[(header::ACCEPT_ENCODING, "gzip")]).await;
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(
        response.into_body().bytes().await.expect("raw"),
        &fixture[..]
    );
}

#[tokio::test]
async fn a_range_request_asks_for_no_coding_and_gets_the_body_as_sent() {
    let fixture = include_bytes!("fixtures/hello.gz");
    let address = serve(encoded("gzip", fixture)).await;
    let response = get(url(address, "/"), &[(header::RANGE, "bytes=0-")]).await;
    assert_eq!(response.headers()["x-accepted"], "");
    assert_eq!(
        response.into_body().bytes().await.expect("raw"),
        &fixture[..]
    );
}

#[cfg(not(any(feature = "gzip", feature = "brotli", feature = "zstd")))]
#[tokio::test]
async fn without_a_coding_feature_no_coding_is_asked_for_or_decoded() {
    let fixture = include_bytes!("fixtures/hello.gz");
    let address = serve(encoded("gzip", fixture)).await;
    let response = get(url(address, "/"), &[]).await;
    assert_eq!(response.headers()["x-accepted"], "");
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(
        response.into_body().bytes().await.expect("raw"),
        &fixture[..]
    );
}
