//! euripos is an async HTTP client with a platform-agnostic API that adapts
//! its transport to the network it is on.
//!
//! The crate is pre-release. [`Client`] is the plain baseline every later
//! optimization is measured against: HTTP/1.1 and HTTP/2 on hyper, TLS on
//! rustls with the platform verifier, and four finite timeouts, with no
//! adaptive behaviour. Requests and responses are the [`http`] crate's types,
//! re-exported so the versions always match:
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use euripos::http::Request;
//! use euripos::{Client, ErrorKind, TimeoutKind};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::builder()
//!     .connect_timeout(Duration::from_secs(5))
//!     .build();
//! let request = Request::get("https://example.com/").body(())?;
//! match client.send(request).await {
//!     Ok(response) => println!("{}", response.status()),
//!     Err(error) if error.kind() == ErrorKind::Timeout(TimeoutKind::Connect) => {
//!         eprintln!("no connection within 5s");
//!     }
//!     Err(error) => return Err(error.into()),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Errors are classified by [`ErrorKind`], and what the platform reports about
//! the network is read as [`NetworkFacts`], where every fact may be `Unknown`.
//! Nothing in this API differs between platforms.

mod body;
mod client;
mod error;
mod facts;
mod json;
mod marker;
mod monitor;
mod multipart;
mod platform;
mod pool;
mod timeout;
mod tls;

pub use body::Body;
pub use client::{Client, ClientBuilder};
pub use error::{Error, ErrorKind, TimeoutKind};
pub use facts::{
    Constraint, InterfaceKind, IpAvailability, Metering, NetworkFacts, NetworkId, ProxyConfig,
};
pub use multipart::{Form, Part};

/// The [`http`](https://docs.rs/http/1) crate, whose `Request` and `Response`
/// this client sends and returns.
pub use http;
