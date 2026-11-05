//! The reqwest-parity features (docs/design/parity.md) through the public
//! API, each against a loopback server the test starts. One module per
//! feature.

mod body;
mod cookies;
mod json;
mod multipart;
mod redirect;
mod server;
