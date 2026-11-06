//! Requests through an HTTP proxy (docs/design/proxy.md) through the public
//! API, against a loopback proxy the test starts: `http` requests in
//! absolute form, `https` requests through a `CONNECT` tunnel, and the
//! credentials each carries.

mod absolute;
mod connect;
mod server;
