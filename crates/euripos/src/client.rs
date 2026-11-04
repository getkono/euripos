//! The client and its builder.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use http::{Request, Response};

use crate::marker::NotUnwindSafe;
use crate::{Body, Error, NetworkFacts};

/// An HTTP client.
///
/// Build one with [`Client::new`] for the defaults or [`Client::builder`] to
/// change a timeout. A `Client` is cheap to clone and is meant to be shared:
/// clones use the same connections.
#[derive(Clone)]
pub struct Client {
    timeouts: Timeouts,
    _unwind: NotUnwindSafe,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client with the default timeouts.
    #[must_use]
    pub fn new() -> Self {
        ClientBuilder::new().build()
    }

    /// A builder that starts from the default timeouts.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Send `request` and resolve to the response once its head has arrived.
    ///
    /// The response body streams as it is read. The returned future owns
    /// everything it needs, so it can be spawned.
    ///
    /// # Errors
    ///
    /// Resolves to an [`Error`] whose [`kind`](Error::kind) says whether the
    /// connection, the TLS handshake, a timeout, the protocol or a network
    /// change failed the request, or that it was cancelled.
    ///
    /// # Panics
    ///
    /// The returned future panics when polled: the transport has not landed
    /// yet, and this signature is the contract it will meet.
    pub fn send<B>(
        &self,
        request: Request<B>,
    ) -> impl Future<Output = Result<Response<Body>, Error>> + Send + 'static
    where
        B: Into<Body>,
    {
        let request = request.map(Into::into);
        let client = self.clone();
        async move {
            let _ = (client.timeouts, request);
            // TODO(#3): send through the hyper baseline transport.
            unimplemented!("the transport lands with the baseline client")
        }
    }

    /// What the platform reports about the network the client is using now.
    ///
    /// A fact the platform does not report is `Unknown`.
    #[must_use]
    pub fn network_facts(&self) -> NetworkFacts {
        // TODO(#4): read the platform's network monitor.
        NetworkFacts::UNKNOWN
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds a [`Client`].
///
/// The only settings are the four timeouts. Each is finite: a timeout can be
/// changed but not turned off.
#[derive(Debug)]
#[must_use]
pub struct ClientBuilder {
    timeouts: Timeouts,
}

impl ClientBuilder {
    /// A builder that starts from the default timeouts.
    pub fn new() -> Self {
        Self {
            timeouts: Timeouts::default(),
        }
    }

    /// How long establishing the transport connection may take, from
    /// resolving the name to the connection being open.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.connect = Some(timeout);
        self
    }

    /// How long the TLS handshake may take once the connection is open.
    pub fn tls_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.tls = Some(timeout);
        self
    }

    /// How long a whole request may take, from sending it to the end of the
    /// response body.
    pub fn total_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.total = Some(timeout);
        self
    }

    /// How long reading the response may wait while no bytes arrive.
    pub fn read_stall_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.read_stall = Some(timeout);
        self
    }

    /// The client these settings describe.
    #[must_use]
    pub fn build(self) -> Client {
        Client {
            timeouts: self.timeouts,
            _unwind: NotUnwindSafe::default(),
        }
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// The four timeouts; `None` is the default.
// TODO(#3): replace `None` with the named default constants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Timeouts {
    connect: Option<Duration>,
    tls: Option<Duration>,
    total: Option<Duration>,
    read_stall: Option<Duration>,
}

#[cfg(test)]
mod tests {
    use super::{Client, Timeouts};
    use std::time::Duration;

    #[test]
    fn builder_records_each_timeout_and_leaves_the_rest_default() {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .read_stall_timeout(Duration::from_secs(4))
            .build();
        assert_eq!(
            client.timeouts,
            Timeouts {
                connect: Some(Duration::from_secs(1)),
                tls: None,
                total: None,
                read_stall: Some(Duration::from_secs(4)),
            }
        );
        let client = Client::builder()
            .tls_timeout(Duration::from_secs(2))
            .total_timeout(Duration::from_secs(3))
            .build();
        assert_eq!(client.timeouts.tls, Some(Duration::from_secs(2)));
        assert_eq!(client.timeouts.total, Some(Duration::from_secs(3)));
        assert_eq!(Client::new().timeouts, Timeouts::default());
    }

    #[test]
    fn client_is_send_sync_and_static() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<Client>();
    }
}
