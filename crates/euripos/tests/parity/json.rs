//! JSON request and response bodies (RFC 8259).

use euripos::Body;
use euripos::http::{Request, header};

use crate::server::{Received, client, respond, serve, url};

/// Answers with the request's `Content-Type` as a JSON string, followed by
/// the request body it received.
async fn echo_content_type(request: Received) -> euripos::http::Response<Body> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .map_or("", |value| value.to_str().expect("ascii"))
        .to_owned();
    let body = String::from_utf8(request.into_body().to_vec()).expect("utf-8");
    respond(200, format!("[\"{content_type}\", {body}]"))
}

#[tokio::test]
async fn a_json_body_is_sent_as_application_json_and_a_json_response_is_read_back() {
    let address = serve(echo_content_type).await;
    let body = Body::from_json(&serde_json::json!({ "id": 7 })).expect("serializable");
    let request = Request::post(url(address, "/"))
        .body(body)
        .expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    let echoed: (String, serde_json::Value) = response.into_body().json().await.expect("JSON");
    assert_eq!(echoed.0, "application/json");
    assert_eq!(echoed.1, serde_json::json!({ "id": 7 }));
}

#[tokio::test]
async fn a_content_type_the_request_sets_is_kept() {
    let address = serve(echo_content_type).await;
    let body = Body::from_json(&1).expect("serializable");
    let request = Request::post(url(address, "/"))
        .header(header::CONTENT_TYPE, "application/vnd.api+json")
        .body(body)
        .expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    let echoed: (String, u8) = response.into_body().json().await.expect("JSON");
    assert_eq!(echoed, ("application/vnd.api+json".to_owned(), 1));
}
