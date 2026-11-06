//! Each hop of a redirect chain chooses its own route: the proxy, or the
//! origin directly, as the platform's configuration says for that hop's
//! host.
//!
//! The platform's configuration on Linux is the environment, read for each
//! request (docs/design/proxy.md). The test sets it on a child process,
//! this test binary run again for this one test, rather than on its own
//! process, where other tests run on other threads.

use std::process::Command;

use euripos::Client;
use euripos::http::Request;

use crate::server::{BOUND, origin, redirecting_proxy, text};

/// An origin no resolver knows (RFC 6761 §6.4): a request for it succeeds
/// only through the proxy.
const UNRESOLVABLE: &str = "http://origin.invalid:8080/start";

/// Set in the child process: the test sends its request instead of
/// starting the servers.
const CHILD: &str = "EURIPOS_TEST_REDIRECT_CHILD";

#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the environment is the proxy source on Linux only (#63)"
)]
async fn a_redirect_to_a_host_no_proxy_names_leaves_the_proxy() {
    if std::env::var_os(CHILD).is_some() {
        // The platform's proxy configuration, read for each hop.
        let client = Client::builder().total_timeout(BOUND).build();
        let response = client
            .send(Request::get(UNRESOLVABLE).body(()).expect("request"))
            .await
            .expect("a response from the redirect's target");
        assert_eq!(text(response).await, "direct");
        return;
    }
    let origin = origin().await;
    let proxy = redirecting_proxy(format!("http://{origin}/next")).await;
    let mut child = Command::new(std::env::current_exe().expect("this test binary"));
    child
        .args([
            "--exact",
            "redirect::a_redirect_to_a_host_no_proxy_names_leaves_the_proxy",
            "--include-ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("http_proxy", proxy.uri(None))
        .env("no_proxy", origin.ip().to_string())
        .env_remove("HTTP_PROXY")
        .env_remove("NO_PROXY")
        .env_remove("all_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("REQUEST_METHOD");
    // The servers run on this runtime while the child waits on them.
    let output = tokio::task::spawn_blocking(move || child.output())
        .await
        .expect("the child's wait")
        .expect("the child runs");
    assert!(
        output.status.success(),
        "the child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The child ran the test, rather than matching no test and passing.
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    // The first hop went through the proxy; the second did not.
    let seen = proxy.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, UNRESOLVABLE);
}
