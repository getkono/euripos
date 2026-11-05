//! The client and its builder.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use http::{Request, Response};

use crate::marker::NotUnwindSafe;
use crate::pool::{Dst, Pool};
use crate::timeout::{Timeouts, deadline, until};
use crate::tls::Trust;
use crate::{Body, Error, NetworkFacts, TimeoutKind};

/// An HTTP client.
///
/// Build one with [`Client::new`] for the defaults or [`Client::builder`] to
/// change a timeout. A `Client` is cheap to clone and is meant to be shared:
/// clones use the same connections.
#[derive(Clone)]
pub struct Client {
    timeouts: Timeouts,
    pool: Arc<Pool>,
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
    /// The request URI is absolute, `http` or `https`. An `https` request
    /// negotiates HTTP/2 or HTTP/1.1 by ALPN; an `http` request uses
    /// HTTP/1.1. The response body streams as it is read, under the same
    /// total and read-stall timeouts. The returned future owns everything it
    /// needs, so it can be spawned.
    ///
    /// # Errors
    ///
    /// Resolves to an [`Error`] whose [`kind`](Error::kind) says whether the
    /// connection, the TLS handshake, a timeout, the protocol or a network
    /// change failed the request, or that it was cancelled. A URI with no
    /// `http` or `https` origin fails as [`ErrorKind::Connect`](crate::ErrorKind::Connect).
    ///
    /// # Panics
    ///
    /// The returned future panics when polled outside a tokio runtime.
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
            let total = deadline(client.timeouts.total);
            let dst = Dst::from_uri(request.uri())?;
            let (response, lease) =
                until(total, TimeoutKind::Total, client.pool.send(dst, request)).await?;
            let read_stall = client.timeouts.read_stall;
            Ok(response.map(|incoming| Body::streaming(incoming, lease, read_stall, total)))
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
/// changed but not turned off. The defaults are 10 seconds to connect,
/// 10 seconds for the TLS handshake, 30 seconds for the whole request and
/// 30 seconds for a read that receives nothing.
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
    /// resolving the name to the connection being open. Defaults to 10
    /// seconds.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.connect = timeout;
        self
    }

    /// How long the TLS handshake may take once the connection is open.
    /// Defaults to 10 seconds.
    pub fn tls_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.tls = timeout;
        self
    }

    /// How long a whole request may take, from sending it to the end of the
    /// response body. Defaults to 30 seconds.
    pub fn total_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.total = timeout;
        self
    }

    /// How long reading the response may wait while no bytes arrive: for
    /// its head once a connection is ready, and for each later piece of its
    /// body. Defaults to 30 seconds.
    ///
    /// The wait for the head includes writing the request body, so a request
    /// whose body takes longer than this to send fails as
    /// [`TimeoutKind::ReadStall`](crate::TimeoutKind::ReadStall) even when
    /// [`total_timeout`](Self::total_timeout) allows more; a slow upload
    /// raises both.
    pub fn read_stall_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.read_stall = timeout;
        self
    }

    /// The client these settings describe.
    #[must_use]
    pub fn build(self) -> Client {
        self.build_with(Trust::platform())
    }

    /// The client these settings describe, trusting servers as `trust` says.
    fn build_with(self, trust: Trust) -> Client {
        Client {
            timeouts: self.timeouts,
            pool: Arc::new(Pool::new(self.timeouts, trust)),
            _unwind: NotUnwindSafe::default(),
        }
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Client;
    use crate::timeout::Timeouts;
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
                connect: Duration::from_secs(1),
                read_stall: Duration::from_secs(4),
                ..Timeouts::default()
            }
        );
        let client = Client::builder()
            .tls_timeout(Duration::from_secs(2))
            .total_timeout(Duration::from_secs(3))
            .build();
        assert_eq!(client.timeouts.tls, Duration::from_secs(2));
        assert_eq!(client.timeouts.total, Duration::from_secs(3));
        assert_eq!(Client::new().timeouts, Timeouts::default());
    }

    #[test]
    fn client_is_send_sync_and_static() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<Client>();
    }

    /// TLS over loopback: a server presenting a self-signed certificate for
    /// `localhost`, and clients that trust it.
    mod tls {
        use super::Client;
        use crate::tls::{Trust, with_alpn};
        use crate::{Body, Error};
        use http::{Request, Response, Version};
        use http_body::Body as _;
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use hyper_util::server::conn::auto;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use std::future::poll_fn;
        use std::net::SocketAddr;
        use std::pin::Pin;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;

        struct Server {
            address: SocketAddr,
            certificate: CertificateDer<'static>,
            accepted: Arc<AtomicUsize>,
        }

        /// A TLS server offering `alpn`, answering every request with
        /// `hello`.
        async fn server(alpn: &[&[u8]]) -> Server {
            let key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("a self-signed certificate");
            let certificate = key.cert.der().clone();
            let private =
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()));
            let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default versions")
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private)
            .expect("the key matches the certificate");
            config.alpn_protocols = alpn.iter().map(|id| id.to_vec()).collect();
            let acceptor = TlsAcceptor::from(Arc::new(config));
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("address");
            let accepted = Arc::new(AtomicUsize::new(0));
            let counter = accepted.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((tcp, _)) = listener.accept().await else {
                        return;
                    };
                    counter.fetch_add(1, Ordering::SeqCst);
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else {
                            return;
                        };
                        let service = hyper::service::service_fn(|_request| async {
                            Ok::<_, Error>(Response::new(Body::from("hello")))
                        });
                        let _ = auto::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(tls), service)
                            .await;
                    });
                }
            });
            Server {
                address,
                certificate,
                accepted,
            }
        }

        /// A client that trusts only `server`'s certificate.
        fn client(server: &Server) -> Client {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(server.certificate.clone()).expect("a valid root");
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            Client::builder()
                .total_timeout(Duration::from_secs(10))
                .build_with(Trust::Fixed(Arc::new(with_alpn(config))))
        }

        async fn text(mut body: Body) -> String {
            let mut out = Vec::new();
            while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
                if let Ok(data) = frame.expect("the body arrives").into_data() {
                    out.extend_from_slice(&data);
                }
            }
            String::from_utf8(out).expect("utf-8")
        }

        async fn get(client: &Client, server: &Server) -> (Version, String) {
            let uri = format!("https://localhost:{}/", server.address.port());
            let request = Request::get(uri).body(()).expect("request");
            let response = client.send(request).await.expect("a response");
            (response.version(), text(response.into_body()).await)
        }

        #[tokio::test]
        async fn alpn_h2_shares_one_connection_between_requests() {
            let server = server(&[b"h2", b"http/1.1"]).await;
            let client = client(&server);
            for _ in 0..3 {
                assert_eq!(
                    get(&client, &server).await,
                    (Version::HTTP_2, "hello".into())
                );
            }
            let concurrent = futures_join(&client, &server).await;
            assert!(concurrent.iter().all(|got| got.0 == Version::HTTP_2));
            assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn concurrent_first_requests_to_an_h2_origin_all_succeed() {
            let server = server(&[b"h2", b"http/1.1"]).await;
            let client = client(&server);
            let concurrent = futures_join(&client, &server).await;
            assert!(
                concurrent
                    .iter()
                    .all(|got| *got == (Version::HTTP_2, "hello".into())),
                "{concurrent:?}"
            );
        }

        #[tokio::test]
        async fn without_alpn_h2_the_client_falls_back_to_http1_and_reuses_it() {
            let server = server(&[b"http/1.1"]).await;
            let client = client(&server);
            for _ in 0..3 {
                assert_eq!(
                    get(&client, &server).await,
                    (Version::HTTP_11, "hello".into())
                );
            }
            assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
        }

        /// Four requests at once.
        async fn futures_join(client: &Client, server: &Server) -> Vec<(Version, String)> {
            let tasks: Vec<_> = (0..4)
                .map(|_| {
                    let client = client.clone();
                    let uri = format!("https://localhost:{}/", server.address.port());
                    tokio::spawn(async move {
                        let request = Request::get(uri).body(()).expect("request");
                        let response = client.send(request).await.expect("a response");
                        (response.version(), text(response.into_body()).await)
                    })
                })
                .collect();
            let mut out = Vec::new();
            for task in tasks {
                out.push(task.await.expect("the request task"));
            }
            out
        }
    }
}
