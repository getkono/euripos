//! `multipart/form-data` request bodies (RFC 7578).

use bytes::Bytes;
use euripos::http::{HeaderValue, Request, header};
use euripos::{Body, Form, Part};

use crate::server::{Received, client, respond, serve, url};

/// Answers with the request's `Content-Type` on the first line and its body
/// after it.
async fn echo(request: Received) -> euripos::http::Response<Body> {
    let content_type = request.headers()[header::CONTENT_TYPE]
        .to_str()
        .expect("ascii")
        .to_owned();
    let mut out = format!("{content_type}\n").into_bytes();
    out.extend_from_slice(request.body());
    respond(200, out)
}

#[tokio::test]
async fn a_form_arrives_as_multipart_form_data_with_its_parts_in_order() {
    let address = serve(echo).await;
    let form = Form::new().text("title", "holiday").part(
        "photo",
        Part::new(Bytes::from_static(b"\xff\xd8 not quite a jpeg"))
            .file_name("beach \"1\".jpg")
            .content_type(HeaderValue::from_static("image/jpeg")),
    );
    let boundary = form.boundary().to_owned();
    let request = Request::post(url(address, "/"))
        .body(Body::from(form))
        .expect("request");
    let response = client(|builder| builder)
        .send(request)
        .await
        .expect("a response");
    let echoed = response.into_body().bytes().await.expect("the echo");
    let (content_type, body) = echoed.split_at(
        echoed
            .iter()
            .position(|&byte| byte == b'\n')
            .expect("a first line"),
    );
    assert_eq!(
        content_type,
        format!("multipart/form-data; boundary={boundary}").as_bytes()
    );
    let mut expected = format!(
        "\n--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nholiday\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"photo\"; \
         filename=\"beach %221%22.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n"
    )
    .into_bytes();
    expected.extend_from_slice(b"\xff\xd8 not quite a jpeg");
    expected.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    assert_eq!(body, expected);
}
