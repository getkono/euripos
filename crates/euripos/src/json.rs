//! JSON request and response bodies (RFC 8259), through serde.
//!
//! See docs/design/parity.md, "JSON".

use http::HeaderValue;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Body, Error, ErrorKind};

/// The media type of a JSON body (RFC 8259 §11), which has no charset
/// parameter: JSON exchanged between systems is UTF-8 (§8.1).
const APPLICATION_JSON: HeaderValue = HeaderValue::from_static("application/json");

impl Body {
    /// A body holding `value` serialized as JSON.
    ///
    /// A request sent with it carries `Content-Type: application/json`
    /// unless the request sets its own `Content-Type`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Body`] when `value` cannot be serialized as JSON: its
    /// `Serialize` implementation fails, or it is a map whose keys are not
    /// strings.
    pub fn from_json<T: Serialize + ?Sized>(value: &T) -> Result<Self, Error> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| Error::with_source(ErrorKind::Body, error))?;
        Ok(Self::from(bytes).with_content_type(APPLICATION_JSON))
    }

    /// The whole body, read to its end and deserialized from JSON.
    ///
    /// The body is parsed whatever its `Content-Type` says.
    ///
    /// # Errors
    ///
    /// As [`bytes`](Body::bytes), and [`ErrorKind::Decode`] when the body is
    /// not JSON that deserializes into `T`.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T, Error> {
        let bytes = self.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|error| Error::with_source(ErrorKind::Decode, error))
    }
}

#[cfg(test)]
mod tests {
    use crate::{Body, ErrorKind};
    use std::collections::BTreeMap;

    #[test]
    fn from_json_serializes_and_names_its_media_type() {
        let mut body = Body::from_json(&[1, 2, 3]).expect("serializable");
        let content_type = body.take_content_type().expect("a media type");
        assert_eq!(content_type, "application/json");
    }

    #[test]
    fn a_map_with_non_string_keys_fails_as_a_body_error() {
        let map = BTreeMap::from([((1, 2), "pair")]);
        let error = Body::from_json(&map).expect_err("JSON keys are strings");
        assert_eq!(error.kind(), ErrorKind::Body);
    }

    #[tokio::test]
    async fn json_reads_the_body_and_deserializes_it() {
        let body = Body::from_json(&vec!["a", "b"]).expect("serializable");
        let value: Vec<String> = body.json().await.expect("valid JSON");
        assert_eq!(value, ["a", "b"]);
    }

    #[tokio::test]
    async fn json_that_does_not_fit_the_type_fails_as_decode() {
        let error = Body::from("{\"not\": \"a list\"}")
            .json::<Vec<u8>>()
            .await
            .expect_err("an object is not a list");
        assert_eq!(error.kind(), ErrorKind::Decode);
    }
}
