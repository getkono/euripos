//! `http` requests, sent to the proxy whole (RFC 9112 §3.2.2).

use euripos::http::{Request, header};
use euripos::{Client, ErrorKind, Proxy};

use crate::server::{BOUND, client, origin, proxy, text};

/// An origin no resolver knows (RFC 6761 §6.4): a request for it succeeds
/// only through the proxy.
const UNRESOLVABLE: &str = "http://origin.invalid:8080/a?b=c";

#[tokio::test]
async fn an_http_request_goes_to_the_proxy_in_absolute_form() {
    let proxy = proxy(|_| 200).await;
    let client = client(Proxy::new(&proxy.uri(None)).expect("proxy"));
    let response = client
        .send(Request::get(UNRESOLVABLE).body(()).expect("request"))
        .await
        .expect("a response through the proxy");
    assert_eq!(text(response).await, "via proxy");
    let seen = proxy.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, UNRESOLVABLE);
    assert_eq!(seen[0].headers[header::HOST], "origin.invalid:8080");
    assert!(!seen[0].headers.contains_key(header::PROXY_AUTHORIZATION));
}

#[tokio::test]
async fn credentials_in_the_proxy_uri_are_sent_as_basic() {
    let proxy = proxy(|_| 200).await;
    let uri = proxy.uri(Some("Aladdin:open%20sesame"));
    let client = client(Proxy::new(&uri).expect("proxy"));
    client
        .send(Request::get(UNRESOLVABLE).body(()).expect("request"))
        .await
        .expect("a response through the proxy");
    // RFC 7617 §2's example.
    assert_eq!(
        proxy.seen()[0].headers[header::PROXY_AUTHORIZATION],
        "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
    );
}

#[tokio::test]
async fn basic_auth_replaces_the_uri_credentials_on_every_request() {
    let proxy = proxy(|_| 200).await;
    let configured = Proxy::new(&proxy.uri(Some("old:old")))
        .expect("proxy")
        .basic_auth("Aladdin", "open sesame");
    let client = client(configured);
    for _ in 0..2 {
        client
            .send(Request::get(UNRESOLVABLE).body(()).expect("request"))
            .await
            .expect("a response through the proxy");
    }
    let seen = proxy.seen();
    assert_eq!(seen.len(), 2);
    for request in seen {
        assert_eq!(
            request.headers[header::PROXY_AUTHORIZATION],
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }
}

#[tokio::test]
async fn a_407_from_the_proxy_fails_as_proxy_auth_required() {
    let proxy = proxy(|_| 407).await;
    let client = client(Proxy::new(&proxy.uri(Some("user:wrong"))).expect("proxy"));
    let error = client
        .send(Request::get(UNRESOLVABLE).body(()).expect("request"))
        .await
        .expect_err("the proxy refuses the credentials");
    assert_eq!(error.kind(), ErrorKind::ProxyAuthRequired);
    // Not retried with other credentials.
    assert_eq!(proxy.seen().len(), 1);
}

#[tokio::test]
async fn another_proxy_error_is_returned_as_a_response() {
    let proxy = proxy(|_| 502).await;
    let client = client(Proxy::new(&proxy.uri(None)).expect("proxy"));
    let response = client
        .send(Request::get(UNRESOLVABLE).body(()).expect("request"))
        .await
        .expect("the proxy's answer");
    assert_eq!(response.status(), 502);
}

#[tokio::test]
async fn no_proxy_sends_the_request_to_the_origin() {
    let proxy = proxy(|_| 200).await;
    let origin = origin().await;
    let client = Client::builder()
        .total_timeout(BOUND)
        .proxy(Proxy::new(&proxy.uri(None)).expect("proxy"))
        .no_proxy()
        .build();
    let response = client
        .send(
            Request::get(format!("http://{origin}/"))
                .body(())
                .expect("request"),
        )
        .await
        .expect("a response from the origin");
    assert_eq!(text(response).await, "direct");
    assert!(proxy.seen().is_empty());
}
