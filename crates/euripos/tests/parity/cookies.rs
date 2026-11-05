//! The cookie store (RFC 6265).

use euripos::http::{Request, Response, header};
use euripos::{Body, Client};

use crate::server::{Received, client, respond, serve, url};

/// `/login` sets a session cookie and redirects to `/me`; every response
/// says which `Cookie` header reached it.
async fn site(request: Received) -> Response<Body> {
    let cookie = request
        .headers()
        .get(header::COOKIE)
        .map_or("-".to_owned(), |value| {
            value.to_str().expect("ascii").to_owned()
        });
    let mut response = respond(
        if request.uri().path() == "/login" {
            303
        } else {
            200
        },
        cookie,
    );
    if request.uri().path() == "/login" {
        let headers = response.headers_mut();
        headers.insert(header::LOCATION, "/me".parse().expect("a header value"));
        headers.append(
            header::SET_COOKIE,
            "session=abc; Path=/; HttpOnly"
                .parse()
                .expect("a header value"),
        );
        headers.append(
            header::SET_COOKIE,
            "theme=dark; Max-Age=0".parse().expect("a header value"),
        );
    }
    response
}

async fn get(client: &Client, uri: String) -> (u16, String) {
    let response = client
        .send(Request::get(uri).body(()).expect("request"))
        .await
        .expect("a response");
    let status = response.status().as_u16();
    (status, response.into_body().text().await.expect("text"))
}

#[tokio::test]
async fn a_cookie_set_on_a_redirect_reaches_the_next_hop_and_later_requests() {
    let address = serve(site).await;
    let client = client(|builder| builder.cookie_store(true));
    assert_eq!(
        get(&client, url(address, "/login")).await,
        (200, "session=abc".to_owned())
    );
    // A clone shares the store.
    assert_eq!(
        get(&client.clone(), url(address, "/me")).await,
        (200, "session=abc".to_owned())
    );
}

#[tokio::test]
async fn without_the_store_no_cookie_is_kept() {
    let address = serve(site).await;
    let client = client(|builder| builder);
    assert_eq!(
        get(&client, url(address, "/login")).await,
        (200, "-".to_owned())
    );
}

#[tokio::test]
async fn a_cookie_header_the_request_sets_is_sent_alone() {
    let address = serve(site).await;
    let client = client(|builder| builder.cookie_store(true));
    get(&client, url(address, "/login")).await;
    let request = Request::get(url(address, "/me"))
        .header(header::COOKIE, "mine=1")
        .body(())
        .expect("request");
    let response = client.send(request).await.expect("a response");
    assert_eq!(response.into_body().text().await.expect("text"), "mine=1");
}
