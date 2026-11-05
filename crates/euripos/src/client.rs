//! The client and its builder.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use http::{Request, Response, header};

use crate::cookies::Jar;
use crate::marker::NotUnwindSafe;
use crate::monitor::NetworkMonitor;
use crate::pool::{Dst, Pool};
use crate::redirect;
use crate::timeout::{Timeouts, deadline, until};
use crate::tls::Trust;
use crate::{Body, Error, ErrorKind, NetworkFacts, TimeoutKind};

/// An HTTP client.
///
/// Build one with [`Client::new`] for the defaults or [`Client::builder`] to
/// change a setting. A `Client` is cheap to clone and is meant to be shared:
/// clones use the same connections.
#[derive(Clone)]
pub struct Client {
    timeouts: Timeouts,
    max_redirects: u32,
    /// The cookie store, when the client keeps one.
    cookies: Option<Arc<Jar>>,
    pool: Arc<Pool>,
    _unwind: NotUnwindSafe,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("timeouts", &self.timeouts)
            .field("max_redirects", &self.max_redirects)
            .field("cookie_store", &self.cookies.is_some())
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
    /// HTTP/1.1. A body that knows its media type, such as
    /// [`Body::from_json`], sets `Content-Type` unless the request already
    /// has one. Redirects are followed as
    /// [`ClientBuilder::max_redirects`] describes, and the response is the
    /// last one in the chain. The response body streams as it is read, under
    /// the same total and read-stall timeouts. The returned future owns
    /// everything it needs, so it can be spawned.
    ///
    /// # Errors
    ///
    /// Resolves to an [`Error`] whose [`kind`](Error::kind) says whether the
    /// connection, the TLS handshake, a timeout, the protocol, a network
    /// change, the request body or too many redirects failed the request, or
    /// that it was cancelled. A URI with no
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
        let mut request = request.map(Into::into);
        if let Some(content_type) = request.body_mut().take_content_type()
            && !request.headers().contains_key(header::CONTENT_TYPE)
        {
            request
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
        }
        self.clone().execute(request)
    }

    /// Sends `request`, following redirects, under one total timeout.
    async fn execute(self, request: Request<Body>) -> Result<Response<Body>, Error> {
        let total = deadline(self.timeouts.total);
        let read_stall = self.timeouts.read_stall;
        let (parts, mut body) = request.into_parts();
        let http::request::Parts {
            mut method,
            mut uri,
            version,
            mut headers,
            extensions,
            ..
        } = parts;
        let mut extensions = Some(extensions);
        let mut redirects = 0;
        loop {
            // A body is copied only while a redirect could need it again.
            let replay = if self.max_redirects > redirects {
                body.try_clone()
            } else {
                None
            };
            let mut request = Request::new(body);
            *request.method_mut() = method.clone();
            *request.uri_mut() = uri.clone();
            *request.version_mut() = version;
            *request.headers_mut() = headers.clone();
            if let Some(extensions) = extensions.take() {
                *request.extensions_mut() = extensions;
            }
            if let Some(jar) = &self.cookies
                && !headers.contains_key(header::COOKIE)
                && let Some(cookie) = jar.header(&uri)
            {
                request.headers_mut().insert(header::COOKIE, cookie);
            }
            let dst = Dst::from_uri(&uri)?;
            let (response, lease) =
                until(total, TimeoutKind::Total, self.pool.send(dst, request)).await?;
            if let Some(jar) = &self.cookies {
                jar.store(&uri, response.headers().get_all(header::SET_COOKIE));
            }
            let response =
                response.map(|incoming| Body::streaming(incoming, lease, read_stall, total));
            if self.max_redirects == 0 {
                return Ok(response);
            }
            let Some(next) = redirect::follow(
                response.status(),
                response.headers().get(header::LOCATION),
                &method,
                &uri,
            ) else {
                return Ok(response);
            };
            if redirects == self.max_redirects {
                return Err(Error::new(ErrorKind::Redirect));
            }
            body = if next.keep_body {
                // A body that cannot be sent again ends the chain here.
                let Some(replay) = replay else {
                    return Ok(response);
                };
                replay
            } else {
                Body::empty()
            };
            redirects += 1;
            redirect::strip(&mut headers, &uri, &next.uri, next.keep_body);
            method = next.method;
            uri = next.uri;
        }
    }

    /// What the platform reports about the network the client is using now.
    ///
    /// A fact the platform does not report is `Unknown`. The first call in a
    /// process starts watching the network, which may block it for up to
    /// half a second; later calls return at once.
    #[must_use]
    pub fn network_facts(&self) -> NetworkFacts {
        NetworkMonitor::global().map_or(NetworkFacts::UNKNOWN, NetworkMonitor::facts)
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds a [`Client`].
///
/// The settings are the four timeouts, how many redirects to follow, and
/// whether to keep cookies. Each timeout is finite: it can be changed but
/// not turned off. The defaults are 10 seconds to connect, 10 seconds for
/// the TLS handshake, 30 seconds for the whole request, 30 seconds for a
/// read that receives nothing, 10 redirects, and no cookie store.
#[derive(Debug)]
#[must_use]
pub struct ClientBuilder {
    timeouts: Timeouts,
    max_redirects: u32,
    cookie_store: bool,
}

impl ClientBuilder {
    /// A builder that starts from the default settings.
    pub fn new() -> Self {
        Self {
            timeouts: Timeouts::default(),
            max_redirects: redirect::DEFAULT_MAX_REDIRECTS,
            cookie_store: false,
        }
    }

    /// Whether the client keeps the cookies servers set and sends them
    /// back (RFC 6265). Defaults to `false`: no cookie is stored or sent
    /// unless the request sets `Cookie` itself.
    ///
    /// The store lives as long as the client and its clones, which share
    /// it, and is held in memory only. A request that sets its own `Cookie`
    /// header is sent with that header alone. Domains are checked as
    /// RFC 6265 §5.3 describes, but without the public suffix list: a
    /// cookie for a single-label domain such as `com` is refused, while one
    /// for a multi-label public suffix such as `co.uk` is not.
    pub fn cookie_store(mut self, enabled: bool) -> Self {
        self.cookie_store = enabled;
        self
    }

    /// How many redirects a request follows before it fails as
    /// [`ErrorKind::Redirect`](crate::ErrorKind::Redirect). Defaults to 10;
    /// `0` follows none, and returns each redirect response as it is.
    ///
    /// A `301`, `302`, `303`, `307` or `308` response with an `http` or
    /// `https` `Location` is followed (RFC 9110 §15.4). `303`, and `301` or
    /// `302` to a `POST`, are followed with a `GET` and no body; the others
    /// resend the request as it was, unless its body streams and cannot be
    /// sent again, when the redirect response is returned. When the origin
    /// changes, `Authorization`, `Proxy-Authorization`, `Cookie` and `Host`
    /// are not sent to it. The whole chain runs under one
    /// [`total_timeout`](Self::total_timeout).
    pub fn max_redirects(mut self, max: u32) -> Self {
        self.max_redirects = max;
        self
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
            max_redirects: self.max_redirects,
            cookies: self.cookie_store.then(Arc::default),
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
    fn builder_records_the_redirect_limit_and_defaults_to_ten() {
        assert_eq!(Client::new().max_redirects, 10);
        assert_eq!(Client::builder().max_redirects(0).build().max_redirects, 0);
    }

    #[test]
    fn the_cookie_store_is_off_by_default_and_shared_by_clones() {
        assert!(Client::new().cookies.is_none());
        let client = Client::builder().cookie_store(true).build();
        let clone = client.clone();
        let (Some(store), Some(cloned)) = (&client.cookies, &clone.cookies) else {
            panic!("both clients keep cookies");
        };
        assert!(std::sync::Arc::ptr_eq(store, cloned));
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
